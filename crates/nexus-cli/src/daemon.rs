//! 常驻程序：命令行的 `nexus daemon` 和 Android 应用都从这里启动。

use crate::tun;
use nexus_fabric::{read_msg, write_msg, FrameReader};
use nexus_fabric_iroh::{Config, IrohTransport, Relay};
use nexus_net_core::fakeip::FakePool;
use nexus_runtime::{ctl_session, load_or_create_device_key, CtlCall, Node};
use nexus_sys_real::RealSystem;
use nexus_wire::{CtlHello, CtlResponse, ErrorCode, Input, WireError};
use std::io;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::ctl_endpoint::{self, BoxedCtlStream, CtlKind};

/// 命令行参数对应的传输设置。
pub fn transport_config(local_only: bool, relay: &str, no_discovery: bool) -> Config {
    if local_only {
        return Config::local_only();
    }
    let relay = match relay {
        "default" => Relay::Default,
        "disabled" => Relay::Disabled,
        url => Relay::Custom(url.to_string()),
    };
    let rendezvous = (!no_discovery).then(|| nexus_fabric_iroh::rendezvous::DEFAULT_URL.to_string());
    Config { relay, discovery: !no_discovery, bind: None, direct: true, scheduling: true, rendezvous }
}

/// `--tun-net`：网段的起点加前缀。
pub fn parse_tun_net(s: &str) -> Option<nexus_net_core::fakeip::FakePool> {
    let (ip, prefix) = s.split_once('/')?;
    nexus_net_core::fakeip::FakePool::new(ip.parse().ok()?, prefix.parse().ok()?)
}

/// 完整节点的虚拟网卡。
pub enum Tun {
    /// 常驻程序自己建网卡（Linux、macOS）。
    Named { name: String, pool: FakePool },
    /// 别人建好的网卡（Android 的 VpnService），`label` 只用于显示。
    Device { device: tun_rs::AsyncDevice, label: String, pool: FakePool },
}

pub struct Options {
    pub data_dir: PathBuf,
    pub agent_socket: PathBuf,
    pub name: Option<String>,
    pub config: Config,
    /// 已经在监听的统一入口。
    pub proxy: Option<tokio::net::TcpListener>,
    pub tun: Option<Tun>,
}

/// 运行常驻程序，直到节点停止或 `stop` 完成。
pub async fn run(opts: Options, stop: impl std::future::Future<Output = ()>) -> io::Result<()> {
    let Options { data_dir, agent_socket, name, config, proxy, tun } = opts;
    let (data_dir, agent_socket) = (data_dir.as_path(), agent_socket.as_path());
    let system = RealSystem::new(data_dir)?;
    let key = load_or_create_device_key(&system)?;
    let name = name
        .or_else(default_node_name)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "node".to_string());
    let name: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(64).collect();
    let name = if name.is_empty() { "node".to_string() } else { name };
    let transport = IrohTransport::bind(key.to_seed(), config).await?;
    let node = Node::new(transport, system, &name)?;

    // 虚拟网卡先于控制套接字：控制套接字出现时完整节点已经可用。
    match tun {
        None => {}
        Some(Tun::Named { name: dev, pool }) => {
            let entry = node.enable_tun(&dev, pool.clone());
            tun::start(&dev, &pool, entry).map_err(|e| io::Error::other(format!("无法建虚拟网卡 {dev}（需要管理员权限）：{e}")))?;
            let (net, prefix) = pool.network();
            eprintln!("nexus daemon: 完整节点：虚拟网卡 {dev}，网段 {net}/{prefix}，名字服务 {}", pool.dns());
        }
        Some(Tun::Device { device, label, pool }) => {
            let entry = node.enable_tun(&label, pool.clone());
            tun::run_stack(device, &pool, entry)?;
            eprintln!("nexus daemon: 完整节点：虚拟网卡 {label}，名字服务 {}", pool.dns());
        }
    }

    // 本人入口：只有数据目录的主人能连。
    let socket = data_dir.join("ctl.sock");
    let mut listener = ctl_endpoint::bind(&socket, CtlKind::Owner).await?;

    // agent 入口：本机任何账号都能连上，但只接受 agent 凭据。
    let mut agent_listener = ctl_endpoint::bind(agent_socket, CtlKind::Agent).await?;

    if let Some(listener) = proxy {
        let entry = node.proxy_entry();
        if let Ok(a) = listener.local_addr() {
            eprintln!("nexus daemon: 统一入口 {a}（SOCKS5 / HTTP 代理）");
        }
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let _ = stream.set_nodelay(true);
                if entry.send(Box::new(stream)).is_err() {
                    return;
                }
            }
        });
    }

    let (tx, mut rx) = mpsc::channel::<CtlCall>(64);
    let agent_tx = tx.clone();
    tokio::spawn(async move {
        while let Ok(stream) = listener.accept().await {
            tokio::spawn(serve_ctl(stream, tx.clone(), false));
        }
    });
    tokio::spawn(async move {
        while let Ok(stream) = agent_listener.accept().await {
            tokio::spawn(serve_ctl(stream, agent_tx.clone(), true));
        }
    });

    let result = tokio::select! {
        r = node.run(&mut rx) => r,
        _ = stop => Ok(()),
    };
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(agent_socket);
    result
}

/// 没有指定名字时用主机名。Windows 上没有 /etc/hostname，用 COMPUTERNAME。
fn default_node_name() -> Option<String> {
    #[cfg(unix)]
    {
        std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string())
    }
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").ok()
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// 等到收到 SIGINT 或 SIGTERM。
pub async fn signalled() {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate() => {}
    }
}

async fn terminate() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    }
    // Windows 只支持 Ctrl+C（控制台控制事件），上面的 select 已经覆盖。
    #[cfg(windows)]
    {
        std::future::pending().await
    }
}

/// 常驻程序一侧：读出请求，开一个会话交给节点，然后在套接字和会话之间双向转发。
async fn serve_ctl(mut stream: BoxedCtlStream, tx: mpsc::Sender<CtlCall>, agent_only: bool) {
    let Ok(hello) = read_msg::<_, CtlHello>(&mut stream).await else { return };
    if agent_only && hello.agent.is_none() {
        let e = WireError::new(ErrorCode::Denied, "agent 入口只接受 agent 凭据（--agent 或 NEXUS_AGENT）");
        let _ = write_msg(&mut stream, &CtlResponse::Error(e)).await;
        let _ = stream.shutdown().await;
        return;
    }
    let (call, mut client) = ctl_session(hello.request, hello.agent);
    if tx.send(call).await.is_err() {
        return;
    }
    let (rd, mut wr) = tokio::io::split(stream);
    let inputs = client.tx;
    let forward_inputs = async move {
        let mut reader = FrameReader::new(rd);
        while let Ok(Some(input)) = reader.next::<Input>().await {
            if inputs.send(input).await.is_err() {
                return;
            }
        }
        // 命令行断开：通道随 `inputs` 一起关闭，节点会把它当作取消。
    };
    let forward_responses = async move {
        while let Some(resp) = client.rx.recv().await {
            if write_msg(&mut wr, &resp).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    };
    tokio::join!(forward_inputs, forward_responses);
}
