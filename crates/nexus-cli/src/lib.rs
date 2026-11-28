//! 命令行的客户端部分：解析参数、向常驻程序发起控制调用、格式化输出。
//!
//! 这里的入口函数不接触进程、终端或网络，由调用方注入控制通道，
//! 因此仿真测试可以直接调用它，行为与真实的 `nexus` 二进制一致。
#![forbid(unsafe_code)]

pub mod app;
pub mod ctl_endpoint;
pub mod daemon;
pub mod desktop;
pub mod doctor;
pub mod drive;
pub mod tun;
pub mod upgrade;
mod push;
mod unix_ctl;
pub use unix_ctl::UnixCtl;

use clap::{Parser, Subcommand};
use nexus_core_proto::roster::DeviceRole;
use nexus_core_proto::{PathKind, Timestamp};
use nexus_net_core::route;
use nexus_runtime::CtlClient;
use nexus_wire::RouteExplanation;
use nexus_wire::{
    AgentCredential, AgentInfo, AuditRecord, ClientMsg, CtlRequest, CtlResponse, ErrorCode, ExecEvent, ExecResult, ExecSpec, Input, ListEntry, SessionInfo, ShareRecord,
    ShareStatus, Size, StatusReport, WireError,
};
use serde_json::{json, Value};
use std::future::Future;
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// `--json` 输出的结构版本。结构发生不兼容变化时递增。
pub const JSON_SCHEMA: u32 = 2;

/// 服务名字的后缀：home 上的 web 是 web.home.internal。
pub const SERVICE_SUFFIX: &str = "internal";
pub const EXIT_DAEMON_UNREACHABLE: i32 = 101;
pub const EXIT_USAGE: i32 = 102;
pub const EXIT_UNKNOWN_NODE: i32 = 103;
pub const EXIT_INTERNAL: i32 = 104;
pub const EXIT_DENIED: i32 = 105;
pub const EXIT_INVALID_STATE: i32 = 106;
pub const EXIT_INVALID_INPUT: i32 = 107;
pub const EXIT_UNREACHABLE: i32 = 108;
pub const EXIT_NOT_FOUND: i32 = 109;
/// 对方（或本机）的版本太旧，不认识这个请求或回应（M7.1）。
pub const EXIT_VERSION: i32 = 110;
/// `nexus exec` 的命令因超时被终止（与 timeout(1) 一致）。
pub const EXIT_TIMED_OUT: i32 = 124;
/// `nexus exec` 的命令因输出超过上限被终止。
pub const EXIT_TRUNCATED: i32 = 125;
/// `nexus exec` 的命令无法启动（与 shell 一致）。
pub const EXIT_SPAWN_FAILED: i32 = 127;

/// `nexus exec` 默认的输出上限。
pub const DEFAULT_MAX_OUTPUT: u64 = 64 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(name = "nexus", version, about = "把你的所有设备组成一个整体")]
pub struct Cli {
    /// 本节点的数据目录（默认 $NEXUS_DATA_DIR 或 ~/.local/share/nexus）
    #[arg(long, global = true, value_name = "目录")]
    pub data_dir: Option<PathBuf>,
    /// 以 agent 的身份执行：凭据文本（nxa1- 开头）或凭据文件的路径。也可以用环境变量 NEXUS_AGENT 给出。
    /// agent 只能看到和做凭据里授权的事，不能做管理操作
    #[arg(long, global = true, env = "NEXUS_AGENT", value_name = "凭据或文件", hide_env_values = true)]
    pub agent: Option<String>,
    #[command(subcommand)]
    pub cmd: Cmd,
}

// 命令行只解析一次，变体大小不重要。
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// 运行常驻程序
    Daemon {
        /// 本节点的名字（默认取主机名）
        #[arg(long)]
        name: Option<String>,
        /// 只在本机回环地址上通信，不使用中继和地址发现（用于测试）
        #[arg(long)]
        local_only: bool,
        /// 只检查能不能读数据目录，然后退出（升级时检查新版本用）
        #[arg(long, hide = true)]
        preflight: bool,
        /// 中继：default（公共中继）、disabled，或自建中继的地址
        #[arg(long, default_value = "default")]
        relay: String,
        /// 只经中继通信，不打开 UDP 直连套接字（适用于禁用 UDP 的沙箱或受限网络）
        #[arg(long)]
        relay_only: bool,
        /// 不使用公共地址发现服务
        #[arg(long)]
        no_discovery: bool,
        /// 碰头服务（短加入码用）：default（公共的地址发现服务）、disabled，或自建的地址，例如锚点的
        /// https://<锚点>/pkarr。默认：用了地址发现时是 default，否则 disabled
        #[arg(long, value_name = "地址")]
        rendezvous: Option<String>,
        /// 关掉流量调度（只用于对比测量）
        #[arg(long, hide = true)]
        no_scheduling: bool,
        /// agent 入口（只接受 agent 凭据的本机套接字）的位置，默认在数据目录里。
        /// 要让以其他系统账号运行的 agent 使用 Nexus，把它放在那个账号能进入的目录，
        /// 并让 agent 设置同样的 NEXUS_AGENT_SOCKET
        #[arg(long, env = "NEXUS_AGENT_SOCKET", value_name = "路径")]
        agent_socket: Option<PathBuf>,
        /// 本机统一入口（SOCKS5 和 HTTP 代理同一个端口）的监听地址，例如 127.0.0.1:1080；只能是本机地址
        #[arg(long, value_name = "地址:端口")]
        proxy: Option<String>,
        /// 成为完整节点：建这个名字的虚拟网卡，系统里的程序不设代理按名字访问服务（需要管理员权限）
        #[arg(long, value_name = "网卡名")]
        tun: Option<String>,
        /// 完整节点的网段：本机临时地址从这里分配，第三个地址是名字服务
        #[arg(long, value_name = "地址/前缀", requires = "tun", default_value = "198.18.0.0/16")]
        tun_net: String,
        #[command(flatten)]
        anchor: AnchorArgs,
    },
    /// 在第一台设备上建立你的网络
    Init {
        /// 这台设备在网络中的名字
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 管理设备丢了：在网络里的另一台设备上输入建网时的恢复码，让这台设备成为管理设备
    Recover {
        /// `nexus init` 输出的恢复码（空格和 - 忽略，不区分大小写）
        code: String,
        #[arg(long)]
        json: bool,
    },
    /// 让这台设备准备加入网络：输出短加入码（例如 7KQ4M-9XZ21）和长加入码，10 分钟内有效，
    /// 在管理设备上用 `nexus device approve` 批准。短码要经碰头服务（见 daemon --rendezvous），长码不需要
    Join {
        #[arg(long)]
        json: bool,
    },
    /// 管理名册里的设备（只能在管理设备上执行）
    Device {
        #[command(subcommand)]
        cmd: DeviceCmd,
    },
    /// 列出你的所有设备及其状态；给出设备名时，列出那台设备上你能用的服务
    ///
    /// 例：nexus ls home --json
    Ls {
        /// 设备名：列出它发布的服务
        node: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 把本机的一个服务发布给网络里的其他设备（只监听本机的服务也可以）
    ///
    /// 例：nexus publish web --port 8080 --proto http --env OPENAI_BASE_URL=http://{addr}/v1
    ///
    /// 你自己的设备默认可以使用；agent 和网页登录需要授权 svc.connect@<设备>/svc/<名字>。
    /// 使用方：nexus connect <设备>/<名字>。
    Publish {
        /// 服务的名字（字母、数字、- 和 _）
        name: String,
        /// 服务在本机监听的端口
        #[arg(long)]
        port: u16,
        /// 服务在本机监听的地址
        #[arg(long, default_value = "127.0.0.1")]
        host: std::net::IpAddr,
        /// 协议，告诉使用方怎么用：tcp、http，udp（经完整节点按名字访问，M11.5），远程桌面是 vnc 或 rdp
        #[arg(long, default_value = "tcp")]
        proto: String,
        /// 使用方需要的环境变量，值里的 {addr} 换成使用方访问服务的地址；可以写多次
        #[arg(long, value_name = "名字=值")]
        env: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// 撤销一个服务的发布
    Unpublish {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// 连到一台设备上发布的服务：标准输入输出接到服务上，或者在本机开一个端口转发过去
    ///
    /// 例：nexus connect home/web --listen 127.0.0.1:8080
    ///
    /// 不需要任何密钥：目标设备按你的身份判定。
    Connect {
        /// <设备>/<服务>
        target: String,
        /// 在本机这个地址上监听，每个连接各自接到服务
        #[arg(long, value_name = "地址:端口")]
        listen: Option<String>,
    },
    /// 在本机浏览器里打开一个设备发布的网页服务，直到按 Ctrl-C
    ///
    /// 在 127.0.0.1 上开一个端口接到服务，用 $BROWSER（没有时用系统默认的浏览器）打开。
    /// 例：nexus open home/web、nexus open home/web/admin?tab=2
    Open {
        /// <设备>/<服务>，后面可以接路径
        target: String,
        /// 本机端口；默认挑一个空闲的
        #[arg(long)]
        port: Option<u16>,
        /// 不打开浏览器，只显示网址
        #[arg(long)]
        no_browser: bool,
    },
    /// 远程桌面：把本机的 VNC / RDP 发布出去，或者用本机的查看器打开另一台设备的桌面
    ///
    /// 例：在 home 上 nexus desktop share；在别的设备上 nexus desktop open home
    Desktop {
        #[command(subcommand)]
        cmd: desktop::DesktopCmd,
    },
    /// 文件：工作区（版本化的内容树）。路径写作 <工作区>:/<路径>，例如 work:/src/main.rs
    Drive {
        #[command(subcommand)]
        cmd: drive::DriveCmd,
    },
    /// 本机作为出口：开放后，名册里的设备可以经本机访问外网（路由规则 --via 本机）
    ///
    /// agent 和网页登录需要授权 net.exit@<本机>/exit。出口不替别人连本机地址。
    Exit {
        #[command(subcommand)]
        cmd: ExitCmd,
    },
    /// 统一入口的路由规则：名字以 .internal 结尾的去网络里的服务，其余按规则，第一条匹配的生效，都不匹配时直连
    Route {
        #[command(subcommand)]
        cmd: RouteCmd,
    },
    /// 在本机运行一个程序，它用到的服务先在本机开好端口，并按服务的说明设置环境变量
    ///
    /// 例：nexus run --use home/llm -- python app.py
    ///
    /// 退出码与程序相同；用到的服务不存在或没有授权时程序不启动，退出码同 nexus connect。
    Run {
        /// 用到的服务 <设备>/<服务>，可以写多次
        #[arg(long = "use", value_name = "设备/服务", required = true)]
        uses: Vec<String>,
        /// 要运行的程序及其参数
        #[arg(last = true, required = true, value_name = "命令")]
        command: Vec<String>,
    },
    /// 在一台设备上执行命令，返回它的输出和退出码
    ///
    /// 命令不经过 shell。本机的标准输入会转发给远端命令。
    /// 退出码与远端命令相同；超时为 124，输出超限为 125，无法启动为 127。
    Exec {
        /// 节点的名字或标识
        node: String,
        /// 超过这么多秒就终止命令
        #[arg(long, value_name = "秒")]
        timeout: Option<f64>,
        /// 输出（标准输出与标准错误合计）的字节数上限，超过就终止命令
        #[arg(long, value_name = "字节", default_value_t = DEFAULT_MAX_OUTPUT)]
        max_output: u64,
        /// 工作目录（默认是远端用户的主目录）
        #[arg(long, value_name = "目录")]
        cwd: Option<String>,
        /// 追加环境变量，可重复
        #[arg(long = "env", value_name = "名字=值")]
        env: Vec<String>,
        /// 结束后输出一份 JSON，包含完整的输出、退出码和是否超时、是否截断
        #[arg(long)]
        json: bool,
        /// 要执行的程序及其参数
        #[arg(last = true, required = true, value_name = "命令")]
        command: Vec<String>,
    },
    /// 进入一台设备上的常驻终端会话（按 Ctrl-] 断开，会话继续运行）
    ///
    /// 会话不存在时自动创建。断开后重新连上，看到的屏幕和没断开时一样。
    /// 网络中断时自动重连。会话里的 shell 退出时，以它的退出码退出。
    Ssh {
        /// 节点的名字或标识
        node: String,
        /// 会话的名字
        #[arg(long, default_value = "main")]
        session: String,
        /// 标准输入不是终端时使用的终端尺寸，如 80x24
        #[arg(long, value_name = "列x行")]
        size: Option<String>,
    },
    /// 查看和结束终端会话
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// 签发、撤销和列出 agent 凭据（只能在管理设备上）
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    /// 查看本机的审计记录：经过本机的每次访问，包括被拒绝的
    Audit {
        /// 只显示最近的这么多条
        #[arg(long, value_name = "条数")]
        limit: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// 以 MCP 服务器的方式运行（标准输入输出），供 agent 框架调用
    ///
    /// 提供设备/服务发现、命令执行、终端会话、工作区读写与提交、路径/路由诊断和审计查询工具。
    /// 带 --agent 或 NEXUS_AGENT 时，所有工具都以该 agent 的身份执行。
    Mcp,
    /// 访问网页节点发布的硬件能力（摄像头、麦克风、位置，M8.3）
    Hw {
        #[command(subcommand)]
        cmd: HwCmd,
    },
    /// 临时分享（M9.1）：把文件/目录、网页服务或只读终端用限时/限次链接分享给访客
    Share {
        #[command(subcommand)]
        cmd: ShareCmd,
    },
    /// Vault（M9.2）：加密保存秘密并按条目同步到授权设备，每次取用记入审计
    Vault {
        #[command(subcommand)]
        cmd: VaultCmd,
    },
    /// 在管理设备上轮换网络的根密钥并生成新恢复码（无需重建网络，旧恢复码与旧根签发的 agent 立即失效）
    RotateRoot {
        #[arg(long)]
        json: bool,
    },
    /// 登记、列出、撤销用于网页登录的通行密钥
    Web {
        #[command(subcommand)]
        cmd: WebCmd,
    },
    /// 网络的锚点：常开、有公网地址的设备，提供中继和网页节点的页面
    Anchor {
        #[command(subcommand)]
        cmd: AnchorCmd,
    },
    /// 输出一段 ~/.ssh/config 配置，让系统自带的 ssh、scp、git 能用 <设备名>.nexus 访问设备
    ///
    /// 用法：nexus ssh-config >> ~/.ssh/config，之后 ssh home.nexus、scp f home.nexus:/tmp/、
    /// git clone home.nexus:/path/repo 都能用，不需要任何密钥。交互式登录进入常驻会话 main。
    SshConfig,
    /// 终端会话的持有进程（M7.2），由常驻程序启动，不直接运行
    #[command(hide = true)]
    SessionHold {
        #[arg(long)]
        name: String,
        #[arg(long)]
        cols: u16,
        #[arg(long)]
        rows: u16,
        #[arg(long)]
        created_at_ms: u64,
        /// 按上次记下的状态恢复（设备重启或持有进程意外结束之后，M11.5）
        #[arg(long)]
        restore: bool,
    },
    /// 供 ssh 的 ProxyCommand 调用：在标准输入输出上充当 SSH 服务端（一般不直接运行）
    SshProxy {
        /// ssh 传入的主机名，形如 home.nexus
        host: String,
    },
    /// 查看本机或另一个节点的状态
    Status {
        /// 节点的名字或标识；省略时为本机
        node: Option<String>,
        /// 以 JSON 输出
        #[arg(long)]
        json: bool,
    },
    /// 一键体检：汇总本机常驻程序、设备在线状态、连接路径、版本兼容性与名字解析，并给出下一步建议
    Doctor {
        /// 以 JSON 输出
        #[arg(long)]
        json: bool,
    },
    /// 本机的网络接口，以及哪些按流量计费（蜂窝网络、手机热点）
    ///
    /// 计费的接口只承担交互流量（按键、终端输出、请求）和故障接管，不承担批量传输。
    /// 默认自动识别；可以按接口手动设置。例：nexus iface wlan0 --metered on
    Iface {
        /// 接口名；省略时列出所有接口
        name: Option<String>,
        /// 设置这个接口是否计费：on、off，或 auto（按自动识别）
        #[arg(long, value_parser = ["on", "off", "auto"], requires = "name")]
        metered: Option<String>,
        /// 以 JSON 输出
        #[arg(long)]
        json: bool,
    },
    /// 到其他设备的路径：经过本机哪个网络地址、直连还是中继、往返时间、收发字节数
    ///
    /// 有多个网络接口（例如有线和 5G）时，每个接口一条路径，同时使用；一条断了数据换到别的路径上。
    Path {
        /// 设备的名字或标识；省略时列出所有设备
        device: Option<String>,
        /// 以 JSON 输出
        #[arg(long)]
        json: bool,
    },
    /// 把本机的常驻程序换成新版本，并重新启动（终端会话不断）
    ///
    /// 先检查新版本能运行、协议主版本相同、能读数据目录；换上之后要在 --confirm-within 内连上升级前在线的设备，
    /// 连不上就自动换回原来的版本。原来的二进制留作 <二进制>.prev。
    Upgrade {
        /// 新版本：发布的 .tar.gz（旁边有 SHA256SUMS 时校验），或者 nexus 二进制
        #[arg(long, value_name = "文件", required_unless_present_any = ["status", "rollback"])]
        file: Option<String>,
        /// 新版本要在这段时间内连上升级前在线的设备，例如 60s、5m
        #[arg(long, default_value = "60s", value_name = "时长")]
        confirm_within: String,
        /// 协议主版本不同也换（换上之后连不上没升级的设备）
        #[arg(long)]
        force: bool,
        /// 不升级本机，经 Nexus 把新版本送到这些设备并升级它们，一台一台地（逗号分隔或重复）
        #[arg(long, value_name = "设备", value_delimiter = ',', conflicts_with = "all")]
        to: Vec<String>,
        /// 不升级本机，升级其他所有在线的设备（按名字顺序，一台换回了就停）
        #[arg(long)]
        all: bool,
        /// 查看最近一次升级的状态
        #[arg(long, conflicts_with_all = ["file", "rollback", "to", "all"])]
        status: bool,
        /// 换回上一个版本并重新启动
        #[arg(long, conflicts_with_all = ["file", "status", "to", "all"])]
        rollback: bool,
        #[arg(long)]
        json: bool,
    },
    /// 协议版本（主 次），升级时检查新版本用
    #[command(hide = true)]
    ProtocolVersion,
}

#[derive(Subcommand, Debug)]
pub enum DeviceCmd {
    /// 批准一台设备加入
    Approve {
        /// 新设备上 `nexus join` 输出的短加入码或长加入码（短码不区分大小写，分隔符可有可无）
        code: String,
        /// 给它起的名字（默认用它自己建议的名字）
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 把一台设备移出名册，它将无法再访问其他设备
    Revoke {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SessionCmd {
    /// 列出一台设备上的终端会话
    Ls {
        node: String,
        #[arg(long)]
        json: bool,
    },
    /// 结束一个终端会话，连着它的客户端随之退出
    Kill {
        node: String,
        session: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum HwCmd {
    /// 列出网页节点当前已显式同意开放的硬件能力
    Ls {
        /// 网页节点的标识（64 位十六进制）
        node_id: String,
        /// 网页节点的地址提示
        #[arg(long, default_value = "")]
        hint: String,
        #[arg(long)]
        json: bool,
    },
    /// 从网页节点采集一项硬件能力（camera、microphone、location）
    Capture {
        /// 网页节点的标识（64 位十六进制）
        node_id: String,
        /// 硬件能力：camera、microphone 或 location
        kind: String,
        /// 网页节点的地址提示
        #[arg(long, default_value = "")]
        hint: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ShareCmd {
    /// 创建一条只读临时分享链接（文件/目录、已发布网页服务或只读终端）
    Create {
        /// 分享工作区里的文件或目录，格式 <工作区>:/<路径>
        #[arg(long, value_name = "工作区:/路径")]
        drive: Option<String>,
        /// 分享本机已发布的网页服务（--proto http）
        #[arg(long, value_name = "服务名")]
        service: Option<String>,
        /// 分享本机正在运行的终端会话（访客只读观看）
        #[arg(long, value_name = "会话名")]
        terminal: Option<String>,
        /// 有效期，例如 30m、1h、24h
        #[arg(long, default_value = "1h")]
        ttl: String,
        /// 最多允许打开的次数（不设则不限次数）
        #[arg(long, value_name = "次数")]
        max_uses: Option<u32>,
        /// 网页入口地址（默认用网络的锚点）
        #[arg(long, value_name = "网址")]
        page: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 列出本机的所有临时分享及其状态（有效/已过期/已用尽/已撤销）
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// 立即撤销一条临时分享
    Revoke {
        /// 分享编号（8 位十六进制）
        id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum VaultCmd {
    /// 写入或轮换一条秘密（加密后同步给 --device 指定的授权设备）
    Put {
        /// 条目名（字母、数字、- 和 _）
        name: String,
        /// 秘密内容（不给则从标准输入读取）
        #[arg(long)]
        value: Option<String>,
        /// 允许持有并读取该条目的设备名（可写多次；管理设备默认包含）
        #[arg(long = "device", value_name = "设备名")]
        devices: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// 读取并解密一条秘密（每次取用都会记入审计）
    Get {
        /// 条目名
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// 列出本机可见的 Vault 条目
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// 删除一条秘密并通知各设备擦除
    Rm {
        /// 条目名
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// 从管理设备主动同步本机被授权的条目
    Sync {
        #[arg(long)]
        json: bool,
    },
    /// 列出因设备被吊销而需要轮换的条目
    RotateNeeded {
        /// 只看某台已吊销设备
        #[arg(long)]
        device: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum WebCmd {
    /// 发起登记（只能在管理设备上）：输出一个链接，在要用的设备上打开它，创建通行密钥后即登记完成
    ///
    /// 例：nexus web enroll --label phone
    ///
    /// 默认权限是所有设备的终端会话和状态查看（term.session.*@**、node.info@**），
    /// 不能执行命令，也不能做管理操作；用 --grant 改为别的权限，格式同 nexus agent issue。
    Enroll {
        /// 通行密钥在网络里的名字，例如 phone
        #[arg(long)]
        label: String,
        /// 授权条目，格式 能力@资源；可重复。不给时使用默认权限
        #[arg(long = "grant", value_name = "能力@资源")]
        grants: Vec<String>,
        /// 登记链接的有效期
        #[arg(long, value_name = "时长", default_value = "10m")]
        ttl: String,
        /// 网页节点的页面地址；通行密钥绑定在它的域名上。不给时用网络的锚点（nexus anchor set），
        /// 没有锚点时用 https://bewith.link
        #[arg(long, value_name = "地址")]
        page: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 列出登记的通行密钥（任何设备上都可以查看）
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// 撤销一个通行密钥（只能在管理设备上）：之后用它不能再登录
    Revoke {
        #[arg(value_name = "名字")]
        label: String,
        #[arg(long)]
        json: bool,
    },
    /// 在本机托管网页节点的页面（开发和自用），按 Ctrl-C 停止
    ///
    /// 例：nexus web serve --dir ./web/dist，然后 nexus web enroll --label dev --page http://localhost:8080
    ///
    /// 只监听本机地址：通行密钥要求页面是 https，只有 localhost 例外。
    Serve {
        /// 页面所在的目录（包含 index.html）
        #[arg(long, value_name = "目录")]
        dir: PathBuf,
        /// 监听的地址，只能是本机地址
        #[arg(long, value_name = "地址:端口", default_value = "127.0.0.1:8080")]
        listen: String,
    },
}

/// 让这台设备同时做网络的锚点：内置中继、托管网页节点的页面、自动管理证书。
#[derive(clap::Args, Debug, Clone, Default)]
pub struct AnchorArgs {
    /// 作为锚点运行，域名指向这台机器，例如 --anchor bewith.link：在 443 端口提供页面和中继，
    /// 自动向 Let's Encrypt 申请证书（需要 443 端口可从公网访问），80 端口重定向到 https
    #[arg(long, value_name = "域名", conflicts_with = "anchor_dev")]
    pub anchor: Option<String>,
    /// 开发用的锚点：只在本机地址上以 http 提供页面和中继，例如 --anchor-dev 127.0.0.1:3340；
    /// 锚点地址是 http://localhost:端口
    #[arg(long, value_name = "地址:端口")]
    pub anchor_dev: Option<String>,
    /// 网页节点的页面目录（包含 index.html，见 web/README.md）
    #[arg(long, value_name = "目录")]
    pub page_dir: Option<PathBuf>,
    /// 使用这份证书（PEM，含证书链），不自动申请
    #[arg(long, value_name = "文件", requires_all = ["tls_key", "anchor"])]
    pub tls_cert: Option<PathBuf>,
    /// 证书的私钥（PEM）
    #[arg(long, value_name = "文件", requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,
    /// 申请证书时留给 Let's Encrypt 的联系邮箱（可选）
    #[arg(long, value_name = "邮箱")]
    pub acme_contact: Option<String>,
    /// 使用 Let's Encrypt 的测试环境（证书不受浏览器信任，用于先试一遍流程）
    #[arg(long)]
    pub acme_staging: bool,
    /// https 监听地址
    #[arg(long, value_name = "地址:端口", default_value = "0.0.0.0:443")]
    pub https_listen: String,
    /// http 监听地址（重定向到 https）
    #[arg(long, value_name = "地址:端口", default_value = "0.0.0.0:80")]
    pub http_listen: String,
}

impl AnchorArgs {
    pub fn enabled(&self) -> bool {
        self.anchor.is_some() || self.anchor_dev.is_some()
    }
}

#[derive(Subcommand, Debug)]
pub enum AnchorCmd {
    /// 设置网络的锚点（只能在管理设备上）：各设备把它加为中继，登记链接默认指向它上面的页面
    ///
    /// 例：nexus anchor set https://bewith.link
    ///
    /// 锚点是一台运行 nexus daemon --anchor 的常开设备。
    Set {
        /// 锚点地址，https://主机[:端口]；本机开发可以用 http://localhost 或 http://127.0.0.1
        #[arg(value_name = "地址", required_unless_present = "clear")]
        url: Option<String>,
        /// 清除锚点
        #[arg(long, conflicts_with = "url")]
        clear: bool,
        #[arg(long)]
        json: bool,
    },
    /// 查看网络的锚点
    Show {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ExitCmd {
    /// 开放出口
    On {
        #[arg(long)]
        json: bool,
    },
    /// 关闭出口
    Off {
        #[arg(long)]
        json: bool,
    },
    /// 出口是否开放
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum RouteCmd {
    /// 加一条规则（加在最后）
    ///
    /// 例：nexus route add --domain example.com --reject
    ///     nexus route add --domain example.com --via anchor
    ///     nexus route add --cidr 10.0.0.0/8 --socks5 127.0.0.1:7890
    #[command(group = clap::ArgGroup::new("outbound").required(true))]
    Add {
        /// 匹配这个域名和它的子域名
        #[arg(long, value_name = "域名", conflicts_with_all = ["cidr", "domain_exact"])]
        domain: Option<String>,
        /// 仅精确匹配这个域名本身（不含子域名）
        #[arg(long, value_name = "域名", conflicts_with_all = ["cidr", "domain"])]
        domain_exact: Option<String>,
        /// 匹配这个地址段，例如 10.0.0.0/8
        #[arg(long, value_name = "地址/前缀")]
        cidr: Option<String>,
        /// 拒绝
        #[arg(long, group = "outbound")]
        reject: bool,
        /// 直连
        #[arg(long, group = "outbound")]
        direct: bool,
        /// 经网络里的这台设备出去（它要先 nexus exit on）
        #[arg(long, value_name = "设备", group = "outbound")]
        via: Option<String>,
        /// 交给这个上游 SOCKS5 代理，例如 127.0.0.1:7890
        #[arg(long, value_name = "主机:端口", group = "outbound")]
        socks5: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 删除一条规则
    Rm {
        /// 规则的编号（见 nexus route ls）
        index: u32,
        #[arg(long)]
        json: bool,
    },
    /// 列出规则
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// 说明一个目标会走哪里
    ///
    /// 例：nexus route explain web.home.internal:80
    Explain {
        /// 主机:端口
        target: String,
        #[arg(long)]
        json: bool,
    },
    /// 从 Clash / Mihomo YAML 规则子集导入并输出审计报告（N1）
    Import {
        /// 规则文件路径
        file: std::path::PathBuf,
        /// 代理组默认映射到的 Nexus 出口设备
        #[arg(long, value_name = "设备")]
        via: Option<String>,
        /// 代理组默认映射到的上游 SOCKS5 地址（主机:端口）
        #[arg(long, value_name = "主机:端口")]
        socks5: Option<String>,
        /// 替换现有规则（默认追加）；替换前自动保存 LKG 快照
        #[arg(long)]
        replace: bool,
        /// 仅解析并输出审计报告，不下发给常驻程序
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// 回滚路由规则到上一份已验证的 LKG 快照（N1）
    Rollback {
        #[arg(long)]
        json: bool,
    },
    /// 对当前路由规则执行静态冲突/遮蔽/无效上游审计（N1）
    Audit {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum AgentCmd {
    /// 签发一份 agent 凭据
    ///
    /// 例：nexus agent issue --label builder --grant term.exec@home --grant 'term.session.*@home/session/build' --ttl 4h
    ///
    /// 能力和资源：node.info@设备；term.exec@设备；term.session.attach、term.session.list、
    /// term.session.kill@设备/session/会话名；svc.connect@设备/svc/服务名；net.exit@设备/exit；
    /// drive.read、drive.write@设备/drive/工作区。末尾的星号匹配一组能力（term.session.*）；
    /// 资源的一段写成星号时匹配任意一段（drive.read@*/drive/work），末尾的两个星号匹配其下的一切。
    ///
    /// 输出的凭据含 agent 的私钥，请像密码一样保管；agent 用 --agent 或 NEXUS_AGENT 使用它。
    Issue {
        /// agent 的名字
        #[arg(long)]
        label: String,
        /// 授权条目，格式 能力@资源，例如 term.exec@home；可重复
        #[arg(long = "grant", value_name = "能力@资源", required = true)]
        grants: Vec<String>,
        /// 有效期，例如 30m、4h、7d
        #[arg(long, value_name = "时长")]
        ttl: String,
        /// 把凭据写进这个文件（权限 600），而不是打印出来
        #[arg(long, value_name = "文件")]
        out: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// 撤销一个 agent（按名字或标识），新名册推送到各设备后它的请求一律被拒绝
    Revoke {
        /// agent 的名字或标识
        #[arg(value_name = "AGENT")]
        who: String,
        #[arg(long)]
        json: bool,
    },
    /// 列出签发过的 agent
    Ls {
        #[arg(long)]
        json: bool,
    },
}

/// 读取凭据：凭据文本本身，或凭据文件的路径。
pub fn load_agent(text: &str) -> Result<AgentCredential, String> {
    let content = if AgentCredential::looks_like(text) {
        text.to_string()
    } else {
        std::fs::read_to_string(text).map_err(|e| format!("读不了凭据文件 {text}：{e}"))?
    };
    AgentCredential::decode(&content).ok_or_else(|| "凭据格式不对：应当是 nexus agent issue 输出的 nxa1- 开头的文本".to_string())
}

/// 解析 `30s`、`15m`、`4h`、`7d` 形式的时长，返回毫秒。
pub fn parse_ttl(text: &str) -> Option<u64> {
    let text = text.trim();
    let (num, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = num.parse().ok()?;
    let ms = match unit {
        "s" => 1000,
        "m" => 60 * 1000,
        "h" => 3600 * 1000,
        "d" => 24 * 3600 * 1000,
        _ => return None,
    };
    n.checked_mul(ms).filter(|&v| v > 0)
}

/// 到本机常驻程序的控制通道。每次调用打开一个会话。
pub trait Ctl {
    fn open(&self, req: CtlRequest) -> impl Future<Output = io::Result<CtlClient>>;
}

/// 命令的输出去处。真实的命令行写到终端，测试里收集起来。
pub trait Sink {
    fn out(&mut self, bytes: &[u8]);
    fn err(&mut self, bytes: &[u8]);
}

/// 把输出收集到内存里的 [`Sink`]。
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Sink for Captured {
    fn out(&mut self, bytes: &[u8]) {
        self.stdout.extend_from_slice(bytes);
    }
    fn err(&mut self, bytes: &[u8]) {
        self.stderr.extend_from_slice(bytes);
    }
}

/// 一条命令的完整结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    /// 标准输出（不是合法 UTF-8 的部分被替换）。
    pub stdout: String,
    pub stderr: String,
    /// 标准输出的原始字节。
    pub stdout_raw: Vec<u8>,
}

impl Output {
    pub fn from_captured(code: i32, c: Captured) -> Self {
        Self {
            code,
            stdout: String::from_utf8_lossy(&c.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&c.stderr).into_owned(),
            stdout_raw: c.stdout,
        }
    }
}

#[doc(hidden)]
pub fn fail<K: Sink>(sink: &mut K, code: i32, kind: &str, message: &str) -> i32 {
    let line = json!({ "error": kind, "message": message });
    sink.err(format!("{line}\n").as_bytes());
    code
}

#[doc(hidden)]
pub fn fail_wire<K: Sink>(sink: &mut K, e: &nexus_wire::WireError) -> i32 {
    let (code, kind) = exit_for(e.code);
    fail(sink, code, kind, &e.message)
}

/// 错误码对应的退出码和错误种类。
fn exit_for(code: ErrorCode) -> (i32, &'static str) {
    match code {
        ErrorCode::UnknownNode => (EXIT_UNKNOWN_NODE, "unknown_node"),
        ErrorCode::Denied => (EXIT_DENIED, "denied"),
        ErrorCode::InvalidState => (EXIT_INVALID_STATE, "invalid_state"),
        ErrorCode::InvalidInput => (EXIT_INVALID_INPUT, "invalid_input"),
        ErrorCode::Unreachable => (EXIT_UNREACHABLE, "unreachable"),
        ErrorCode::SpawnFailed => (EXIT_SPAWN_FAILED, "spawn_failed"),
        ErrorCode::NotFound => (EXIT_NOT_FOUND, "not_found"),
        ErrorCode::UnsupportedVersion => (EXIT_VERSION, "version"),
        _ => (EXIT_INTERNAL, "internal"),
    }
}

/// 解析参数。帮助和版本信息写到 `sink` 后以 `Err(0)` 返回，用法错误以 `Err(EXIT_USAGE)` 返回。
pub fn parse<I, A, K>(args: I, sink: &mut K) -> Result<Cli, i32>
where
    I: IntoIterator<Item = A>,
    A: Into<std::ffi::OsString> + Clone,
    K: Sink,
{
    let argv = std::iter::once("nexus".into()).chain(args.into_iter().map(Into::into));
    Cli::try_parse_from(argv).map_err(|e| {
        let text = e.render().to_string();
        if e.use_stderr() {
            sink.err(text.as_bytes());
            EXIT_USAGE
        } else {
            sink.out(text.as_bytes());
            0
        }
    })
}

const DAEMON_HINT: &str = "，请先运行 nexus daemon";

pub async fn call<C: Ctl>(ctl: &C, req: CtlRequest) -> io::Result<CtlResponse> {
    let mut session = ctl.open(req).await?;
    session
        .rx
        .recv()
        .await
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "常驻程序没有回应"))
}

/// 执行一条客户端命令，返回退出码。`stdin` 只有 `exec` 会读取。
pub async fn run_client<C: Ctl, R: AsyncRead + Unpin, K: Sink>(cmd: &Cmd, ctl: &C, stdin: &mut R, sink: &mut K) -> i32 {
    let (req, json) = match cmd {
        Cmd::Daemon { .. } => return fail(sink, EXIT_USAGE, "usage", "daemon 不是客户端命令"),
        Cmd::Exec { node, timeout, max_output, cwd, env, json, command } => {
            let mut pairs = Vec::new();
            for kv in env {
                match kv.split_once('=') {
                    Some((k, v)) if !k.is_empty() => pairs.push((k.to_string(), v.to_string())),
                    _ => return fail(sink, EXIT_USAGE, "usage", &format!("--env 的格式应为 名字=值：{kv}")),
                }
            }
            if timeout.is_some_and(|t| !(t > 0.0 && t.is_finite())) {
                return fail(sink, EXIT_USAGE, "usage", "--timeout 必须是正数");
            }
            let spec = ExecSpec {
                program: command[0].clone(),
                args: command[1..].to_vec(),
                cwd: cwd.clone(),
                env: pairs,
                timeout_ms: timeout.map(|t| (t * 1000.0).ceil() as u64),
                max_output: *max_output,
            };
            return exec(ctl, CtlRequest::Exec { node: node.clone(), spec }, *json, stdin, sink).await;
        }
        Cmd::Status { node: None, json } => (CtlRequest::Status, *json),
        Cmd::Doctor { json } => return doctor::run(ctl, *json, sink).await,
        Cmd::Path { device, json } => (CtlRequest::Paths { device: device.clone() }, *json),
        Cmd::Upgrade { status: true, json, .. } => (CtlRequest::UpgradeStatus, *json),
        Cmd::Upgrade { rollback: true, json, .. } => (CtlRequest::UpgradeRollback, *json),
        Cmd::Upgrade { file, confirm_within, force, json, to, all, .. } => {
            let Some(confirm_ms) = parse_ttl(confirm_within) else {
                return fail(sink, EXIT_USAGE, "usage", &format!("--confirm-within 的格式不对：{confirm_within}（例如 60s、5m）"));
            };
            if *all || !to.is_empty() {
                let file = std::path::PathBuf::from(file.clone().unwrap_or_default());
                let opts = push::Options { file, confirm_ms, force: *force, to: to.clone(), all: *all, json: *json };
                return push::run(ctl, &opts, sink).await;
            }
            let file = file.clone().unwrap_or_default();
            // 常驻程序的工作目录不一定和这里一样：给它绝对路径。
            let file = std::path::absolute(&file).map(|p| p.display().to_string()).unwrap_or(file);
            (CtlRequest::Upgrade { file, confirm_ms, force: *force }, *json)
        }
        Cmd::Iface { name, metered, json } => {
            let set = match (name, metered.as_deref()) {
                (Some(n), Some(m)) => Some((n.clone(), match m {
                    "on" => Some(true),
                    "off" => Some(false),
                    _ => None,
                })),
                _ => None,
            };
            (CtlRequest::Interfaces { set }, *json)
        }
        Cmd::Status { node: Some(n), json } => (CtlRequest::RemoteStatus { node: n.clone() }, *json),
        Cmd::Init { name, json } => (CtlRequest::Init { name: name.clone() }, *json),
        Cmd::Recover { code, json } => (CtlRequest::Recover { code: code.clone() }, *json),
        Cmd::RotateRoot { json } => (CtlRequest::RootRotate, *json),
        Cmd::Join { json } => (CtlRequest::JoinStart, *json),
        Cmd::Device { cmd: DeviceCmd::Approve { code, name, json } } => {
            (CtlRequest::Approve { code: code.clone(), name: name.clone() }, *json)
        }
        Cmd::Device { cmd: DeviceCmd::Revoke { name, json } } => (CtlRequest::Revoke { name: name.clone() }, *json),
        Cmd::Ls { node: None, json } => (CtlRequest::List, *json),
        Cmd::Ls { node: Some(node), json } => (CtlRequest::Services { node: node.clone() }, *json),
        Cmd::Publish { name, port, host, proto, env, json } => {
            let mut pairs = Vec::new();
            for kv in env {
                match kv.split_once('=') {
                    Some((k, v)) => pairs.push((k.to_string(), v.to_string())),
                    None => return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("--env 的格式应为 名字=值：{kv}")),
                }
            }
            let addr = std::net::SocketAddr::new(*host, *port);
            (CtlRequest::Publish { name: name.clone(), addr, proto: proto.clone(), env: pairs }, *json)
        }
        Cmd::Unpublish { name, json } => (CtlRequest::Unpublish { name: name.clone() }, *json),
        Cmd::Connect { target, listen } => {
            let Some((node, name)) = target.split_once('/').filter(|(n, s)| !n.is_empty() && !s.is_empty()) else {
                return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("服务的路径应为 <设备>/<服务>：{target}"));
            };
            return match listen {
                None => connect_stdio(ctl, node, name, stdin, sink).await,
                Some(addr) => connect_listen(ctl, node, name, addr, sink).await,
            };
        }
        Cmd::Route { cmd: RouteCmd::Add { domain, domain_exact, cidr, reject, via, socks5, json, .. } } => {
            let matcher = match (domain, domain_exact, cidr) {
                (Some(d), _, _) => route::Matcher::parse_domain(d),
                (None, Some(d), _) => route::Matcher::parse_domain_exact(d),
                (None, None, Some(c)) => route::Matcher::parse_cidr(c),
                (None, None, None) => Err("需要 --domain、--domain-exact 或 --cidr".to_string()),
            };
            let matcher = match matcher {
                Ok(m) => m,
                Err(msg) => return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &msg),
            };
            let outbound = match (via, socks5) {
                (Some(device), _) => route::Outbound::Exit(device.clone()),
                (None, Some(upstream)) => route::Outbound::Socks5(upstream.clone()),
                (None, None) if *reject => route::Outbound::Reject,
                (None, None) => route::Outbound::Direct,
            };
            (CtlRequest::RouteAdd { rule: route::Rule { matcher, outbound } }, *json)
        }
        Cmd::Exit { cmd: ExitCmd::On { json } } => (CtlRequest::ExitSet { enabled: true }, *json),
        Cmd::Exit { cmd: ExitCmd::Off { json } } => (CtlRequest::ExitSet { enabled: false }, *json),
        Cmd::Exit { cmd: ExitCmd::Status { json } } => (CtlRequest::ExitShow, *json),
        Cmd::Route { cmd: RouteCmd::Rm { index, json } } => (CtlRequest::RouteRm { index: *index }, *json),
        Cmd::Route { cmd: RouteCmd::Ls { json } } => (CtlRequest::RouteList, *json),
        Cmd::Route { cmd: RouteCmd::Explain { target, json } } => {
            if nexus_net_core::Target::parse(target).is_none() {
                return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("目标应当写成 主机:端口：{target}"));
            }
            (CtlRequest::RouteExplain { target: target.clone() }, *json)
        }
        Cmd::Route { cmd: RouteCmd::Import { file, via, socks5, replace, dry_run, json } } => {
            let text = match std::fs::read_to_string(file) {
                Ok(t) => t,
                Err(e) => return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("无法读取规则文件 {}：{e}", file.display())),
            };
            let opts = nexus_net_core::import::ImportOptions {
                default_exit: via.clone(),
                default_socks5: socks5.clone(),
            };
            let report = nexus_net_core::import::parse_clash_rules(&text, &opts);
            let audit = route::audit_rules(&report.imported);
            if *dry_run {
                if *json {
                    sink.out(doc(json!({
                        "schema": JSON_SCHEMA,
                        "dry_run": true,
                        "imported": report.imported.len(),
                        "skipped": report.skipped,
                        "notes": report.notes,
                        "audit": audit,
                    })).as_bytes());
                } else {
                    let mut out = format!("试运行：可导入 {} 条，跳过 {} 条
", report.imported.len(), report.skipped.len());
                    for s in &report.skipped {
                        out.push_str(&format!("  跳过 第{}行 {}: {}
", s.line, s.raw, s.reason));
                    }
                    for n in &report.notes {
                        out.push_str(&format!("  提示 {n}
"));
                    }
                    for i in &audit.issues {
                        out.push_str(&format!("  审计 {}
", i.message));
                    }
                    sink.out(out.as_bytes());
                }
                return 0;
            }
            (CtlRequest::RouteImport { rules: report.imported, replace: *replace }, *json)
        }
        Cmd::Route { cmd: RouteCmd::Rollback { json } } => (CtlRequest::RouteRollback, *json),
        Cmd::Route { cmd: RouteCmd::Audit { json } } => (CtlRequest::RouteAudit, *json),
        Cmd::Run { uses, command } => return run_with_services(ctl, uses, command, sink).await,
        Cmd::Open { target, port, no_browser } => return open_in_browser(ctl, target, *port, !*no_browser, sink).await,
        Cmd::Drive { cmd } => return drive::run(cmd, ctl, stdin, sink).await,
        Cmd::Desktop { cmd } => return desktop::run(cmd, ctl, sink).await,
        Cmd::Ssh { .. } => {
            let args = match ssh_args(cmd) {
                Ok(a) => a,
                Err(msg) => return fail(sink, EXIT_USAGE, "usage", &msg),
            };
            let (_, mut no_resizes) = mpsc::channel(1);
            let mut out = SinkWriter(sink);
            let result = ssh(ctl, &args, stdin, &mut out, &mut no_resizes).await;
            return match result {
                Ok(code) => code,
                Err(e) => ssh_fail(sink, &e),
            };
        }
        Cmd::Mcp => return mcp(ctl, stdin, sink).await,
        Cmd::SshConfig | Cmd::SshProxy { .. } | Cmd::SessionHold { .. } | Cmd::ProtocolVersion | Cmd::Web { cmd: WebCmd::Serve { .. } } => {
            return fail(sink, EXIT_USAGE, "usage", "这条命令只能由 nexus 程序直接运行")
        }
        Cmd::Agent { cmd: AgentCmd::Issue { label, grants, ttl, json, .. } } => {
            let Some(ttl_ms) = parse_ttl(ttl) else {
                return fail(sink, EXIT_USAGE, "usage", &format!("--ttl 的格式应为 数字加单位（s、m、h、d），例如 4h：{ttl}"));
            };
            (CtlRequest::AgentIssue { label: label.clone(), grants: grants.clone(), ttl_ms }, *json)
        }
        Cmd::Agent { cmd: AgentCmd::Revoke { who, json } } => (CtlRequest::AgentRevoke { agent: who.clone() }, *json),
        Cmd::Agent { cmd: AgentCmd::Ls { json } } => (CtlRequest::AgentList, *json),
        Cmd::Audit { limit, json } => (CtlRequest::Audit { limit: *limit }, *json),
        Cmd::Web { cmd: WebCmd::Enroll { label, grants, ttl, page, json } } => {
            let Some(ttl_ms) = parse_ttl(ttl) else {
                return fail(sink, EXIT_USAGE, "usage", &format!("--ttl 的格式应为 数字加单位（s、m、h、d），例如 10m：{ttl}"));
            };
            (CtlRequest::WebEnroll { label: label.clone(), grants: grants.clone(), ttl_ms, page: page.clone() }, *json)
        }
        Cmd::Hw { cmd: HwCmd::Ls { node_id, hint, json } } => {
            let Ok(id) = node_id.parse() else {
                return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("节点标识格式不对：{node_id}"));
            };
            (CtlRequest::HardwareList { node_id: id, hint: hint.clone() }, *json)
        }
        Cmd::Hw { cmd: HwCmd::Capture { node_id, kind, hint, json } } => {
            let Ok(id) = node_id.parse() else {
                return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("节点标识格式不对：{node_id}"));
            };
            let Some(k) = nexus_wire::HardwareKind::parse(kind) else {
                return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("硬件能力应为 camera、microphone 或 location：{kind}"));
            };
            (CtlRequest::HardwareCapture { node_id: id, hint: hint.clone(), kind: k }, *json)
        }
        Cmd::Share { cmd: ShareCmd::Create { drive, service, terminal, ttl, max_uses, page, json } } => {
            let Some(ttl_ms) = parse_ttl(ttl) else {
                return fail(sink, EXIT_USAGE, "usage", &format!("--ttl 的格式应为 数字加单位（s、m、h、d），例如 1h：{ttl}"));
            };
            let target = match (drive, service, terminal) {
                (Some(d), None, None) => {
                    let Some((ws, p)) = d.split_once(":/") else {
                        return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("工作区路径应写成 <工作区>:/<路径>：{d}"));
                    };
                    nexus_wire::ShareTarget::Drive { ws: ws.to_string(), path: format!("/{p}") }
                }
                (None, Some(s), None) => nexus_wire::ShareTarget::Service { name: s.clone() },
                (None, None, Some(t)) => nexus_wire::ShareTarget::Terminal { session: t.clone() },
                _ => return fail(sink, EXIT_USAGE, "usage", "必须且只能指定 --drive、--service 或 --terminal 之一"),
            };
            (CtlRequest::ShareCreate { target, ttl_ms, max_uses: *max_uses, page: page.clone() }, *json)
        }
        Cmd::Share { cmd: ShareCmd::Ls { json } } => (CtlRequest::ShareList, *json),
        Cmd::Share { cmd: ShareCmd::Revoke { id, json } } => (CtlRequest::ShareRevoke { id: id.clone() }, *json),
        Cmd::Vault { cmd: VaultCmd::Put { name, value, devices, json } } => {
            let bytes = match value {
                Some(v) => v.as_bytes().to_vec(),
                None => {
                    let mut buf = Vec::new();
                    let _ = stdin.read_to_end(&mut buf).await;
                    buf
                }
            };
            (CtlRequest::VaultPut { name: name.clone(), value: bytes, devices: devices.clone() }, *json)
        }
        Cmd::Vault { cmd: VaultCmd::Get { name, json } } => (CtlRequest::VaultGet { name: name.clone() }, *json),
        Cmd::Vault { cmd: VaultCmd::Ls { json } } => (CtlRequest::VaultList, *json),
        Cmd::Vault { cmd: VaultCmd::Rm { name, json } } => (CtlRequest::VaultRm { name: name.clone() }, *json),
        Cmd::Vault { cmd: VaultCmd::Sync { json } } => (CtlRequest::VaultSync, *json),
        Cmd::Vault { cmd: VaultCmd::RotateNeeded { device, json } } => {
            (CtlRequest::VaultRotateNeeded { device: device.clone() }, *json)
        }
        Cmd::Web { cmd: WebCmd::Ls { json } } => (CtlRequest::WebList, *json),
        Cmd::Anchor { cmd: AnchorCmd::Set { url, clear, json } } => {
            (CtlRequest::AnchorSet { url: if *clear { None } else { url.clone() } }, *json)
        }
        Cmd::Anchor { cmd: AnchorCmd::Show { json } } => (CtlRequest::AnchorShow, *json),
        Cmd::Web { cmd: WebCmd::Revoke { label, json } } => (CtlRequest::WebRevoke { label: label.clone() }, *json),
        Cmd::Session { cmd: SessionCmd::Ls { node, json } } => (CtlRequest::SessionList { node: node.clone() }, *json),
        Cmd::Session { cmd: SessionCmd::Kill { node, session, json } } => {
            (CtlRequest::SessionKill { node: node.clone(), session: session.clone() }, *json)
        }
    };
    let resp = match call(ctl, req).await {
        Err(e) => {
            return fail(sink, EXIT_DAEMON_UNREACHABLE, "daemon_unreachable", &format!("连不上本机的常驻程序（{e}）{DAEMON_HINT}"))
        }
        Ok(r) => r,
    };
    let text = match resp {
        CtlResponse::Error(e) => return fail_wire(sink, &e),
        CtlResponse::Exec(_)
        | CtlResponse::Session(_)
        | CtlResponse::SshHostKey(_)
        | CtlResponse::PipeData(_)
        | CtlResponse::PipeEnd
        | CtlResponse::Tun(_)
        | CtlResponse::Drive(_) => {
            return fail(sink, EXIT_INTERNAL, "internal", "常驻程序的回应无法理解")
        }
        CtlResponse::Sessions(list) => if json { sessions_json(&list) } else { sessions_text(&list) },
        CtlResponse::AgentIssued(r) => {
            let out = match cmd {
                Cmd::Agent { cmd: AgentCmd::Issue { out, .. } } => out.clone(),
                _ => None,
            };
            if let Some(path) = &out {
                if let Err(e) = write_private(path, &r.credential) {
                    return fail(sink, EXIT_INTERNAL, "internal", &format!("写不了凭据文件 {}：{e}", path.display()));
                }
            }
            let agent_id = nexus_core_proto::NodeId(r.agent_id.0).to_string();
            if json {
                let mut v = json!({
                    "schema": JSON_SCHEMA,
                    "agent_id": agent_id,
                    "label": r.label,
                    "not_after_ms": r.not_after.0,
                });
                match &out {
                    Some(path) => v["credential_file"] = json!(path.display().to_string()),
                    None => v["credential"] = json!(r.credential),
                }
                doc(v)
            } else {
                let how = match &out {
                    Some(path) => format!("凭据已写入 {}。\n使用：NEXUS_AGENT={} nexus <命令>\n", path.display(), path.display()),
                    None => format!("凭据（含私钥，请像密码一样保管）：\n\n{}\n\n使用：NEXUS_AGENT=<凭据> nexus <命令>，或 nexus --agent <凭据> <命令>\n", r.credential),
                };
                format!("已签发 agent {}（{}），有效期至 {}。\n{how}", r.label, &agent_id[..8], local_time(r.not_after.0))
            }
        }
        CtlResponse::AgentRevoked(m) => {
            if json {
                doc(json!({
                    "schema": JSON_SCHEMA,
                    "label": m.name,
                    "agent_id": m.node_id.to_string(),
                    "roster_version": m.roster_version,
                    "pushed": m.pushed,
                    "unreachable": m.unreachable,
                }))
            } else {
                let mut s = format!("已撤销 agent {}，名册版本 {}，已通知 {} 台设备。\n", m.name, m.roster_version, m.pushed);
                if !m.unreachable.is_empty() {
                    s.push_str(&format!("暂时没联系上：{}。它们会在下次与已更新的设备通信时得到新名册。\n", m.unreachable.join("、")));
                }
                if matches!(cmd, Cmd::Device { cmd: DeviceCmd::Revoke { .. } }) {
                    if let Ok(CtlResponse::VaultRotateNeeded(items)) =
                        call(ctl, CtlRequest::VaultRotateNeeded { device: Some(m.name.clone()) }).await
                    {
                        for item in items {
                            if !item.entries.is_empty() {
                                s.push_str(&format!("需要轮换的 Vault 条目：{}\n", item.entries.join("、")));
                            }
                        }
                    }
                }
                s
            }
        }
        CtlResponse::Agents(list) => if json { agents_json(&list) } else { agents_text(&list) },
        CtlResponse::HardwareList(list) => {
            let names: Vec<&str> = list.iter().map(|k| k.as_str()).collect();
            if json {
                format!("{}\n", json!({ "capabilities": names }))
            } else if names.is_empty() {
                "没有开放的硬件能力。\n".into()
            } else {
                format!("{}\n", names.join("\n"))
            }
        }
        CtlResponse::HardwareCapture(cap) => {
            if json {
                let v = match cap {
                    nexus_wire::HardwareCapture::Camera { mime, data } => {
                        json!({ "kind": "camera", "mime": mime, "data_base64": base64(&data) })
                    }
                    nexus_wire::HardwareCapture::Microphone { mime, duration_ms, data } => {
                        json!({ "kind": "microphone", "mime": mime, "duration_ms": duration_ms, "data_base64": base64(&data) })
                    }
                    nexus_wire::HardwareCapture::Location { lat_e7, lon_e7, accuracy_cm, timestamp_ms } => {
                        json!({ "kind": "location", "lat_e7": lat_e7, "lon_e7": lon_e7, "accuracy_cm": accuracy_cm, "timestamp_ms": timestamp_ms })
                    }
                };
                format!("{v}\n")
            } else {
                match cap {
                    nexus_wire::HardwareCapture::Camera { mime, data } => {
                        format!("camera ({mime}, {} bytes)\n", data.len())
                    }
                    nexus_wire::HardwareCapture::Microphone { mime, duration_ms, data } => {
                        format!("microphone ({mime}, {duration_ms}ms, {} bytes)\n", data.len())
                    }
                    nexus_wire::HardwareCapture::Location { lat_e7, lon_e7, accuracy_cm, .. } => {
                        format!("location ({:.6}, {:.6}, ±{:.2}m)\n", lat_e7 as f64 / 1e7, lon_e7 as f64 / 1e7, accuracy_cm as f64 / 100.0)
                    }
                }
            }
        }
        CtlResponse::ShareCreated(r) => {
            if json {
                format!(
                    "{}\n",
                    json!({
                        "id": r.id,
                        "url": r.url,
                        "kind": r.target.kind_str(),
                        "expires_at_ms": r.expires_at.0,
                        "max_uses": r.max_uses,
                    })
                )
            } else {
                format!("分享已创建（编号 {}）：\n  {}\n", r.id, r.url)
            }
        }
        CtlResponse::Shares(list) => {
            let now = if list.is_empty() {
                Timestamp(0)
            } else {
                match call(ctl, CtlRequest::Status).await {
                    Ok(CtlResponse::Status(st)) => match st.info {
                        Some(info) => info.now,
                        None => return fail(sink, EXIT_INTERNAL, "internal", "常驻程序没有返回当前时间"),
                    },
                    Ok(CtlResponse::Error(e)) => return fail_wire(sink, &e),
                    Ok(_) => return fail(sink, EXIT_INTERNAL, "internal", "常驻程序的回应无法理解"),
                    Err(e) => {
                        return fail(sink, EXIT_DAEMON_UNREACHABLE, "daemon_unreachable", &format!("连不上本机的常驻程序（{e}）{DAEMON_HINT}"));
                    }
                }
            };
            if json { shares_json(&list, now) } else { shares_text(&list, now) }
        }
        CtlResponse::ShareRevoked(s) => {
            if json {
                format!("{}\n", json!({ "id": s.id, "revoked": s.revoked }))
            } else {
                format!("已撤销分享 {}\n", s.id)
            }
        }
        CtlResponse::VaultPut(info) => {
            if json {
                format!(
                    "{}\n",
                    json!({
                        "name": info.name,
                        "version": info.version,
                        "updated_at_ms": info.updated_at.0,
                        "devices": info.devices,
                    })
                )
            } else {
                format!("已保存 Vault 条目 {}（版本 {}，授权设备：{}）\n", info.name, info.version, info.devices.join("、"))
            }
        }
        CtlResponse::VaultValue { name, value } => {
            let text = String::from_utf8_lossy(&value);
            if json {
                format!("{}\n", json!({ "name": name, "value": text }))
            } else {
                format!("{text}\n")
            }
        }
        CtlResponse::VaultList(list) => {
            if json {
                let entries: Vec<Value> = list
                    .iter()
                    .map(|e| {
                        json!({
                            "name": e.name,
                            "version": e.version,
                            "updated_at_ms": e.updated_at.0,
                            "devices": e.devices,
                        })
                    })
                    .collect();
                format!("{}\n", json!({ "entries": entries }))
            } else if list.is_empty() {
                "没有 Vault 条目。\n".into()
            } else {
                list.iter()
                    .map(|e| format!("{}  v{}  {}\n", e.name, e.version, e.devices.join(",")))
                    .collect()
            }
        }
        CtlResponse::VaultRotateNeeded(items) => {
            if json {
                let arr: Vec<Value> = items
                    .iter()
                    .map(|i| json!({ "revoked_device": i.revoked_device, "entries": i.entries }))
                    .collect();
                format!("{}\n", json!({ "items": arr }))
            } else if items.is_empty() {
                "没有需要轮换的 Vault 条目。\n".into()
            } else {
                items
                    .iter()
                    .map(|i| format!("已吊销设备 {} 接触过、建议轮换的条目：{}\n", i.revoked_device, i.entries.join("、")))
                    .collect()
            }
        }
        CtlResponse::RootRotated(r) => {
            let v = json!({
                "schema": JSON_SCHEMA,
                "old_root": nexus_core_proto::NodeId(r.old_root.0).to_string(),
                "new_root": nexus_core_proto::NodeId(r.new_root.0).to_string(),
                "recovery_code": r.recovery_code,
                "roster_version": r.roster_version,
                "pushed": r.pushed,
                "unreachable": r.unreachable,
            });
            if json {
                doc(v)
            } else {
                format!(
                    "已轮换网络根密钥（名册版本 {}，已通知 {} 台设备）。\n\n新恢复码（只显示这一次，请离线保存；旧恢复码已失效）：\n  {}\n",
                    r.roster_version, r.pushed, r.recovery_code
                )
            }
        }
        CtlResponse::WebEnroll(r) => {
            if json {
                doc(json!({ "schema": JSON_SCHEMA, "label": r.label, "url": r.url, "expires_at_ms": r.expires_at.0 }))
            } else {
                format!("在要用的设备上打开下面的链接，创建通行密钥后登记即完成（链接只能用一次）：\n\n{}\n", r.url)
            }
        }
        CtlResponse::WebKeys(list) => {
            if json {
                let keys: Vec<Value> = list
                    .iter()
                    .map(|k| json!({ "label": k.label, "rp_id": k.rp_id, "grants": k.grants, "added_at_ms": k.added_at.0 }))
                    .collect();
                doc(json!({ "schema": JSON_SCHEMA, "passkeys": keys }))
            } else if list.is_empty() {
                "还没有登记通行密钥\n".to_string()
            } else {
                list.iter().map(|k| format!("{:<16} {}  {}\n", k.label, k.rp_id, k.grants.join(" "))).collect()
            }
        }
        CtlResponse::WebRevoked(m) => {
            if json {
                doc(json!({
                    "schema": JSON_SCHEMA,
                    "label": m.name,
                    "roster_version": m.roster_version,
                    "pushed": m.pushed,
                    "unreachable": m.unreachable,
                }))
            } else {
                format!("已撤销通行密钥 {}，名册版本 {}，已通知 {} 台设备。\n", m.name, m.roster_version, m.pushed)
            }
        }
        CtlResponse::Anchor(a) => {
            if json {
                doc(json!({
                    "schema": JSON_SCHEMA,
                    "anchor": a.anchor,
                    "roster_version": a.roster_version,
                    "pushed": a.pushed,
                    "unreachable": a.unreachable,
                }))
            } else {
                let what = match &a.anchor {
                    Some(url) => format!("锚点：{url}"),
                    None => "没有设置锚点".to_string(),
                };
                match cmd {
                    Cmd::Anchor { cmd: AnchorCmd::Set { .. } } => {
                        format!("{what}。名册版本 {}，已通知 {} 台设备。\n", a.roster_version, a.pushed)
                    }
                    _ => format!("{what}\n"),
                }
            }
        }
        CtlResponse::Audit(records) => if json { audit_json(&records) } else { audit_text(&records) },
        CtlResponse::Ok => match cmd {
            Cmd::Route { cmd: RouteCmd::Rollback { .. } } => {
                if json {
                    doc(json!({ "schema": JSON_SCHEMA, "rolled_back": true }))
                } else {
                    "已回滚到上一版规则快照\n".into()
                }
            }
            Cmd::Vault { cmd: VaultCmd::Rm { name, .. } } => {
                if json {
                    doc(json!({ "schema": JSON_SCHEMA, "removed": name }))
                } else {
                    format!("已删除 Vault 条目 {name}\n")
                }
            }
            Cmd::Session { cmd: SessionCmd::Kill { session, .. } } => {
                if json {
                    doc(json!({ "schema": JSON_SCHEMA, "killed": session }))
                } else {
                    format!("已结束会话 {session}\n")
                }
            }
            Cmd::Route { cmd: RouteCmd::Rm { index, .. } } => {
                if json {
                    doc(json!({ "schema": JSON_SCHEMA, "removed": index }))
                } else {
                    format!("已删除第 {index} 条规则\n")
                }
            }
            Cmd::Unpublish { name, .. } => {
                if json {
                    doc(json!({ "schema": JSON_SCHEMA, "unpublished": name }))
                } else {
                    format!("已撤销发布 {name}\n")
                }
            }
            _ => String::new(),
        },
        CtlResponse::RouteImported { total, audit } => {
            if json {
                doc(json!({ "schema": JSON_SCHEMA, "total": total, "audit": audit }))
            } else if audit.is_clean() {
                format!("已导入规则（当前共 {total} 条，静态审计无冲突；已保存上一版 LKG 快照）
")
            } else {
                let mut out = format!("已导入规则（当前共 {total} 条，发现 {} 条审计提示；已保存上一版 LKG 快照）：
", audit.issues.len());
                for i in &audit.issues {
                    out.push_str(&format!("  - {}
", i.message));
                }
                out
            }
        }
        CtlResponse::RouteAudit(audit) => {
            if json {
                doc(json!({ "schema": JSON_SCHEMA, "clean": audit.is_clean(), "issues": audit.issues }))
            } else if audit.is_clean() {
                "规则静态审计通过：无重复、无遮蔽、无无效上游
".into()
            } else {
                let mut out = format!("规则静态审计发现 {} 项问题：
", audit.issues.len());
                for i in &audit.issues {
                    out.push_str(&format!("  - {}
", i.message));
                }
                out
            }
        }
        CtlResponse::Exit { enabled } => {
            if json {
                doc(json!({ "schema": JSON_SCHEMA, "exit": enabled }))
            } else if enabled {
                "出口已开放：名册里的设备可以经本机访问外网（不包括本机地址）\n".to_string()
            } else {
                "出口没有开放\n".to_string()
            }
        }
        CtlResponse::RouteAdded { index } => {
            if json {
                doc(json!({ "schema": JSON_SCHEMA, "index": index }))
            } else {
                format!("已加为第 {index} 条规则\n")
            }
        }
        CtlResponse::Routes(rules) => {
            if json {
                let list: Vec<Value> = rules
                    .iter()
                    .enumerate()
                    .map(|(i, r)| outbound_json(&r.outbound, json!({ "index": i + 1, "match": r.matcher.to_string() })))
                    .collect();
                doc(json!({ "schema": JSON_SCHEMA, "rules": list }))
            } else if rules.is_empty() {
                format!("没有路由规则：.{SERVICE_SUFFIX} 的名字去网络里的服务，其余直连\n")
            } else {
                rules.iter().enumerate().map(|(i, r)| format!("{:>3}  {:<32} {}\n", i + 1, r.matcher.to_string(), r.outbound)).collect()
            }
        }
        CtlResponse::Explained(e) => {
            let target = match cmd {
                Cmd::Route { cmd: RouteCmd::Explain { target, .. } } => target.as_str(),
                _ => "",
            };
            if json {
                let v = match &e {
                    RouteExplanation::Service { device, service } => {
                        json!({ "outbound": "nexus", "rule": null, "device": device, "service": service })
                    }
                    RouteExplanation::BadInternalName => json!({ "outbound": "invalid", "rule": null }),
                    RouteExplanation::Rule { index, rule } => {
                        outbound_json(&rule.outbound, json!({ "rule": index, "match": rule.matcher.to_string() }))
                    }
                    RouteExplanation::Default { outbound } => outbound_json(outbound, json!({ "rule": null })),
                };
                let mut v = v;
                v["schema"] = json!(JSON_SCHEMA);
                v["target"] = json!(target);
                doc(v)
            } else {
                match &e {
                    RouteExplanation::Service { device, service } => format!("{target} → 经 Nexus 到 {device} 上的服务 {service}\n"),
                    RouteExplanation::BadInternalName => {
                        format!("{target} → 不可用：.{SERVICE_SUFFIX} 的名字应当是 <服务>.<设备>.{SERVICE_SUFFIX}\n")
                    }
                    RouteExplanation::Rule { index, rule } => {
                        format!("{target} → {}（第 {index} 条规则：{}）\n", rule.outbound, rule.matcher)
                    }
                    RouteExplanation::Default { outbound } => format!("{target} → {outbound}（没有规则匹配）\n"),
                }
            }
        }
        CtlResponse::Published { path } => {
            if json {
                doc(json!({ "schema": JSON_SCHEMA, "path": path }))
            } else {
                format!("已发布 {path}。其他设备：nexus connect {path}\n")
            }
        }
        CtlResponse::Services { node, services } => {
            if json {
                let list: Vec<Value> = services
                    .iter()
                    .map(|s| {
                        let env: serde_json::Map<String, Value> = s.env.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
                        json!({
                            "name": s.name,
                            "path": format!("{node}/{}", s.name),
                            "dns": format!("{}.{node}.{SERVICE_SUFFIX}", s.name),
                            "proto": s.proto,
                            "env": env,
                        })
                    })
                    .collect();
                doc(json!({ "schema": JSON_SCHEMA, "node": node, "services": list }))
            } else if services.is_empty() {
                format!("{node} 上没有你能用的服务\n")
            } else {
                let mut out = String::new();
                for s in &services {
                    out.push_str(&format!("{node}/{:<20} {}\n", s.name, s.proto));
                    for (k, v) in &s.env {
                        out.push_str(&format!("    {k}={v}\n"));
                    }
                }
                out
            }
        }
        CtlResponse::Interfaces(list) => {
            let only = match cmd {
                Cmd::Iface { name: Some(n), .. } => Some(n.clone()),
                _ => None,
            };
            let list: Vec<_> = list.into_iter().filter(|i| only.as_ref().is_none_or(|n| &i.name == n)).collect();
            if let (Some(n), true) = (&only, list.is_empty()) {
                return fail(sink, EXIT_NOT_FOUND, "not_found", &format!("本机没有启用的接口 {n}"));
            }
            if json {
                let v: Vec<Value> = list
                    .iter()
                    .map(|i| json!({ "name": i.name, "addrs": i.addrs, "metered": i.metered, "auto_metered": i.auto_metered, "setting": i.setting }))
                    .collect();
                doc(json!({ "schema": JSON_SCHEMA, "interfaces": v }))
            } else if list.is_empty() {
                "没有可用的网络接口信息（这个传输不区分接口）\n".to_string()
            } else {
                let mut out = String::new();
                for i in &list {
                    let how = match i.setting {
                        Some(_) => "手动设置",
                        None => "自动识别",
                    };
                    let state = if i.metered { "计费：只承担交互和故障接管" } else { "不计费" };
                    out.push_str(&format!("{:<12} {:<40} {state}（{how}）\n", i.name, i.addrs.join(" ")));
                }
                out
            }
        }
        CtlResponse::Upgrade(r) => {
            if json {
                doc(json!({ "schema": JSON_SCHEMA, "state": r.state, "from": r.from, "to": r.to, "exe": r.exe, "message": r.message, "id": r.id }))
            } else {
                match r.state.as_str() {
                    "pending" => format!(
                        "{} 换成了 {}，常驻程序正在重新启动；连上其他设备后确认（nexus upgrade --status 查看）。\n",
                        r.from, r.to
                    ),
                    "confirmed" => format!("已经从 {} 升级到 {}（{}）。\n", r.from, r.to, r.exe),
                    "rolled_back" => format!("已经换回 {}：{}\n", r.from, r.message),
                    "none" => "还没有升级过。\n".to_string(),
                    other => format!("{other}：{}\n", r.message),
                }
            }
        }
        CtlResponse::Paths(devices) => {
            if json {
                let list: Vec<Value> = devices
                    .iter()
                    .map(|d| {
                        let paths: Vec<Value> = d
                            .paths
                            .iter()
                            .map(|p| {
                                json!({
                                    "local": p.local,
                                    "remote": p.remote,
                                    "kind": path_str(Some(p.kind)),
                                    "rtt_ms": p.rtt_us as f64 / 1000.0,
                                    "tx_bytes": p.tx_bytes,
                                    "rx_bytes": p.rx_bytes,
                                    "lost_packets": p.lost_packets,
                                    "metered": p.metered,
                                })
                            })
                            .collect();
                        json!({ "device": d.device, "paths": paths })
                    })
                    .collect();
                doc(json!({ "schema": JSON_SCHEMA, "devices": list }))
            } else {
                let mut out = String::new();
                for d in &devices {
                    if d.paths.is_empty() {
                        out.push_str(&format!("{}：没有连接\n", d.device));
                        continue;
                    }
                    out.push_str(&format!("{}：{} 条路径\n", d.device, d.paths.len()));
                    for p in &d.paths {
                        let kind = match p.kind {
                            nexus_core_proto::PathKind::Direct => "直连",
                            nexus_core_proto::PathKind::Relay => "中继",
                        };
                        let metered = if p.metered { "，计费" } else { "" };
                        out.push_str(&format!(
                            "  {} → {}  {kind}{metered}  往返 {:.1} 毫秒  发 {} 收 {} 字节  丢包 {}\n",
                            p.local,
                            p.remote,
                            p.rtt_us as f64 / 1000.0,
                            p.tx_bytes,
                            p.rx_bytes,
                            p.lost_packets
                        ));
                    }
                }
                out
            }
        }
        CtlResponse::Status(r) => {
            // 本机的状态里加上完整节点的虚拟网卡。
            let tun = match cmd {
                Cmd::Status { node: None, .. } => match call(ctl, CtlRequest::TunShow).await {
                    Ok(CtlResponse::Tun(t)) => t,
                    _ => None,
                },
                _ => None,
            };
            if json {
                let mut v = status_json(&r);
                if let Some(t) = &tun {
                    v["tun"] = json!({ "name": t.name, "net": t.net, "dns": t.dns });
                }
                doc(v)
            } else {
                let mut out = status_text(&r);
                if let Some(t) = &tun {
                    out.push_str(&format!("  完整节点：虚拟网卡 {}，网段 {}，名字服务 {}\n", t.name, t.net, t.dns));
                }
                out
            }
        }
        CtlResponse::List(entries) => if json { list_json(&entries) } else { list_text(&entries) },
        CtlResponse::Init(r) => {
            let v = json!({
                "schema": JSON_SCHEMA,
                "name": r.name,
                "node_id": r.node_id.to_string(),
                "root": nexus_core_proto::NodeId(r.root.0).to_string(),
                "recovery_code": r.recovery_code,
            });
            if json {
                doc(v)
            } else {
                format!(
                    "已建立网络，本机 {} 是管理设备。\n\n恢复码（只显示这一次，请离线保存。管理设备丢了时，在任何一台还在的设备上 `nexus recover <恢复码>`）：\n  {}\n\n在另一台设备上运行 `nexus join`，再回到这里用 `nexus device approve <加入码>` 批准它。\n",
                    r.name, r.recovery_code
                )
            }
        }
        CtlResponse::JoinCode { code, short_code, short_code_error, expires_at } => if json {
            doc(json!({
                "schema": JSON_SCHEMA, "code": code, "short_code": short_code, "short_code_error": short_code_error,
                "expires_at_ms": expires_at.0,
            }))
        } else {
            match (short_code, short_code_error) {
                (Some(short), _) => format!(
                    "加入码（10 分钟内有效）：\n\n    {short}\n\n在管理设备上运行：nexus device approve {short}\n\n\
                     管理设备连不上碰头服务时，改用长加入码：\n{code}\n"
                ),
                (None, why) => format!(
                    "加入码（10 分钟内有效）：\n\n{code}\n\n在管理设备上运行：nexus device approve <加入码>\n\n\
                     只能用长加入码：{}\n",
                    why.unwrap_or_default()
                ),
            }
        },
        CtlResponse::Membership(m) => {
            let v = json!({
                "schema": JSON_SCHEMA,
                "name": m.name,
                "node_id": m.node_id.to_string(),
                "roster_version": m.roster_version,
                "pushed": m.pushed,
                "unreachable": m.unreachable,
            });
            if json {
                doc(v)
            } else {
                let mut s = format!("完成：{}，名册版本 {}，已通知 {} 台设备。\n", m.name, m.roster_version, m.pushed);
                if !m.unreachable.is_empty() {
                    s.push_str(&format!("暂时没联系上：{}。它们会在下次与已更新的设备通信时得到新名册。\n", m.unreachable.join("、")));
                }
                if matches!(cmd, Cmd::Device { cmd: DeviceCmd::Revoke { .. } }) {
                    if let Ok(CtlResponse::VaultRotateNeeded(items)) =
                        call(ctl, CtlRequest::VaultRotateNeeded { device: Some(m.name.clone()) }).await
                    {
                        for item in items {
                            if !item.entries.is_empty() {
                                s.push_str(&format!("需要轮换的 Vault 条目：{}\n", item.entries.join("、")));
                            }
                        }
                    }
                }
                s
            }
        }
    };
    sink.out(text.as_bytes());
    0
}

/// 只有当前用户能读写的文件。
fn write_private(path: &std::path::Path, text: &str) -> io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(text.as_bytes())?;
        f.write_all(b"\n")
    }
    #[cfg(windows)]
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(path)?;
        f.write_all(text.as_bytes())?;
        f.write_all(b"\n")
    }
}

fn agents_json(list: &[AgentInfo]) -> String {
    let agents: Vec<Value> = list
        .iter()
        .map(|a| {
            json!({
                "agent_id": nexus_core_proto::NodeId(a.agent_id.0).to_string(),
                "label": a.label,
                "issued_at_ms": a.issued_at.0,
                "not_after_ms": a.not_after.0,
                "grants": a.grants,
                "revoked": a.revoked,
            })
        })
        .collect();
    doc(json!({ "schema": JSON_SCHEMA, "agents": agents }))
}

fn agents_text(list: &[AgentInfo]) -> String {
    if list.is_empty() {
        return "还没有签发过 agent\n".to_string();
    }
    list.iter()
        .map(|a| {
            let state = if a.revoked { "已撤销" } else { "有效" };
            format!("{:<16} {}  {}  {}\n", a.label, &nexus_core_proto::NodeId(a.agent_id.0).to_string()[..8], state, a.grants.join(" "))
        })
        .collect()
}

fn share_status_label(status: ShareStatus) -> &'static str {
    match status {
        ShareStatus::Active => "有效",
        ShareStatus::Expired => "已过期",
        ShareStatus::Exhausted => "已用尽",
        ShareStatus::Revoked => "已撤销",
    }
}

fn shares_json(list: &[ShareRecord], now: Timestamp) -> String {
    let items: Vec<Value> = list
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "kind": s.target.kind_str(),
                "created_at_ms": s.created_at.0,
                "expires_at_ms": s.expires_at.0,
                "max_uses": s.max_uses,
                "uses": s.uses,
                "revoked": s.revoked,
                "expired": s.is_expired(now),
                "state": s.status_at(now).as_str(),
            })
        })
        .collect();
    format!("{}\n", json!({ "shares": items }))
}

fn shares_text(list: &[ShareRecord], now: Timestamp) -> String {
    if list.is_empty() {
        return "没有临时分享。\n".into();
    }
    list.iter()
        .map(|s| {
            let status = share_status_label(s.status_at(now));
            let uses = match s.max_uses {
                Some(m) => format!("{}/{m}", s.uses),
                None => format!("{}（不限次数）", s.uses),
            };
            format!("{}  {}  已用 {uses}  {status}\n", s.id, s.target.kind_str())
        })
        .collect()
}

fn deny_kind(r: &nexus_core_proto::authz::DenyReason) -> &'static str {
    use nexus_core_proto::authz::DenyReason::*;
    match r {
        PeerNotInRoster => "peer_not_in_roster",
        CertBadSignature => "cert_bad_signature",
        CertNotYetValid => "cert_not_yet_valid",
        CertExpired => "cert_expired",
        AgentRevoked => "agent_revoked",
        AssertionBadSignature => "assertion_bad_signature",
        WrongTarget => "wrong_target",
        ActionMismatch => "action_mismatch",
        Stale => "stale",
        NoMatchingGrant => "no_matching_grant",
        Replayed => "replayed",
        IssuerNotInRoster => "issuer_not_in_roster",
        SessionBadSignature => "session_bad_signature",
        SessionNotYetValid => "session_not_yet_valid",
        SessionExpired => "session_expired",
        SessionTooLong => "session_too_long",
        WrongBrowser => "wrong_browser",
        WebKeyRevoked => "web_key_revoked",
    }
}

fn audit_json(records: &[AuditRecord]) -> String {
    use nexus_core_proto::authz::{Outcome, PrincipalRef};
    let events: Vec<Value> = records
        .iter()
        .map(|r| {
            let e = &r.event;
            let principal = match &e.principal {
                PrincipalRef::Device(_) => json!({ "device": r.peer_name }),
                PrincipalRef::Agent { id, label } => json!({ "agent_id": nexus_core_proto::NodeId(id.0).to_string(), "label": label }),
                PrincipalRef::Web { label } => json!({ "passkey": label }),
            };
            let (outcome, reason) = match &e.outcome {
                Outcome::Allow => ("allow", None),
                Outcome::Deny(r) => ("deny", Some(deny_kind(r))),
            };
            json!({
                "at_ms": e.at.0,
                "peer": r.peer_name,
                "principal": principal,
                "capability": e.capability,
                "resource": e.resource,
                "outcome": outcome,
                "reason": reason,
            })
        })
        .collect();
    doc(json!({ "schema": JSON_SCHEMA, "events": events }))
}

/// 毫秒时间戳 → 本地时间，例如 `2026-11-19 23:05:48`。
pub fn local_time(ms: u64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(ms as i64) {
        chrono::LocalResult::Single(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => format!("{ms} 毫秒时间戳"),
    }
}

fn audit_text(records: &[AuditRecord]) -> String {
    use nexus_core_proto::authz::{Outcome, PrincipalRef};
    if records.is_empty() {
        return "没有审计记录\n".to_string();
    }
    records
        .iter()
        .map(|r| {
            let e = &r.event;
            let who = match &e.principal {
                PrincipalRef::Device(_) => r.peer_name.clone(),
                PrincipalRef::Agent { label, .. } => format!("agent {label}（经由 {}）", r.peer_name),
                PrincipalRef::Web { label } => format!("网页 {label}"),
            };
            let outcome = match &e.outcome {
                Outcome::Allow => "允许".to_string(),
                Outcome::Deny(reason) => format!("拒绝：{}", deny_kind(reason)),
            };
            format!("{}  {}  {} {}  {}\n", local_time(e.at.0), who, e.capability, e.resource, outcome)
        })
        .collect()
}

fn sessions_json(list: &[SessionInfo]) -> String {
    let sessions: Vec<Value> = list
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "clients": s.clients,
                "created_at_ms": s.created_at.0,
                "cols": s.size.cols,
                "rows": s.size.rows,
            })
        })
        .collect();
    doc(json!({ "schema": JSON_SCHEMA, "sessions": sessions }))
}

fn sessions_text(list: &[SessionInfo]) -> String {
    if list.is_empty() {
        return "没有会话\n".to_string();
    }
    list.iter()
        .map(|s| format!("{:<16} {} 个客户端  {}x{}\n", s.name, s.clients, s.size.cols, s.size.rows))
        .collect()
}

/// `nexus ssh` 的参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshArgs {
    pub node: String,
    pub session: String,
    /// 初始的终端尺寸。
    pub size: Size,
    /// 命令行上是否明确给出了尺寸。
    pub size_given: bool,
}

/// 从解析好的命令里取出 `nexus ssh` 的参数。
pub fn ssh_args(cmd: &Cmd) -> Result<SshArgs, String> {
    let Cmd::Ssh { node, session, size } = cmd else { return Err("不是 ssh 命令".into()) };
    let parsed = match size {
        None => None,
        Some(text) => {
            let parse = || {
                let (c, r) = text.split_once(['x', 'X'])?;
                let size = Size { cols: c.trim().parse().ok()?, rows: r.trim().parse().ok()? };
                (size.cols > 0 && size.rows > 0).then_some(size)
            };
            Some(parse().ok_or_else(|| format!("--size 的格式应为 列x行，例如 80x24：{text}"))?)
        }
    };
    Ok(SshArgs { node: node.clone(), session: session.clone(), size: parsed.unwrap_or(Size::DEFAULT), size_given: parsed.is_some() })
}

/// `nexus ssh` 失败的原因。
#[derive(Debug)]
pub enum SshError {
    /// 连不上本机的常驻程序。
    Daemon(io::Error),
    Remote(WireError),
}

/// 按 [`SshError`] 输出一行结构化的错误，返回退出码。
pub fn ssh_fail<K: Sink>(sink: &mut K, e: &SshError) -> i32 {
    match e {
        SshError::Daemon(e) => {
            fail(sink, EXIT_DAEMON_UNREACHABLE, "daemon_unreachable", &format!("连不上本机的常驻程序（{e}）{DAEMON_HINT}"))
        }
        SshError::Remote(e) => fail_wire(sink, e),
    }
}

/// 断开会话的按键：Ctrl-]。
pub const DETACH_KEY: u8 = 0x1d;
/// 连接中断后持续重连这么久，仍连不上才放弃。
pub const RECONNECT_FOR: Duration = Duration::from_secs(30);

/// 把 [`Sink`] 当作异步输出使用。
struct SinkWriter<'a, K: Sink>(&'a mut K);

impl<K: Sink> AsyncWrite for SinkWriter<'_, K> {
    fn poll_write(mut self: Pin<&mut Self>, _: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.0.out(buf);
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// `nexus ssh`：连上会话，在本机终端和会话之间转发，直到断开或会话结束。
///
/// 每段输出写出之后才确认，所以本机终端处理不过来时，会话会把这个客户端标记为落后，
/// 而不是越积越多。返回会话结束时 shell 的退出码；主动断开时返回 0。
pub async fn ssh<C: Ctl, R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    ctl: &C,
    args: &SshArgs,
    input: &mut R,
    output: &mut W,
    resizes: &mut mpsc::Receiver<Size>,
) -> Result<i32, SshError> {
    let mut size = args.size;
    let mut resizes_open = true;
    let mut connected_once = false;
    let mut lost_since: Option<tokio::time::Instant> = None;
    let mut buf = vec![0u8; 16 * 1024];
    let note = format!("\r\n[已断开，会话 {} 仍在运行]\r\n", args.session);
    loop {
        // 重新连上时不新建：会话已经结束（例如远端的程序在断开期间退出了）就照实说。
        let req = CtlRequest::Attach { node: args.node.clone(), session: args.session.clone(), size, create: !connected_once };
        let mut session = ctl.open(req).await.map_err(SshError::Daemon)?;
        // 发给常驻程序的消息先进本地队列，有空位时再发：这里从不因为发送而停下，
        // 总能继续读常驻程序的回应，两边不会互相等待。确认可以合并。
        let mut outbox: VecDeque<Input> = VecDeque::new();
        // 连上之后才读输入：连接失败时不能让输入结束被当成断开。
        let mut attached = false;
        let mut leaving = false;
        let lost = loop {
            if leaving && outbox.is_empty() {
                let _ = output.write_all(note.as_bytes()).await;
                let _ = output.flush().await;
                return Ok(0);
            }
            tokio::select! {
                biased;
                resp = session.rx.recv() => match resp {
                    Some(CtlResponse::Session(msg)) => {
                        attached = true;
                        connected_once = true;
                        lost_since = None;
                        let bytes = match msg {
                            ClientMsg::Exit(code) => {
                                let _ = output.flush().await;
                                return Ok(code.unwrap_or(0));
                            }
                            ClientMsg::Restore(b) | ClientMsg::Output(b) => b,
                        };
                        if output.write_all(&bytes).await.is_err() || output.flush().await.is_err() {
                            leaving = true;
                            outbox.push_back(Input::Detach);
                            continue;
                        }
                        match outbox.back_mut() {
                            Some(Input::Ack(n)) => *n += bytes.len() as u64,
                            _ => outbox.push_back(Input::Ack(bytes.len() as u64)),
                        }
                    }
                    Some(CtlResponse::Error(e)) if e.code == ErrorCode::Unreachable && connected_once => break e,
                    Some(CtlResponse::Error(e)) if e.code == ErrorCode::NotFound && connected_once => {
                        let note = format!("\r\n[会话 {} 已经结束了：远端的程序在断开期间退出了]\r\n", args.session);
                        let _ = output.write_all(note.as_bytes()).await;
                        let _ = output.flush().await;
                        return Err(SshError::Remote(e));
                    }
                    Some(CtlResponse::Error(e)) => return Err(SshError::Remote(e)),
                    Some(_) => return Err(SshError::Remote(WireError::new(ErrorCode::Internal, "常驻程序的回应无法理解"))),
                    None => return Err(SshError::Remote(WireError::new(ErrorCode::Internal, "常驻程序提前结束了会话"))),
                },
                permit = session.tx.reserve(), if !outbox.is_empty() => match permit {
                    Ok(permit) => permit.send(outbox.pop_front().expect("队列非空")),
                    Err(_) => return Err(SshError::Remote(WireError::new(ErrorCode::Internal, "常驻程序提前结束了会话"))),
                },
                s = resizes.recv(), if resizes_open => match s {
                    Some(s) => {
                        size = s;
                        outbox.push_back(Input::Resize(s));
                    }
                    None => resizes_open = false,
                },
                n = input.read(&mut buf), if attached && !leaving && outbox.len() < 64 => {
                    let n = n.unwrap_or(0);
                    let data = &buf[..n];
                    let cut = data.iter().position(|&b| b == DETACH_KEY);
                    let typed = &data[..cut.unwrap_or(n)];
                    if !typed.is_empty() {
                        outbox.push_back(Input::Stdin(typed.to_vec()));
                    }
                    if n == 0 || cut.is_some() {
                        leaving = true;
                        outbox.push_back(Input::Detach);
                    }
                }
            }
        };
        // 连接中断：会话还在对方那里，过一会儿重新连上。
        let since = *lost_since.get_or_insert_with(tokio::time::Instant::now);
        if since.elapsed() >= RECONNECT_FOR {
            return Err(SshError::Remote(lost));
        }
        let _ = output.write_all("\r\n[连接中断，正在重连…]\r\n".as_bytes()).await;
        let _ = output.flush().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// ---------------------------------------------------------------- MCP

const MCP_PROTOCOL: &str = "2025-06-18";

fn mcp_tools() -> Value {
    let node = json!({ "type": "string", "description": "设备的名字或标识" });
    let drive_path = json!({ "type": "string", "description": "工作区路径，格式 <工作区>:/<路径>，例如 work:/src/main.rs" });
    let ws = json!({ "type": "string", "description": "工作区名" });
    json!([
        {
            "name": "nexus_ls",
            "description": "列出你的所有设备及其状态（是否在线、直连还是中继、往返时间）。",
            "inputSchema": { "type": "object", "properties": {} },
        },
        {
            "name": "nexus_status",
            "description": "查看一台设备的状态；不给 node 时为本机。",
            "inputSchema": { "type": "object", "properties": { "node": node } },
        },
        {
            "name": "nexus_services",
            "description": "列出一台设备上发布的、当前身份有权使用的服务。",
            "inputSchema": { "type": "object", "properties": { "node": node }, "required": ["node"] },
        },
        {
            "name": "nexus_exec",
            "description": "在一台设备上执行命令（不经过 shell），返回退出码、标准输出和标准错误。需要 shell 特性时写 [\"sh\", \"-c\", \"...\"]。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node": node,
                    "command": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "程序及其参数" },
                    "cwd": { "type": "string", "description": "工作目录（默认是远端用户的主目录）" },
                    "env": { "type": "array", "items": { "type": "string" }, "description": "追加的环境变量，每项格式 名字=值" },
                    "timeout_s": { "type": "number", "description": "超过这么多秒就终止命令（默认 300）" },
                },
                "required": ["node", "command"],
            },
        },
        {
            "name": "nexus_session_ls",
            "description": "列出一台设备上的常驻终端会话。",
            "inputSchema": { "type": "object", "properties": { "node": node }, "required": ["node"] },
        },
        {
            "name": "nexus_session_kill",
            "description": "结束一台设备上的常驻终端会话。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node": node,
                    "session": { "type": "string", "description": "终端会话名" },
                },
                "required": ["node", "session"],
            },
        },
        {
            "name": "nexus_drive_ls",
            "description": "列出所有工作区（不给 path），或列出工作区里的一个目录。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": drive_path,
                    "at": { "type": "string", "description": "可选：查看某次提交（提交号或至少 8 位前缀）" },
                },
            },
        },
        {
            "name": "nexus_drive_read",
            "description": "读取工作区里的文件内容（支持按字节范围读取）。UTF-8 文本返回 content，二进制返回 content_base64。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": drive_path,
                    "at": { "type": "string", "description": "可选：查看某次提交" },
                    "offset": { "type": "integer", "minimum": 0, "description": "从第几个字节开始（默认 0）" },
                    "length": { "type": "integer", "minimum": 0, "description": "读取字节数（默认读到末尾）" },
                },
                "required": ["path"],
            },
        },
        {
            "name": "nexus_drive_write",
            "description": "向工作区写入一个文件（新建或覆盖，中间目录自动建立）。content 与 content_base64 二选一。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": drive_path,
                    "content": { "type": "string", "description": "UTF-8 文本内容" },
                    "content_base64": { "type": "string", "description": "标准 Base64 编码的二进制内容" },
                    "exec": { "type": "boolean", "description": "是否设置可执行位（默认 false）" },
                },
                "required": ["path"],
            },
        },
        {
            "name": "nexus_drive_rm",
            "description": "删除工作区里的文件或目录。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": drive_path,
                    "recursive": { "type": "boolean", "description": "删除目录时设为 true" },
                },
                "required": ["path"],
            },
        },
        {
            "name": "nexus_drive_status",
            "description": "列出工作区里尚未提交的改动。",
            "inputSchema": { "type": "object", "properties": { "ws": ws }, "required": ["ws"] },
        },
        {
            "name": "nexus_drive_commit",
            "description": "把工作区里未提交的改动生成一次提交并同步到副本。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ws": ws,
                    "message": { "type": "string", "description": "提交说明" },
                },
                "required": ["ws"],
            },
        },
        {
            "name": "nexus_drive_where",
            "description": "查看工作区或路径的数据分布、副本列表、同步状态与本机缓存量。",
            "inputSchema": { "type": "object", "properties": { "path": drive_path }, "required": ["path"] },
        },
        {
            "name": "nexus_path",
            "description": "查看到其他设备的网络路径（本机地址、直连或中继、往返时间、收发字节数）；省略 device 时列出所有设备。",
            "inputSchema": { "type": "object", "properties": { "device": node } },
        },
        {
            "name": "nexus_route_explain",
            "description": "解释一个目标地址（主机:端口）经统一入口会被路由到哪里。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "主机:端口，例如 web.home.internal:80" },
                },
                "required": ["target"],
            },
        },
        {
            "name": "nexus_audit",
            "description": "查看本机的审计记录（经过本机的每次访问，包括被拒绝的）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "description": "只返回最近的这么多条" },
                },
            },
        },
    ])
}

fn mcp_b64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((n >> 6) & 0x3f) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 0x3f) as usize] as char } else { '=' });
    }
    out
}

fn mcp_b64_decode(input: &str) -> Result<Vec<u8>, &'static str> {
    let bytes: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !bytes.len().is_multiple_of(4) {
        return Err("Base64 长度必须是 4 的倍数");
    }
    let val = |b: u8| -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.as_chunks::<4>().0 {
        let a = val(chunk[0]).ok_or("无效的 Base64 字符")?;
        let b = val(chunk[1]).ok_or("无效的 Base64 字符")?;
        out.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            let c = val(chunk[2]).ok_or("无效的 Base64 字符")?;
            out.push((b << 4) | (c >> 2));
            if chunk[3] != b'=' {
                let d = val(chunk[3]).ok_or("无效的 Base64 字符")?;
                out.push((c << 6) | d);
            }
        } else if chunk[3] != b'=' {
            return Err("无效的 Base64 填充");
        }
    }
    Ok(out)
}

fn mcp_parse_err(sink: &Captured, code: i32) -> (Value, bool) {
    let err = String::from_utf8_lossy(&sink.stderr);
    let v = err.lines().last().and_then(|l| serde_json::from_str(l).ok()).unwrap_or_else(|| {
        json!({ "error": "internal", "message": err.trim(), "exit_code": code })
    });
    (v, true)
}

/// 把一次工具调用翻译成命令，执行，得到 (结构化结果, 是否出错)。
async fn mcp_call<C: Ctl>(ctl: &C, name: &str, args: &Value) -> (Value, bool) {
    let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);
    let flag = |key: &str| args.get(key).and_then(Value::as_bool).unwrap_or(false);
    let mut stdin_bytes: Option<Vec<u8>> = None;
    let mut raw_read_path: Option<String> = None;

    let cmd = match name {
        "nexus_ls" => Cmd::Ls { node: None, json: true },
        "nexus_status" => Cmd::Status { node: text("node"), json: true },
        "nexus_services" => match text("node") {
            Some(node) => Cmd::Ls { node: Some(node), json: true },
            None => return (json!({ "error": "usage", "message": "缺少 node" }), true),
        },
        "nexus_session_ls" => match text("node") {
            Some(node) => Cmd::Session { cmd: SessionCmd::Ls { node, json: true } },
            None => return (json!({ "error": "usage", "message": "缺少 node" }), true),
        },
        "nexus_session_kill" => match (text("node"), text("session")) {
            (Some(node), Some(session)) => Cmd::Session { cmd: SessionCmd::Kill { node, session, json: true } },
            _ => return (json!({ "error": "usage", "message": "需要 node 和 session" }), true),
        },
        "nexus_exec" => {
            let command: Option<Vec<String>> = args
                .get("command")
                .and_then(Value::as_array)
                .and_then(|a| a.iter().map(|v| v.as_str().map(str::to_string)).collect());
            let env: Vec<String> = match args.get("env") {
                None | Some(Value::Null) => vec![],
                Some(Value::Array(arr)) => match arr.iter().map(|v| v.as_str().map(str::to_string)).collect() {
                    Some(v) => v,
                    None => return (json!({ "error": "usage", "message": "env 必须是字符串数组" }), true),
                },
                Some(_) => return (json!({ "error": "usage", "message": "env 必须是字符串数组" }), true),
            };
            match (text("node"), command) {
                (Some(node), Some(command)) if !command.is_empty() => Cmd::Exec {
                    node,
                    timeout: Some(args.get("timeout_s").and_then(Value::as_f64).unwrap_or(300.0)),
                    max_output: DEFAULT_MAX_OUTPUT,
                    cwd: text("cwd"),
                    env,
                    json: true,
                    command,
                },
                _ => return (json!({ "error": "usage", "message": "需要 node 和非空的 command 数组" }), true),
            }
        }
        "nexus_drive_ls" => Cmd::Drive {
            cmd: drive::DriveCmd::Ls { path: text("path"), at: text("at"), json: true },
        },
        "nexus_drive_read" => match text("path") {
            Some(path) => {
                raw_read_path = Some(path.clone());
                Cmd::Drive {
                    cmd: drive::DriveCmd::Cat {
                        path,
                        at: text("at"),
                        offset: args.get("offset").and_then(Value::as_u64).unwrap_or(0),
                        length: args.get("length").and_then(Value::as_u64),
                    },
                }
            }
            None => return (json!({ "error": "usage", "message": "缺少 path" }), true),
        },
        "nexus_drive_write" => match (text("path"), text("content"), text("content_base64")) {
            (Some(path), Some(content), None) => {
                stdin_bytes = Some(content.into_bytes());
                Cmd::Drive { cmd: drive::DriveCmd::Put { path, exec: flag("exec"), json: true } }
            }
            (Some(path), None, Some(b64)) => match mcp_b64_decode(&b64) {
                Ok(bytes) => {
                    stdin_bytes = Some(bytes);
                    Cmd::Drive { cmd: drive::DriveCmd::Put { path, exec: flag("exec"), json: true } }
                }
                Err(msg) => return (json!({ "error": "usage", "message": msg }), true),
            },
            _ => return (json!({ "error": "usage", "message": "需要 path，且 content 与 content_base64 二选一" }), true),
        },
        "nexus_drive_rm" => match text("path") {
            Some(path) => Cmd::Drive { cmd: drive::DriveCmd::Rm { path, recursive: flag("recursive"), json: true } },
            None => return (json!({ "error": "usage", "message": "缺少 path" }), true),
        },
        "nexus_drive_status" => match text("ws") {
            Some(ws) => Cmd::Drive { cmd: drive::DriveCmd::Status { ws, json: true } },
            None => return (json!({ "error": "usage", "message": "缺少 ws" }), true),
        },
        "nexus_drive_commit" => match text("ws") {
            Some(ws) => Cmd::Drive {
                cmd: drive::DriveCmd::Commit { ws, message: text("message").unwrap_or_default(), json: true },
            },
            None => return (json!({ "error": "usage", "message": "缺少 ws" }), true),
        },
        "nexus_drive_where" => match text("path") {
            Some(path) => Cmd::Drive { cmd: drive::DriveCmd::Where { path, json: true } },
            None => return (json!({ "error": "usage", "message": "缺少 path" }), true),
        },
        "nexus_path" => Cmd::Path { device: text("device"), json: true },
        "nexus_route_explain" => match text("target") {
            Some(target) => Cmd::Route { cmd: RouteCmd::Explain { target, json: true } },
            None => return (json!({ "error": "usage", "message": "缺少 target" }), true),
        },
        "nexus_audit" => {
            let limit = args.get("limit").and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok());
            Cmd::Audit { limit, json: true }
        }
        _ => return (json!({ "error": "usage", "message": format!("没有工具 {name}") }), true),
    };
    let mut sink = Captured::default();
    // run_client 里也会调用到这里（nexus mcp），装箱以打断异步函数的递归。
    let code = if let Some(bytes) = stdin_bytes {
        let mut input = std::io::Cursor::new(bytes);
        Box::pin(run_client(&cmd, ctl, &mut input, &mut sink)).await
    } else {
        let mut empty = tokio::io::empty();
        Box::pin(run_client(&cmd, ctl, &mut empty, &mut sink)).await
    };
    if let Some(path) = raw_read_path {
        if code != 0 {
            return mcp_parse_err(&sink, code);
        }
        let size = sink.stdout.len();
        let value = match std::str::from_utf8(&sink.stdout) {
            Ok(content) if !sink.stdout.contains(&0) => json!({
                "schema": JSON_SCHEMA,
                "path": path,
                "size": size,
                "encoding": "utf-8",
                "content": content,
            }),
            _ => json!({
                "schema": JSON_SCHEMA,
                "path": path,
                "size": size,
                "encoding": "base64",
                "content_base64": mcp_b64_encode(&sink.stdout),
            }),
        };
        return (value, false);
    }
    let out = String::from_utf8_lossy(&sink.stdout);
    match serde_json::from_str::<Value>(out.trim()) {
        // 执行类命令即使远端命令失败也有完整的结果；那不是工具出错。
        Ok(v) if v.get("schema").is_some() => (v, false),
        _ => mcp_parse_err(&sink, code),
    }
}

/// `nexus mcp`：在标准输入输出上按行读写 JSON-RPC。
async fn mcp<C: Ctl, R: AsyncRead + Unpin, K: Sink>(ctl: &C, stdin: &mut R, sink: &mut K) -> i32 {
    let mut lines = tokio::io::BufReader::new(stdin).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Err(e) => Some(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": e.to_string() } })),
            Ok(req) => {
                let id = req.get("id").cloned();
                let method = req.get("method").and_then(Value::as_str).unwrap_or_default();
                let params = req.get("params").cloned().unwrap_or(Value::Null);
                let result: Result<Value, (i64, String)> = match method {
                    "initialize" => Ok(json!({
                        "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or(MCP_PROTOCOL),
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "nexus", "version": env!("CARGO_PKG_VERSION") },
                    })),
                    "ping" => Ok(json!({})),
                    "tools/list" => Ok(json!({ "tools": mcp_tools() })),
                    "tools/call" => {
                        let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
                        let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                        let (value, is_error) = mcp_call(ctl, name, &args).await;
                        Ok(json!({
                            "content": [{ "type": "text", "text": value.to_string() }],
                            "structuredContent": value,
                            "isError": is_error,
                        }))
                    }
                    other => Err((-32601, format!("不支持的方法 {other}"))),
                };
                // 通知（没有 id）不回应。
                id.map(|id| match result {
                    Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
                    Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
                })
            }
        };
        if let Some(reply) = reply {
            sink.out(format!("{reply}\n").as_bytes());
        }
    }
    0
}

/// 打开到服务的一条连接：等常驻程序确认，返回会话。
async fn open_connect<C: Ctl>(ctl: &C, node: &str, name: &str) -> Result<CtlClient, (i32, &'static str, String)> {
    let mut session = ctl
        .open(CtlRequest::Connect { node: node.to_string(), name: name.to_string() })
        .await
        .map_err(|e| (EXIT_DAEMON_UNREACHABLE, "daemon_unreachable", format!("连不上本机的常驻程序（{e}）{DAEMON_HINT}")))?;
    match session.rx.recv().await {
        Some(CtlResponse::Ok) => Ok(session),
        Some(CtlResponse::Error(e)) => {
            let (code, kind) = exit_for(e.code);
            Err((code, kind, e.message))
        }
        _ => Err((EXIT_INTERNAL, "internal", "常驻程序的回应无法理解".to_string())),
    }
}

/// 在一条到服务的连接上搬运字节：`input` 的数据送到服务，服务的数据写到 `output`，直到两个方向都结束。
async fn pipe<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(session: &mut CtlClient, input: &mut R, output: &mut W) {
    let mut buf = vec![0u8; 64 * 1024];
    let (mut reading, mut writing) = (true, true);
    let mut pending: Option<Input> = None;
    while reading {
        tokio::select! {
            biased;
            resp = session.rx.recv() => match resp {
                Some(CtlResponse::PipeData(b)) => {
                    if output.write_all(&b).await.is_err() {
                        break;
                    }
                    let _ = output.flush().await;
                }
                _ => {
                    reading = false;
                    let _ = output.shutdown().await;
                }
            },
            permit = session.tx.reserve(), if pending.is_some() => match permit {
                Ok(p) => p.send(pending.take().expect("有待发的输入")),
                Err(_) => {
                    pending = None;
                    writing = false;
                }
            },
            n = input.read(&mut buf), if writing && pending.is_none() => {
                pending = Some(match n {
                    Ok(0) | Err(_) => {
                        writing = false;
                        Input::CloseStdin
                    }
                    Ok(n) => Input::Stdin(buf[..n].to_vec()),
                });
            }
        }
    }
}

/// `nexus connect <设备>/<服务>`：标准输入输出接到服务上。
async fn connect_stdio<C: Ctl, R: AsyncRead + Unpin, K: Sink>(ctl: &C, node: &str, name: &str, stdin: &mut R, sink: &mut K) -> i32 {
    let mut session = match open_connect(ctl, node, name).await {
        Ok(s) => s,
        Err((code, kind, msg)) => return fail(sink, code, kind, &msg),
    };
    let mut out = SinkWriter(sink);
    pipe(&mut session, stdin, &mut out).await;
    0
}

/// `nexus connect <设备>/<服务> --listen <地址>`：本机每个连接各自接到服务，直到被中断。
async fn connect_listen<C: Ctl, K: Sink>(ctl: &C, node: &str, name: &str, listen: &str, sink: &mut K) -> i32 {
    let addr: std::net::SocketAddr = match listen.parse() {
        Ok(a) => a,
        Err(_) => return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("--listen 应当是 地址:端口：{listen}")),
    };
    // 先确认服务能用，再开始监听：名字写错时立即报错。
    match open_connect(ctl, node, name).await {
        Ok(probe) => drop(probe.tx.try_send(Input::CloseStdin)),
        Err((code, kind, msg)) => return fail(sink, code, kind, &msg),
    }
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("无法监听 {addr}：{e}")),
    };
    let local = listener.local_addr().map(|a| a.to_string()).unwrap_or_else(|_| addr.to_string());
    sink.out(format!("{node}/{name} → {local}（按 Ctrl-C 停止）\n").as_bytes());
    forward_forever(ctl, &listener, node, name).await
}

/// `nexus open`：服务的网址。给了路径就用它；否则用服务说明里第一个 `http://{addr}` 开头的值；都没有就是根路径。
fn open_url(addr: &str, path: Option<&str>, env: &[(String, String)]) -> String {
    if let Some(p) = path {
        return format!("http://{addr}/{p}");
    }
    env.iter()
        .find(|(_, v)| v.starts_with("http://{addr}"))
        .map(|(_, v)| v.replace("{addr}", addr))
        .unwrap_or_else(|| format!("http://{addr}/"))
}

/// 用 `$BROWSER` 或系统默认的浏览器打开网址。
fn launch_browser(url: &str) -> io::Result<()> {
    let mut cmd = match std::env::var("BROWSER").ok().filter(|b| !b.trim().is_empty()) {
        Some(b) => std::process::Command::new(b),
        None if cfg!(target_os = "macos") => std::process::Command::new("open"),
        None if cfg!(windows) => {
            let mut c = std::process::Command::new("cmd");
            c.args(["/c", "start", ""]);
            c
        }
        None => std::process::Command::new("xdg-open"),
    };
    cmd.arg(url).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    let mut child = cmd.spawn()?;
    // 不等它：有的浏览器一直不退出。退出了就收尾，避免留下僵尸进程。
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// `nexus open <设备>/<服务>[/<路径>]`：本机开端口接到网页服务，打开浏览器，直到被中断。
async fn open_in_browser<C: Ctl, K: Sink>(ctl: &C, target: &str, port: Option<u16>, browser: bool, sink: &mut K) -> i32 {
    let mut parts = target.splitn(3, '/');
    let (node, name, path) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next());
    if node.is_empty() || name.is_empty() {
        return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("要打开的应当写成 <设备>/<服务>[/<路径>]：{target}"));
    }
    let info = match service_info(ctl, node, name).await {
        Ok(info) => info,
        Err((code, kind, msg)) => return fail(sink, code, kind, &msg),
    };
    if desktop::is_desktop(&info.proto) {
        if path.is_some() {
            return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("远程桌面没有路径：nexus open {node}/{name}"));
        }
        return desktop::serve(ctl, node, name, &info.proto, port, browser, sink).await;
    }
    if info.proto != "http" {
        let msg = format!("{node}/{name} 不是网页服务（协议 {}）：用 nexus connect {node}/{name} 连接它", info.proto);
        return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &msg);
    }
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port.unwrap_or(0))).await {
        Ok(l) => l,
        Err(e) => return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("无法在本机开端口 {}：{e}", port.unwrap_or(0))),
    };
    let addr = match listener.local_addr() {
        Ok(a) => a.to_string(),
        Err(e) => return fail(sink, EXIT_INTERNAL, "internal", &format!("无法在本机开端口：{e}")),
    };
    let url = open_url(&addr, path, &info.env);
    sink.out(format!("{node}/{name} → {url}（按 Ctrl-C 停止）\n").as_bytes());
    if browser {
        if let Err(e) = launch_browser(&url) {
            sink.err(format!("打不开浏览器（{e}）：在浏览器里手动打开上面的网址\n").as_bytes());
        }
    }
    forward_forever(ctl, &listener, node, name).await
}

/// 先确认服务能用（存在、有授权、发布方的程序在运行），再读它的说明。
async fn service_info<C: Ctl>(ctl: &C, node: &str, name: &str) -> Result<nexus_wire::ServiceInfo, (i32, &'static str, String)> {
    let probe = open_connect(ctl, node, name).await?;
    drop(probe.tx.try_send(Input::CloseStdin));
    let info = match call(ctl, CtlRequest::Services { node: node.to_string() }).await {
        Ok(CtlResponse::Services { services, .. }) => services.into_iter().find(|s| s.name == name),
        Ok(CtlResponse::Error(e)) => {
            let (code, kind) = exit_for(e.code);
            return Err((code, kind, e.message));
        }
        _ => None,
    };
    info.ok_or_else(|| (EXIT_NOT_FOUND, "not_found", format!("{node} 上没有你能用的服务 {name}")))
}

/// 本机端口上的每个连接各自接到服务，直到被中断。
async fn forward_forever<C: Ctl>(ctl: &C, listener: &tokio::net::TcpListener, node: &str, name: &str) -> i32 {
    let conns = futures_util::stream::FuturesUnordered::new();
    tokio::pin!(conns);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((socket, _)) = accepted else { continue };
                conns.push(async move {
                    let Ok(mut session) = open_connect(ctl, node, name).await else { return };
                    let (mut rd, mut wr) = socket.into_split();
                    pipe(&mut session, &mut rd, &mut wr).await;
                });
            }
            Some(()) = futures_util::StreamExt::next(&mut conns), if !conns.is_empty() => {}
        }
    }
}

/// 出口在 JSON 里的样子：`outbound`，出口设备另有 `via`，上游代理另有 `upstream`。
fn outbound_json(o: &route::Outbound, mut v: Value) -> Value {
    let (key, extra) = match o {
        route::Outbound::Direct => ("direct", None),
        route::Outbound::Reject => ("reject", None),
        route::Outbound::Exit(device) => ("exit", Some(("via", device))),
        route::Outbound::Socks5(upstream) => ("socks5", Some(("upstream", upstream))),
    };
    v["outbound"] = json!(key);
    if let Some((k, x)) = extra {
        v[k] = json!(x);
    }
    v
}

/// `nexus run`：为每个用到的服务在本机开一个临时端口，设置环境变量，运行程序，程序退出后关闭端口。
async fn run_with_services<C: Ctl, K: Sink>(ctl: &C, uses: &[String], command: &[String], sink: &mut K) -> i32 {
    let mut env: Vec<(String, String)> = Vec::new();
    let mut listeners = Vec::new();
    for target in uses {
        let Some((node, name)) = target.split_once('/').filter(|(n, s)| !n.is_empty() && !s.is_empty()) else {
            return fail(sink, EXIT_INVALID_INPUT, "invalid_input", &format!("服务的路径应为 <设备>/<服务>：{target}"));
        };
        // 先确认服务能用（存在、有授权、本机程序在运行），再读它的说明。
        match open_connect(ctl, node, name).await {
            Ok(probe) => drop(probe.tx.try_send(Input::CloseStdin)),
            Err((code, kind, msg)) => return fail(sink, code, kind, &msg),
        }
        let info = match call(ctl, CtlRequest::Services { node: node.to_string() }).await {
            Ok(CtlResponse::Services { services, .. }) => services.into_iter().find(|s| s.name == name),
            Ok(CtlResponse::Error(e)) => return fail_wire(sink, &e),
            _ => None,
        };
        let Some(info) = info else {
            return fail(sink, EXIT_NOT_FOUND, "not_found", &format!("{node} 上没有你能用的服务 {name}"));
        };
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(e) => return fail(sink, EXIT_INTERNAL, "internal", &format!("无法在本机开端口：{e}")),
        };
        let addr = match listener.local_addr() {
            Ok(a) => a.to_string(),
            Err(e) => return fail(sink, EXIT_INTERNAL, "internal", &format!("无法在本机开端口：{e}")),
        };
        let var = format!("NEXUS_SVC_{}_{}", env_key(node), env_key(name));
        env.push((var, addr.clone()));
        for (k, v) in &info.env {
            env.push((k.clone(), v.replace("{addr}", &addr)));
        }
        listeners.push((listener, node.to_string(), name.to_string()));
    }
    let mut child = match tokio::process::Command::new(&command[0]).args(&command[1..]).envs(env).spawn() {
        Ok(c) => c,
        Err(e) => return fail(sink, EXIT_SPAWN_FAILED, "spawn_failed", &format!("无法启动 {}：{e}", command[0])),
    };
    let serve = futures_util::future::join_all(listeners.iter().map(|(listener, node, name)| async move {
        let conns = futures_util::stream::FuturesUnordered::new();
        tokio::pin!(conns);
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((socket, _)) = accepted else { continue };
                    conns.push(async move {
                        let Ok(mut session) = open_connect(ctl, node, name).await else { return };
                        let (mut rd, mut wr) = socket.into_split();
                        pipe(&mut session, &mut rd, &mut wr).await;
                    });
                }
                Some(()) = futures_util::StreamExt::next(&mut conns), if !conns.is_empty() => {}
            }
        }
    }));
    tokio::select! {
        status = child.wait() => match status {
            Ok(s) => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0))
                }
                #[cfg(windows)]
                {
                    s.code().unwrap_or(1)
                }
            }
            Err(e) => fail(sink, EXIT_INTERNAL, "internal", &format!("等待程序结束时出错：{e}")),
        },
        _ = serve => EXIT_INTERNAL,
    }
}

/// 环境变量名里用的形式：大写，其他字符换成 _。
fn env_key(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect()
}

/// `nexus exec`：把本机标准输入送过去，把输出实时写出（或在 `--json` 时最后一次性输出）。
async fn exec<C: Ctl, R: AsyncRead + Unpin, K: Sink>(ctl: &C, req: CtlRequest, json: bool, stdin: &mut R, sink: &mut K) -> i32 {
    let mut session = match ctl.open(req).await {
        Ok(s) => s,
        Err(e) => {
            return fail(sink, EXIT_DAEMON_UNREACHABLE, "daemon_unreachable", &format!("连不上本机的常驻程序（{e}）{DAEMON_HINT}"))
        }
    };
    let mut collected = Captured::default();
    let mut buf = vec![0u8; 64 * 1024];
    let mut stdin_open = true;
    let mut pending: Option<Input> = None;
    let result: ExecResult = loop {
        tokio::select! {
            biased;
            resp = session.rx.recv() => match resp {
                Some(CtlResponse::Exec(ExecEvent::Stdout(b))) => if json { collected.out(&b) } else { sink.out(&b) },
                Some(CtlResponse::Exec(ExecEvent::Stderr(b))) => if json { collected.err(&b) } else { sink.err(&b) },
                Some(CtlResponse::Exec(ExecEvent::Exit(r))) => break r,
                Some(CtlResponse::Error(e)) => return fail_wire(sink, &e),
                Some(_) => return fail(sink, EXIT_INTERNAL, "internal", "常驻程序的回应无法理解"),
                None => return fail(sink, EXIT_INTERNAL, "internal", "常驻程序提前结束了会话"),
            },
            // 有空位才发：从不因为发送而停下，总能继续读输出，两边不会互相等待。
            permit = session.tx.reserve(), if pending.is_some() => match permit {
                Ok(permit) => permit.send(pending.take().expect("有待发的输入")),
                Err(_) => {
                    pending = None;
                    stdin_open = false;
                }
            },
            n = stdin.read(&mut buf), if stdin_open && pending.is_none() => {
                pending = Some(match n {
                    Ok(0) | Err(_) => {
                        stdin_open = false;
                        Input::CloseStdin
                    }
                    Ok(n) => Input::Stdin(buf[..n].to_vec()),
                });
            }
        }
    };
    if json {
        let (stdout, stdout_b64) = text_or_base64(&collected.stdout);
        let (stderr, stderr_b64) = text_or_base64(&collected.stderr);
        let v = json!({
            "schema": JSON_SCHEMA,
            "exit_code": result.exit_code,
            "signal": result.signal,
            "timed_out": result.timed_out,
            "truncated": result.truncated,
            "duration_ms": result.duration_ms,
            "stdout": stdout,
            "stdout_base64": stdout_b64,
            "stderr": stderr,
            "stderr_base64": stderr_b64,
        });
        sink.out(doc(v).as_bytes());
    }
    exec_exit_code(&result, json, sink)
}

fn exec_exit_code<K: Sink>(r: &ExecResult, json: bool, sink: &mut K) -> i32 {
    let note = |sink: &mut K, kind: &str, msg: String| {
        if !json {
            fail(sink, 0, kind, &msg);
        }
    };
    if r.timed_out {
        note(sink, "timed_out", format!("命令在 {} 毫秒后因超时被终止", r.duration_ms));
        return EXIT_TIMED_OUT;
    }
    if r.truncated {
        note(sink, "truncated", "命令的输出超过上限，已被终止；上面的输出不完整".to_string());
        return EXIT_TRUNCATED;
    }
    match (r.exit_code, r.signal) {
        (Some(c), _) => c,
        (None, Some(s)) => 128 + s,
        (None, None) => EXIT_INTERNAL,
    }
}

/// 合法 UTF-8 时给出文本，否则给出 base64。
fn text_or_base64(bytes: &[u8]) -> (Option<String>, Option<String>) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (Some(s.to_string()), None),
        Err(_) => (None, Some(base64(bytes))),
    }
}

/// 标准 base64（带填充）。
pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}


fn doc(v: Value) -> String {
    format!("{v}\n")
}

fn path_str(p: Option<PathKind>) -> Option<&'static str> {
    p.map(|p| match p {
        PathKind::Direct => "direct",
        PathKind::Relay => "relay",
    })
}

fn role_str(r: Option<DeviceRole>) -> Option<&'static str> {
    r.map(|r| match r {
        DeviceRole::Admin => "admin",
        DeviceRole::Member => "member",
    })
}

fn report_fields(r: &StatusReport) -> serde_json::Map<String, Value> {
    let v = json!({
        "name": r.name,
        "node_id": r.node_id.to_string(),
        "online": r.online,
        "role": role_str(r.role),
        "roster_version": r.info.as_ref().and_then(|i| i.roster_version),
        "version": r.info.as_ref().map(|i| i.version.clone()),
        "started_at_ms": r.info.as_ref().map(|i| i.started_at.0),
        "uptime_ms": r.info.as_ref().map(|i| i.now.millis_since(i.started_at)),
        "rtt_ms": r.rtt_ms,
        "path": path_str(r.path),
    });
    match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

fn status_json(r: &StatusReport) -> Value {
    let mut m = report_fields(r);
    m.insert("schema".into(), json!(JSON_SCHEMA));
    Value::Object(m)
}

/// 本机常驻程序的版本：列表里本机那一项的。
fn own_version(entries: &[ListEntry]) -> Option<&str> {
    entries.iter().find(|e| e.is_self).and_then(|e| e.report.info.as_ref()).map(|i| i.version.as_str())
}

/// 这台设备的版本和本机不同（M7.1）。不在线、不知道版本时为 false。
fn version_differs(e: &ListEntry, own: Option<&str>) -> bool {
    match (e.report.info.as_ref(), own) {
        (Some(i), Some(own)) => !e.is_self && i.version != own,
        _ => false,
    }
}

fn list_json(entries: &[ListEntry]) -> String {
    let own = own_version(entries);
    let devices: Vec<Value> = entries
        .iter()
        .map(|e| {
            let mut m = report_fields(&e.report);
            m.insert("self".into(), json!(e.is_self));
            m.insert("version_differs".into(), json!(version_differs(e, own)));
            Value::Object(m)
        })
        .collect();
    doc(json!({ "schema": JSON_SCHEMA, "devices": devices }))
}

fn status_text(r: &StatusReport) -> String {
    let mut out = format!("{}  {}", r.name, r.node_id.short());
    if let Some(role) = r.role {
        out.push_str(if role == DeviceRole::Admin { "  管理设备" } else { "  成员" });
    }
    out.push('\n');
    if !r.online {
        out.push_str("  离线\n");
        return out;
    }
    out.push_str("  在线");
    match path_str(r.path) {
        Some("direct") => out.push_str("，直连"),
        Some(_) => out.push_str("，中继"),
        None => {}
    }
    if let Some(ms) = r.rtt_ms {
        out.push_str(&format!("，往返 {ms} 毫秒"));
    }
    out.push('\n');
    if let Some(i) = &r.info {
        out.push_str(&format!("  版本 {}", i.version));
        if let Some(v) = i.roster_version {
            out.push_str(&format!("，名册版本 {v}"));
        } else {
            out.push_str("，尚未加入网络");
        }
        out.push('\n');
    }
    out
}

fn list_text(entries: &[ListEntry]) -> String {
    let mut out = String::new();
    let own = own_version(entries);
    for e in entries {
        let r = &e.report;
        let state = if !r.online {
            "离线".to_string()
        } else if e.is_self {
            "本机".to_string()
        } else {
            let path = if path_str(r.path) == Some("relay") { "中继" } else { "直连" };
            match r.rtt_ms {
                Some(ms) => format!("在线，{path}，{ms} 毫秒"),
                None => format!("在线，{path}"),
            }
        };
        let role = match r.role {
            Some(DeviceRole::Admin) => "管理设备",
            Some(DeviceRole::Member) => "成员",
            None => "未加入网络",
        };
        let version = match &r.info {
            Some(i) if version_differs(e, own) => format!("，版本 {}（和本机不同）", i.version),
            _ => String::new(),
        };
        out.push_str(&format!("{:<16} {}  {:<8} {state}{version}\n", r.name, r.node_id.short(), role));
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn open_url_prefers_path_then_template_then_root() {
        let env = vec![("TOKEN".to_string(), "x".to_string()), ("BASE_URL".to_string(), "http://{addr}/v1".to_string())];
        assert_eq!(super::open_url("127.0.0.1:9", Some("admin?tab=2"), &env), "http://127.0.0.1:9/admin?tab=2");
        assert_eq!(super::open_url("127.0.0.1:9", None, &env), "http://127.0.0.1:9/v1");
        assert_eq!(super::open_url("127.0.0.1:9", None, &[]), "http://127.0.0.1:9/");
    }

    use super::*;

    /// FP-7.1-04：版本不同的设备标出来；本机、离线的不标。
    #[test]
    fn list_marks_devices_on_another_version() {
        let entry = |name: &str, version: Option<&str>, is_self: bool| ListEntry {
            report: StatusReport {
                name: name.into(),
                node_id: nexus_core_proto::NodeId([name.len() as u8; 32]),
                info: version.map(|v| nexus_core_proto::NodeInfo {
                    node_id: nexus_core_proto::NodeId([0; 32]),
                    name: name.into(),
                    version: v.into(),
                    started_at: nexus_core_proto::Timestamp(0),
                    now: nexus_core_proto::Timestamp(0),
                    roster_version: Some(1),
                }),
                online: version.is_some(),
                rtt_ms: Some(3),
                path: Some(PathKind::Direct),
                role: Some(DeviceRole::Member),
            },
            is_self,
        };
        let entries = vec![entry("laptop", Some("0.2.0"), true), entry("home", Some("0.1.0"), false), entry("office", Some("0.2.0"), false), entry("nas", None, false)];
        let text = list_text(&entries);
        assert!(text.lines().any(|l| l.starts_with("home") && l.contains("版本 0.1.0（和本机不同）")), "{text}");
        assert!(!text.lines().any(|l| (l.starts_with("office") || l.starts_with("laptop") || l.starts_with("nas")) && l.contains("和本机不同")), "{text}");
        let v: Value = serde_json::from_str(&list_json(&entries)).unwrap();
        let differs: Vec<bool> = v["devices"].as_array().unwrap().iter().map(|d| d["version_differs"].as_bool().unwrap()).collect();
        assert_eq!(differs, [false, true, false, false]);
    }

    #[test]
    fn base64_matches_rfc4648_examples() {
        for (input, want) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(input.as_bytes()), want);
        }
        assert_eq!(base64(&[0xff, 0x00, b'x']), "/wB4");
    }

    #[test]
    fn shares_formatters_report_empty_and_all_states() {
        assert_eq!(shares_text(&[], Timestamp(0)), "没有临时分享。\n");
        let empty_json: Value = serde_json::from_str(&shares_json(&[], Timestamp(0))).unwrap();
        assert_eq!(empty_json, json!({ "shares": [] }));

        let now = Timestamp(5_000);
        let record = |id: &str, expires_at: u64, max_uses: Option<u32>, uses: u32, revoked: bool| ShareRecord {
            id: id.into(),
            target: nexus_wire::ShareTarget::Service { name: "web".into() },
            created_at: Timestamp(1_000),
            expires_at: Timestamp(expires_at),
            max_uses,
            uses,
            revoked,
        };
        let list = vec![
            record("s_active", 9_000, None, 0, false),
            record("s_expired", 5_000, Some(3), 1, false),
            record("s_exhausted", 9_000, Some(2), 2, false),
            record("s_revoked", 9_000, None, 0, true),
            record("s_rev_exp", 4_000, Some(1), 1, true),
        ];

        let text = shares_text(&list, now);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], "s_active  service  已用 0（不限次数）  有效");
        assert_eq!(lines[1], "s_expired  service  已用 1/3  已过期");
        assert_eq!(lines[2], "s_exhausted  service  已用 2/2  已用尽");
        assert_eq!(lines[3], "s_revoked  service  已用 0（不限次数）  已撤销");
        assert_eq!(lines[4], "s_rev_exp  service  已用 1/1  已撤销");

        let v: Value = serde_json::from_str(&shares_json(&list, now)).unwrap();
        let shares = v["shares"].as_array().unwrap();
        assert_eq!(shares.len(), 5);
        assert_eq!(
            shares[0],
            json!({
                "id": "s_active",
                "kind": "service",
                "created_at_ms": 1_000,
                "expires_at_ms": 9_000,
                "max_uses": null,
                "uses": 0,
                "revoked": false,
                "expired": false,
                "state": "active",
            })
        );
        assert_eq!((shares[1]["revoked"].as_bool(), shares[1]["expired"].as_bool(), shares[1]["state"].as_str()), (Some(false), Some(true), Some("expired")));
        assert_eq!((shares[2]["revoked"].as_bool(), shares[2]["expired"].as_bool(), shares[2]["state"].as_str()), (Some(false), Some(false), Some("exhausted")));
        assert_eq!((shares[3]["revoked"].as_bool(), shares[3]["expired"].as_bool(), shares[3]["state"].as_str()), (Some(true), Some(false), Some("revoked")));
        assert_eq!((shares[4]["revoked"].as_bool(), shares[4]["expired"].as_bool(), shares[4]["state"].as_str()), (Some(true), Some(true), Some("revoked")));
    }
}
