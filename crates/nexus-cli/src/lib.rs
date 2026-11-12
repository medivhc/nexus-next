//! 命令行的客户端部分：解析参数、向常驻程序发起控制调用、格式化输出。
//!
//! 这里的入口函数不接触进程、终端或网络，由调用方注入控制通道，
//! 因此仿真测试可以直接调用它，行为与真实的 `nexus` 二进制一致。
#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use nexus_core_proto::roster::DeviceRole;
use nexus_core_proto::PathKind;
use nexus_runtime::CtlClient;
use nexus_wire::{
    AgentCredential, AgentInfo, AuditRecord, ClientMsg, CtlRequest, CtlResponse, ErrorCode, ExecEvent, ExecResult, ExecSpec, Input, ListEntry, SessionInfo, Size,
    StatusReport, WireError,
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

pub const EXIT_DAEMON_UNREACHABLE: i32 = 101;
pub const EXIT_USAGE: i32 = 102;
pub const EXIT_UNKNOWN_NODE: i32 = 103;
pub const EXIT_INTERNAL: i32 = 104;
pub const EXIT_DENIED: i32 = 105;
pub const EXIT_INVALID_STATE: i32 = 106;
pub const EXIT_INVALID_INPUT: i32 = 107;
pub const EXIT_UNREACHABLE: i32 = 108;
pub const EXIT_NOT_FOUND: i32 = 109;
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
        /// 中继：default（公共中继）、disabled，或自建中继的地址
        #[arg(long, default_value = "default")]
        relay: String,
        /// 不使用公共地址发现服务
        #[arg(long)]
        no_discovery: bool,
    },
    /// 在第一台设备上建立你的网络
    Init {
        /// 这台设备在网络中的名字
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 让这台设备准备加入网络：输出一段加入码，在管理设备上用 `nexus device approve` 批准
    Join {
        #[arg(long)]
        json: bool,
    },
    /// 管理名册里的设备（只能在管理设备上执行）
    Device {
        #[command(subcommand)]
        cmd: DeviceCmd,
    },
    /// 列出你的所有设备及其状态
    Ls {
        #[arg(long)]
        json: bool,
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
    /// 提供工具 nexus_ls、nexus_status、nexus_exec、nexus_session_ls。
    /// 带 --agent 或 NEXUS_AGENT 时，所有工具都以该 agent 的身份执行。
    Mcp,
    /// 输出一段 ~/.ssh/config 配置，让系统自带的 ssh、scp、git 能用 <设备名>.nexus 访问设备
    ///
    /// 用法：nexus ssh-config >> ~/.ssh/config，之后 ssh home.nexus、scp f home.nexus:/tmp/、
    /// git clone home.nexus:/path/repo 都能用，不需要任何密钥。交互式登录进入常驻会话 main。
    SshConfig,
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
}

#[derive(Subcommand, Debug)]
pub enum DeviceCmd {
    /// 批准一台设备加入
    Approve {
        /// 新设备上 `nexus join` 输出的加入码
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
pub enum AgentCmd {
    /// 签发一份 agent 凭据
    ///
    /// 例：nexus agent issue --label builder --grant term.exec@home --grant 'term.session.*@home/session/build' --ttl 4h
    ///
    /// 能力：node.info、term.exec、term.session.attach、term.session.list、term.session.kill，
    /// 末尾的星号匹配一组能力（term.session.*）。资源：设备名，或 设备名/session/会话名；
    /// 一段写成星号时匹配任意一段，末尾的两个星号匹配其下的一切。
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

fn fail<K: Sink>(sink: &mut K, code: i32, kind: &str, message: &str) -> i32 {
    let line = json!({ "error": kind, "message": message });
    sink.err(format!("{line}\n").as_bytes());
    code
}

fn fail_wire<K: Sink>(sink: &mut K, e: &nexus_wire::WireError) -> i32 {
    let (code, kind) = match e.code {
        ErrorCode::UnknownNode => (EXIT_UNKNOWN_NODE, "unknown_node"),
        ErrorCode::Denied => (EXIT_DENIED, "denied"),
        ErrorCode::InvalidState => (EXIT_INVALID_STATE, "invalid_state"),
        ErrorCode::InvalidInput => (EXIT_INVALID_INPUT, "invalid_input"),
        ErrorCode::Unreachable => (EXIT_UNREACHABLE, "unreachable"),
        ErrorCode::SpawnFailed => (EXIT_SPAWN_FAILED, "spawn_failed"),
        ErrorCode::NotFound => (EXIT_NOT_FOUND, "not_found"),
        _ => (EXIT_INTERNAL, "internal"),
    };
    fail(sink, code, kind, &e.message)
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

async fn call<C: Ctl>(ctl: &C, req: CtlRequest) -> io::Result<CtlResponse> {
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
        Cmd::Status { node: Some(n), json } => (CtlRequest::RemoteStatus { node: n.clone() }, *json),
        Cmd::Init { name, json } => (CtlRequest::Init { name: name.clone() }, *json),
        Cmd::Join { json } => (CtlRequest::JoinStart, *json),
        Cmd::Device { cmd: DeviceCmd::Approve { code, name, json } } => {
            (CtlRequest::Approve { code: code.clone(), name: name.clone() }, *json)
        }
        Cmd::Device { cmd: DeviceCmd::Revoke { name, json } } => (CtlRequest::Revoke { name: name.clone() }, *json),
        Cmd::Ls { json } => (CtlRequest::List, *json),
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
        Cmd::SshConfig | Cmd::SshProxy { .. } => {
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
        CtlResponse::Exec(_) | CtlResponse::Session(_) | CtlResponse::SshHostKey(_) => {
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
                format!("已签发 agent {}（{}），有效期至 {} 毫秒时间戳。\n{how}", r.label, &agent_id[..8], r.not_after.0)
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
                s
            }
        }
        CtlResponse::Agents(list) => if json { agents_json(&list) } else { agents_text(&list) },
        CtlResponse::Audit(records) => if json { audit_json(&records) } else { audit_text(&records) },
        CtlResponse::Ok => match cmd {
            Cmd::Session { cmd: SessionCmd::Kill { session, .. } } => {
                if json {
                    doc(json!({ "schema": JSON_SCHEMA, "killed": session }))
                } else {
                    format!("已结束会话 {session}\n")
                }
            }
            _ => String::new(),
        },
        CtlResponse::Status(r) => if json { doc(status_json(&r)) } else { status_text(&r) },
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
                    "已建立网络，本机 {} 是管理设备。\n\n恢复码（请离线保存，所有设备都丢失时用它恢复；它只显示这一次）：\n  {}\n\n在另一台设备上运行 `nexus join`，再回到这里用 `nexus device approve <加入码>` 批准它。\n",
                    r.name, r.recovery_code
                )
            }
        }
        CtlResponse::JoinCode { code, expires_at } => if json {
            doc(json!({ "schema": JSON_SCHEMA, "code": code, "expires_at_ms": expires_at.0 }))
        } else {
            format!("加入码（10 分钟内有效）：\n\n{code}\n\n在管理设备上运行：nexus device approve <加入码>\n")
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
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(text.as_bytes())?;
    f.write_all(b"\n")
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
            };
            let outcome = match &e.outcome {
                Outcome::Allow => "允许".to_string(),
                Outcome::Deny(reason) => format!("拒绝：{}", deny_kind(reason)),
            };
            format!("{}  {}  {} {}  {}\n", e.at.0, who, e.capability, e.resource, outcome)
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
        let req = CtlRequest::Attach { node: args.node.clone(), session: args.session.clone(), size };
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
            "name": "nexus_exec",
            "description": "在一台设备上执行命令（不经过 shell），返回退出码、标准输出和标准错误。需要 shell 特性时写 [\"sh\", \"-c\", \"...\"]。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node": node,
                    "command": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "程序及其参数" },
                    "cwd": { "type": "string", "description": "工作目录（默认是远端用户的主目录）" },
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
    ])
}

/// 把一次工具调用翻译成命令，执行，得到 (结构化结果, 是否出错)。
async fn mcp_call<C: Ctl>(ctl: &C, name: &str, args: &Value) -> (Value, bool) {
    let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);
    let cmd = match name {
        "nexus_ls" => Cmd::Ls { json: true },
        "nexus_status" => Cmd::Status { node: text("node"), json: true },
        "nexus_session_ls" => match text("node") {
            Some(node) => Cmd::Session { cmd: SessionCmd::Ls { node, json: true } },
            None => return (json!({ "error": "usage", "message": "缺少 node" }), true),
        },
        "nexus_exec" => {
            let command: Option<Vec<String>> = args
                .get("command")
                .and_then(Value::as_array)
                .and_then(|a| a.iter().map(|v| v.as_str().map(str::to_string)).collect());
            match (text("node"), command) {
                (Some(node), Some(command)) if !command.is_empty() => Cmd::Exec {
                    node,
                    timeout: Some(args.get("timeout_s").and_then(Value::as_f64).unwrap_or(300.0)),
                    max_output: DEFAULT_MAX_OUTPUT,
                    cwd: text("cwd"),
                    env: vec![],
                    json: true,
                    command,
                },
                _ => return (json!({ "error": "usage", "message": "需要 node 和非空的 command 数组" }), true),
            }
        }
        _ => return (json!({ "error": "usage", "message": format!("没有工具 {name}") }), true),
    };
    let mut sink = Captured::default();
    // run_client 里也会调用到这里（nexus mcp），装箱以打断异步函数的递归。
    let mut empty = tokio::io::empty();
    let code = Box::pin(run_client(&cmd, ctl, &mut empty, &mut sink)).await;
    let out = String::from_utf8_lossy(&sink.stdout);
    match serde_json::from_str::<Value>(out.trim()) {
        // 执行类命令即使远端命令失败也有完整的结果；那不是工具出错。
        Ok(v) if v.get("schema").is_some() => (v, false),
        _ => {
            let err = String::from_utf8_lossy(&sink.stderr);
            let v = err.lines().last().and_then(|l| serde_json::from_str(l).ok()).unwrap_or_else(|| {
                json!({ "error": "internal", "message": err.trim(), "exit_code": code })
            });
            (v, true)
        }
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

fn list_json(entries: &[ListEntry]) -> String {
    let devices: Vec<Value> = entries
        .iter()
        .map(|e| {
            let mut m = report_fields(&e.report);
            m.insert("self".into(), json!(e.is_self));
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
        out.push_str(&format!("{:<16} {}  {:<8} {}\n", r.name, r.node_id.short(), role, state));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_examples() {
        for (input, want) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(input.as_bytes()), want);
        }
        assert_eq!(base64(&[0xff, 0x00, b'x']), "/wB4");
    }
}
