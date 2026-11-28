//! 常驻程序：命令行的 `nexus daemon` 和 Android 应用都从这里启动。

use crate::tun;
use nexus_fabric::{read_msg, write_msg, FrameReader};
use nexus_fabric_iroh::{Config, IrohTransport, Relay};
use nexus_net_core::fakeip::FakePool;
use nexus_runtime::{ctl_session, load_or_create_device_key, CtlCall, Node};
use nexus_sys_real::RealSystem;
use nexus_wire::{CtlHello, CtlRequest, CtlResponse, ErrorCode, Input, WireError};
use std::time::Duration;
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
    /// 常驻程序自己建网卡（Linux、macOS、Windows）。
    Named { name: String, pool: FakePool },
    /// 别人建好的网卡（Android 的 VpnService），`label` 只用于显示。
    Device {
        device: tun_rs::AsyncDevice,
        label: String,
        pool: FakePool,
        app_router: Option<tun::AppRouter>,
        on_traffic: Option<tun::TrafficObserver>,
    },
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
    // 终端会话交给独立的持有进程（M7.2）：常驻程序重启后会话还在。
    // 执行的命令带着本机的二进制和数据目录（M7.3b）：在这台设备上执行的 `nexus` 找得到这里的常驻程序。
    let abs_dir = std::path::absolute(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let mut exec_env = vec![("NEXUS_DATA_DIR".to_string(), abs_dir.display().to_string())];
    let system = match std::env::current_exe() {
        Ok(exe) => {
            exec_env.push(("NEXUS_EXE".to_string(), exe.display().to_string()));
            RealSystem::new(data_dir)?.with_session_holder(exe).with_exec_env(exec_env)
        }
        Err(_) => RealSystem::new(data_dir)?.with_exec_env(exec_env),
    };
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
    // 名字解析配置（macOS、Windows）在常驻程序正常退出时撤掉：守卫活到本函数结束。
    let mut _resolver = None;
    match tun {
        None => {}
        Some(Tun::Named { name: dev, pool }) => {
            let entry = node.enable_tun(&dev, pool.clone());
            let (actual, guard) =
                tun::start(&dev, &pool, entry).map_err(|e| io::Error::other(format!("无法建虚拟网卡 {dev}（需要管理员权限）：{e}")))?;
            if actual != dev {
                node.enable_tun(&actual, pool.clone());
            }
            _resolver = Some(guard);
            let (net, prefix) = pool.network();
            eprintln!("nexus daemon: 完整节点：虚拟网卡 {actual}，网段 {net}/{prefix}，名字服务 {}", pool.dns());
        }
        Some(Tun::Device {
            device,
            label,
            pool,
            app_router,
            on_traffic,
        }) => {
            let entry = node.enable_tun(&label, pool.clone());
            tun::run_stack(device, &pool, entry, app_router, on_traffic)?;
            eprintln!(
                "nexus daemon: 完整节点：虚拟网卡 {label}，名字服务 {}",
                pool.dns()
            );
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
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let _ = stream.set_nodelay(true);
                        if entry.send(Box::new(stream)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        eprintln!("nexus daemon: 统一入口 accept 出错：{e}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        });
    }

    let (tx, mut rx) = mpsc::channel::<CtlCall>(64);
    let (reexec_tx, mut reexec_rx) = mpsc::channel::<PathBuf>(1);
    let agent_tx = tx.clone();
    // 刚升级过（M7.3）：确认新版本连得上网络，连不上就换回。
    tokio::spawn(confirm_upgrade(data_dir.to_path_buf(), tx.clone(), reexec_tx.clone()));
    let owner_dir = data_dir.to_path_buf();
    let owner_reexec = reexec_tx.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok(stream) => {
                    tokio::spawn(serve_ctl(stream, tx.clone(), false, Some(owner_dir.clone()), Some(owner_reexec.clone())));
                }
                Err(e) => {
                    eprintln!("nexus daemon: 控制入口 accept 出错：{e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    });
    tokio::spawn(async move {
        loop {
            match agent_listener.accept().await {
                Ok(stream) => {
                    tokio::spawn(serve_ctl(stream, agent_tx.clone(), true, None, None));
                }
                Err(e) => {
                    eprintln!("nexus daemon: agent 入口 accept 出错：{e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    });

    let mut pending_reexec = None;
    let result = tokio::select! {
        r = node.run(&mut rx) => r,
        Some(exe) = reexec_rx.recv() => {
            pending_reexec = Some(exe);
            Ok(())
        }
        _ = stop => Ok(()),
    };
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(agent_socket);
    if let Some(exe) = pending_reexec {
        let e = crate::upgrade::reexec(&exe);
        eprintln!("nexus daemon: 无法重新启动：{e}");
        return Err(e);
    }
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

/// 升级前检查新版本（`nexus daemon --preflight`，M7.3）：读得了本机的数据目录。
pub fn preflight(data_dir: &std::path::Path) -> io::Result<()> {
    if !data_dir.exists() {
        return Ok(());
    }
    let system = RealSystem::new(data_dir)?;
    load_or_create_device_key(&system)?;
    Ok(())
}

/// 向节点问一个问题，取第一个回答。
async fn ask(tx: &mpsc::Sender<CtlCall>, req: CtlRequest) -> Option<CtlResponse> {
    let (call, mut client) = ctl_session(req, None);
    tx.send(call).await.ok()?;
    client.rx.recv().await
}

/// 现在在线的其他设备的名字。
async fn online_peers(tx: &mpsc::Sender<CtlCall>) -> Vec<String> {
    match ask(tx, CtlRequest::List).await {
        Some(CtlResponse::List(entries)) => entries.into_iter().filter(|e| !e.is_self && e.report.online).map(|e| e.report.name).collect(),
        _ => Vec::new(),
    }
}

/// 升级相关的本机请求（M7.3）：由这里处理，不进节点。返回回应，以及回应之后要执行的二进制。
async fn handle_upgrade(req: &CtlRequest, data_dir: &std::path::Path, tx: &mpsc::Sender<CtlCall>) -> (CtlResponse, Option<PathBuf>) {
    match req {
        CtlRequest::UpgradeStatus => {
            let report = crate::upgrade::load(data_dir).map(|s| s.report()).unwrap_or(nexus_wire::UpgradeReport {
                state: "none".into(),
                from: String::new(),
                to: String::new(),
                exe: String::new(),
                message: String::new(),
                id: 0,
            });
            (CtlResponse::Upgrade(report), None)
        }
        CtlRequest::UpgradeRollback => {
            let Some(mut state) = crate::upgrade::load(data_dir).filter(|s| s.prev.exists()) else {
                return (CtlResponse::Error(WireError::new(ErrorCode::InvalidState, "没有可以换回的上一个版本")), None);
            };
            if let Err(e) = crate::upgrade::restore(&mut state, data_dir, "手动换回") {
                return (CtlResponse::Error(WireError::new(ErrorCode::Internal, format!("换不回去：{e}"))), None);
            }
            let exe = state.exe.clone();
            (CtlResponse::Upgrade(state.report()), Some(exe))
        }
        CtlRequest::Upgrade { file, confirm_ms, force } => {
            if !cfg!(any(unix, windows)) {
                return (CtlResponse::Error(WireError::new(ErrorCode::InvalidState, "这个系统上还不支持 nexus upgrade：手动换二进制再重启服务")), None);
            }
            let exe = match std::env::current_exe() {
                Ok(e) => e,
                Err(e) => return (CtlResponse::Error(WireError::new(ErrorCode::Internal, format!("不知道本机的二进制在哪：{e}"))), None),
            };
            let peers = online_peers(tx).await;
            let file = PathBuf::from(file);
            let (exe2, dir) = (exe.clone(), data_dir.to_path_buf());
            let (confirm_ms, force) = (*confirm_ms, *force);
            let installed = tokio::task::spawn_blocking(move || crate::upgrade::install(&file, &exe2, &dir, peers, confirm_ms, force)).await;
            match installed {
                Ok(Ok(state)) => (CtlResponse::Upgrade(state.report()), Some(exe)),
                Ok(Err(e)) => (CtlResponse::Error(e), None),
                Err(e) => (CtlResponse::Error(WireError::new(ErrorCode::Internal, e.to_string())), None),
            }
        }
        _ => (CtlResponse::Error(WireError::new(ErrorCode::Internal, "不是升级请求")), None),
    }
}

/// 新版本启动时：按 `upgrade.json` 确认（M7.3）。
async fn confirm_upgrade(data_dir: PathBuf, tx: mpsc::Sender<CtlCall>, reexec_tx: mpsc::Sender<PathBuf>) {
    use crate::upgrade;
    let Some(mut state) = upgrade::load(&data_dir).filter(|s| s.state == "pending") else { return };
    state.attempts += 1;
    let _ = upgrade::save(&data_dir, &state);
    let roll_back = |mut state: upgrade::UpgradeState, why: String| {
        let dir = data_dir.clone();
        let reexec_tx = reexec_tx.clone();
        async move {
            eprintln!("nexus daemon: {why}，换回 {}", state.from);
            if upgrade::restore(&mut state, &dir, &why).is_ok() {
                let _ = reexec_tx.send(state.exe).await;
            }
        }
    };
    if state.attempts > upgrade::MAX_ATTEMPTS {
        let why = format!("新版本 {} 启动了 {} 次都没有确认", state.to, upgrade::MAX_ATTEMPTS);
        return roll_back(state, why).await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(state.confirm_ms);
    loop {
        let online = online_peers(&tx).await;
        if state.peers.is_empty() || online.iter().any(|p| state.peers.contains(p)) {
            state.state = "confirmed".into();
            state.message = match online.first() {
                Some(p) => format!("新版本连上了 {p}"),
                None => "升级前没有在线的设备，没有检查网络".into(),
            };
            let _ = upgrade::save(&data_dir, &state);
            eprintln!("nexus daemon: 升级到 {} 已确认（{}）", state.to, state.message);
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            let secs = state.confirm_ms / 1000;
            let why = format!("升级到 {} 之后 {secs} 秒内连不上升级前在线的设备（{}）", state.to, state.peers.join("、"));
            return roll_back(state, why).await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// 常驻程序一侧：读出请求，开一个会话交给节点，然后在套接字和会话之间双向转发。
/// `owner_dir`：本人入口时是数据目录（升级请求要用）。
async fn serve_ctl(
    mut stream: BoxedCtlStream,
    tx: mpsc::Sender<CtlCall>,
    agent_only: bool,
    owner_dir: Option<PathBuf>,
    reexec_tx: Option<mpsc::Sender<PathBuf>>,
) {
    let Ok(hello) = read_msg::<_, CtlHello>(&mut stream).await else { return };
    if agent_only && hello.agent.is_none() {
        let e = WireError::new(ErrorCode::Denied, "agent 入口只接受 agent 凭据（--agent 或 NEXUS_AGENT）");
        let _ = write_msg(&mut stream, &CtlResponse::Error(e)).await;
        let _ = stream.shutdown().await;
        return;
    }
    if matches!(hello.request, CtlRequest::Upgrade { .. } | CtlRequest::UpgradeStatus | CtlRequest::UpgradeRollback) {
        let (resp, then) = match (&owner_dir, &hello.agent) {
            (Some(dir), None) => handle_upgrade(&hello.request, dir, &tx).await,
            _ => (CtlResponse::Error(WireError::new(ErrorCode::Denied, "只有本人能升级常驻程序")), None),
        };
        let _ = write_msg(&mut stream, &resp).await;
        let _ = stream.shutdown().await;
        if let (Some(exe), Some(reexec_tx)) = (then, reexec_tx) {
            // 命令行收到回答后再通知主循环清理并重新执行新的二进制。
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = reexec_tx.send(exe).await;
        }
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
