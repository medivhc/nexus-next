use nexus_cli::daemon::{self, parse_tun_net, transport_config};
use nexus_cli::{load_agent, parse, run_client, ssh, ssh_args, ssh_fail, Cli, Cmd, Ctl, Sink, EXIT_INTERNAL, EXIT_USAGE};
use nexus_fabric::{write_msg, FrameReader};
use nexus_fabric_iroh::Relay;
use nexus_runtime::CtlClient;
use nexus_wire::{AgentCredential, CtlHello, CtlRequest, CtlResponse, Input};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use nexus_cli::ctl_endpoint;

mod anchor;
#[cfg(target_os = "linux")]
mod mount;
mod ssh_proxy;
mod web_serve;

/// 写到真实终端，每段都立即刷新，长时间运行的命令也能实时看到输出。
struct Terminal;

impl Sink for Terminal {
    fn out(&mut self, bytes: &[u8]) {
        let mut out = io::stdout().lock();
        let _ = out.write_all(bytes);
        let _ = out.flush();
    }
    fn err(&mut self, bytes: &[u8]) {
        let mut err = io::stderr().lock();
        let _ = err.write_all(bytes);
        let _ = err.flush();
    }
}

fn main() {
    let cli = match parse(std::env::args_os().skip(1), &mut Terminal) {
        Ok(cli) => cli,
        Err(code) => std::process::exit(code),
    };
    // 排查问题用：NEXUS_LOG=iroh=debug 之类的过滤规则，把内部日志写到标准错误。
    if let Ok(filter) = std::env::var("NEXUS_LOG") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_writer(io::stderr)
            .try_init();
    }
    let data_dir = data_dir(&cli);
    // 终端会话的持有进程（M7.2）：自己建运行时，shell 退出才返回。
    if let Cmd::SessionHold { name, cols, rows, created_at_ms } = &cli.cmd {
        let size = nexus_sys_real::Size { cols: *cols, rows: *rows };
        let r = nexus_sys_real::holder::run(&data_dir, name, size, nexus_core_proto::Timestamp(*created_at_ms));
        if let Err(e) = r {
            Terminal.err(format!("nexus session-hold: {e}\n").as_bytes());
            std::process::exit(EXIT_INTERNAL);
        }
        std::process::exit(0);
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("无法启动运行时");
    let agent = match cli.agent.as_deref().map(load_agent).transpose() {
        Ok(a) => a,
        Err(msg) => {
            Terminal.err(format!("{}\n", serde_json::json!({ "error": "usage", "message": msg })).as_bytes());
            std::process::exit(EXIT_USAGE);
        }
    };
    let code = rt.block_on(async {
        match &cli.cmd {
            Cmd::Daemon { name, local_only, preflight, relay, relay_only, no_discovery, rendezvous, no_scheduling, agent_socket, proxy, tun, tun_net, anchor: anchor_args } => {
                // 升级前检查新版本（M7.3）：读得了数据目录就行，不起网络。
                if *preflight {
                    return match nexus_cli::daemon::preflight(&data_dir) {
                        Ok(()) => 0,
                        Err(e) => {
                            Terminal.err(format!("nexus daemon --preflight: {e}\n").as_bytes());
                            EXIT_INTERNAL
                        }
                    };
                }
                let tun = match tun {
                    None => None,
                    Some(dev) => match parse_tun_net(tun_net) {
                        Some(pool) => Some((dev.clone(), pool)),
                        None => {
                            let msg = format!("--tun-net 应当是网段的起点加前缀（8 到 29），例如 198.18.0.0/16：{tun_net}");
                            Terminal.err(format!("{}\n", serde_json::json!({ "error": "invalid_input", "message": msg })).as_bytes());
                            return nexus_cli::EXIT_INVALID_INPUT;
                        }
                    },
                };
                // 统一入口只对本机开放：别的机器连得上就等于把网络借给了它。
                let proxy = match proxy.as_deref().map(str::parse::<std::net::SocketAddr>) {
                    None => None,
                    Some(Ok(a)) if a.ip().is_loopback() => Some(a),
                    Some(_) => {
                        let msg = format!("--proxy 只能是本机地址，例如 127.0.0.1:1080：{}", proxy.as_deref().unwrap_or_default());
                        Terminal.err(format!("{}\n", serde_json::json!({ "error": "invalid_input", "message": msg })).as_bytes());
                        return nexus_cli::EXIT_INVALID_INPUT;
                    }
                };
                let proxy = match proxy {
                    None => None,
                    Some(a) => match tokio::net::TcpListener::bind(a).await {
                        Ok(l) => Some(l),
                        Err(e) => {
                            Terminal.err(format!("nexus daemon: 无法监听统一入口 {a}：{e}\n").as_bytes());
                            return EXIT_INTERNAL;
                        }
                    },
                };
                let agent_socket = agent_socket.clone().unwrap_or_else(|| data_dir.join("agent.sock"));
                let mut config = transport_config(*local_only, relay, *no_discovery);
                if *relay_only {
                    config.direct = false;
                }
                config.scheduling = !*no_scheduling;
                match rendezvous.as_deref() {
                    None => {}
                    Some("disabled") => config.rendezvous = None,
                    Some("default") => config.rendezvous = Some(nexus_fabric_iroh::rendezvous::DEFAULT_URL.to_string()),
                    Some(url) => config.rendezvous = Some(url.to_string()),
                }
                // 锚点：先起中继和页面，本机也用自己的中继。
                let _anchor = if anchor_args.enabled() {
                    if let Err(e) = std::fs::create_dir_all(&data_dir) {
                        Terminal.err(format!("nexus daemon: 无法创建数据目录：{e}\n").as_bytes());
                        return EXIT_INTERNAL;
                    }
                    match anchor::start(anchor_args, &data_dir).await {
                        Err(anchor::BadArgs(msg)) => {
                            Terminal.err(format!("{}\n", serde_json::json!({ "error": "invalid_input", "message": msg })).as_bytes());
                            return nexus_cli::EXIT_INVALID_INPUT;
                        }
                        Ok(Err(e)) => {
                            Terminal.err(format!("nexus daemon: 锚点：{e}\n").as_bytes());
                            return EXIT_INTERNAL;
                        }
                        Ok(Ok(a)) => {
                            eprintln!("nexus daemon: 锚点 {}（页面和中继）", a.url);
                            if relay == "default" && !*local_only {
                                config.relay = Relay::Custom(a.url.clone());
                            }
                            Some(a)
                        }
                    }
                } else {
                    if anchor_args.page_dir.is_some() {
                        let msg = "--page-dir 只用于锚点（--anchor 或 --anchor-dev）";
                        Terminal.err(format!("{}\n", serde_json::json!({ "error": "usage", "message": msg })).as_bytes());
                        return EXIT_USAGE;
                    }
                    None
                };
                let opts = daemon::Options {
                    data_dir: data_dir.clone(),
                    agent_socket,
                    name: name.clone(),
                    config,
                    proxy,
                    tun: tun.map(|(name, pool)| daemon::Tun::Named { name, pool }),
                };
                match daemon::run(opts, daemon::signalled()).await {
                    Ok(()) => 0,
                    Err(e) => {
                        Terminal.err(format!("nexus daemon: {e}\n").as_bytes());
                        EXIT_INTERNAL
                    }
                }
            }
            // Unix 上在前面就处理了；别的系统没有持有进程。
            Cmd::SessionHold { .. } => EXIT_USAGE,
            Cmd::ProtocolVersion => {
                println!("{} {}", nexus_wire::WIRE_VERSION, nexus_wire::WIRE_MINOR);
                0
            }
            Cmd::SshProxy { host } => {
                let ctl = UnixCtl::new(&data_dir, agent.clone());
                ssh_proxy::run(ctl, host).await
            }
            Cmd::Web { cmd: nexus_cli::WebCmd::Serve { dir, listen } } => web_serve::run(&mut Terminal, dir, listen).await,
            Cmd::SshConfig => {
                let exe = std::env::current_exe().unwrap_or_else(|_| "nexus".into());
                let dir = std::fs::canonicalize(&data_dir).unwrap_or(data_dir.clone());
                print!(
                    "\n# Nexus：ssh <设备名>.nexus（由 nexus ssh-config 生成）\n\
                     Host *.nexus\n\
                     \x20   ProxyCommand \"{exe}\" --data-dir \"{dir}\" ssh-proxy %h\n\
                     \x20   UserKnownHostsFile \"{dir}/ssh_known_hosts\"\n\
                     \x20   StrictHostKeyChecking accept-new\n\
                     \x20   LogLevel ERROR\n",
                    exe = exe.display(),
                    dir = dir.display(),
                );
                0
            }
            #[cfg(target_os = "linux")]
            Cmd::Drive { cmd: nexus_cli::drive::DriveCmd::Mount { path, dir, record } } => {
                mount::run(path, dir, record.as_deref(), &data_dir, UnixCtl::new(&data_dir, agent.clone()), &mut Terminal).await
            }
            cmd @ Cmd::Ssh { .. } if io::stdin().is_terminal() && io::stdout().is_terminal() => {
                let ctl = UnixCtl::new(&data_dir, agent.clone());
                interactive_ssh(cmd, &ctl).await
            }
            cmd => {
                let ctl = UnixCtl::new(&data_dir, agent.clone());
                run_client(cmd, &ctl, &mut tokio::io::stdin(), &mut Terminal).await
            }
        }
    });
    std::process::exit(code)
}

/// 在真实终端里运行 `nexus ssh`：原始模式，尺寸取自终端并跟随窗口变化。
async fn interactive_ssh(cmd: &Cmd, ctl: &UnixCtl) -> i32 {
    let mut args = match ssh_args(cmd) {
        Ok(a) => a,
        Err(msg) => {
            Terminal.err(format!("{}\n", serde_json::json!({ "error": "usage", "message": msg })).as_bytes());
            return EXIT_USAGE;
        }
    };
    let current = || crossterm::terminal::size().ok().map(|(cols, rows)| nexus_wire::Size { cols, rows });
    if !args.size_given {
        if let Some(size) = current() {
            args.size = size;
        }
    }
    let (tx, mut resizes) = mpsc::channel(4);
    if !args.size_given {
        #[cfg(unix)]
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let Ok(mut winch) = signal(SignalKind::window_change()) else { return };
            while winch.recv().await.is_some() {
                if let Some(size) = current() {
                    if tx.send(size).await.is_err() {
                        return;
                    }
                }
            }
        });
        // Windows 没有 SIGWINCH：每半秒看一次窗口大小，变了就告诉对方。
        #[cfg(windows)]
        tokio::spawn(async move {
            let mut last = current();
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let now = current();
                if now != last {
                    last = now;
                    if let Some(size) = now {
                        if tx.send(size).await.is_err() {
                            return;
                        }
                    }
                }
            }
        });
    }
    if crossterm::terminal::enable_raw_mode().is_err() {
        Terminal.err("nexus ssh: 无法把终端切换到原始模式\n".as_bytes());
        return EXIT_INTERNAL;
    }
    let result = ssh(ctl, &args, &mut tokio::io::stdin(), &mut tokio::io::stdout(), &mut resizes).await;
    let _ = crossterm::terminal::disable_raw_mode();
    match result {
        Ok(code) => code,
        Err(e) => ssh_fail(&mut Terminal, &e),
    }
}

fn data_dir(cli: &Cli) -> PathBuf {
    if let Some(d) = &cli.data_dir {
        return d.clone();
    }
    if let Some(d) = std::env::var_os("NEXUS_DATA_DIR") {
        return d.into();
    }
    // Windows 上没有 HOME：用 %LOCALAPPDATA%\nexus（M11.3）。
    #[cfg(windows)]
    if let Some(d) = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("USERPROFILE")) {
        return PathBuf::from(d).join("nexus");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| ".".into());
    home.join(".local/share/nexus")
}

/// 命令行一侧：通过 Unix 套接字连接常驻程序，把套接字两个方向接到会话通道上。
#[derive(Clone)]
pub(crate) struct UnixCtl {
    socket: PathBuf,
    agent: Option<AgentCredential>,
}

impl UnixCtl {
    /// 用户本人用数据目录里的控制入口；带凭据时用 agent 入口（`NEXUS_AGENT_SOCKET` 或数据目录里的 agent.sock）。
    fn new(data_dir: &Path, agent: Option<AgentCredential>) -> Self {
        let socket = match &agent {
            None => data_dir.join("ctl.sock"),
            Some(_) => std::env::var_os("NEXUS_AGENT_SOCKET").map(PathBuf::from).unwrap_or_else(|| data_dir.join("agent.sock")),
        };
        Self { socket, agent }
    }
}

impl Ctl for UnixCtl {
    async fn open(&self, req: CtlRequest) -> io::Result<CtlClient> {
        let mut stream = ctl_endpoint::connect(&self.socket).await?;
        write_msg(&mut stream, &CtlHello { agent: self.agent.clone(), request: req }).await?;
        let (rd, mut wr) = tokio::io::split(stream);
        let (in_tx, mut in_rx) = mpsc::channel::<Input>(16);
        let (resp_tx, resp_rx) = mpsc::channel::<CtlResponse>(16);
        tokio::spawn(async move {
            while let Some(input) = in_rx.recv().await {
                if write_msg(&mut wr, &input).await.is_err() {
                    break;
                }
            }
            let _ = wr.shutdown().await;
        });
        tokio::spawn(async move {
            let mut reader = FrameReader::new(rd);
            while let Ok(Some(resp)) = reader.next::<CtlResponse>().await {
                if resp_tx.send(resp).await.is_err() {
                    break;
                }
            }
        });
        Ok(CtlClient { tx: in_tx, rx: resp_rx })
    }
}

