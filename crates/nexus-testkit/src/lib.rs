//! 测试工具箱：同一份验收场景，跑在两种后端上。
//!
//! - 仿真后端：所有节点在同一个线程里，网络和时间都是模拟的，由种子决定，可精确复现。
//! - 真实后端：每个节点是一个真正的 `nexus daemon` 进程，命令行是真正的 `nexus` 二进制。
//!
//! 场景只通过命令行这一层操作节点，和用户、agent 看到的完全一样。
#![forbid(unsafe_code)]

pub use nexus_cli::Output;

use nexus_cli::{load_agent, parse, run_client, ssh, ssh_args, ssh_fail, Captured, Ctl, Sink, SshArgs, EXIT_USAGE};
use nexus_fabric_sim::SimTransport;
use nexus_runtime::{ctl_session, load_or_create_device_key, CtlCall, CtlClient, Node, ProxyStream};
use nexus_sys_sim::{SimClock, SimDisk, SimServices, SimSystem};
pub use nexus_sys_sim::SimService as Service;
use nexus_wire::{AgentCredential, CtlRequest};
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// 场景运行时所在的世界。
pub struct World {
    inner: Inner,
    trace: Rc<RefCell<Vec<String>>>,
}

enum Inner {
    Sim {
        nodes: HashMap<String, mpsc::Sender<CtlCall>>,
        clock: SimClock,
        services: HashMap<String, SimServices>,
        next_port: std::cell::Cell<u16>,
        proxies: HashMap<String, mpsc::UnboundedSender<ProxyStream>>,
    },
    Real(RealWorld),
}

impl World {
    /// 在节点上运行一个只监听本机的服务，返回它的端口。
    pub async fn serve(&self, node: &str, service: Service) -> u16 {
        match &self.inner {
            Inner::Sim { services, next_port, .. } => {
                let port = next_port.get();
                next_port.set(port + 1);
                services.get(node).unwrap_or_else(|| panic!("没有节点 {node}")).add(port, service);
                port
            }
            Inner::Real(real) => real.serve(service).await,
        }
    }

    /// 在节点上开一个从「外面」连得到的服务，返回（地址，端口）。
    /// 仿真里只有用这个地址才连得上，而且只在这个节点上；真实后端里是这台机器的一个非本机地址。
    pub async fn serve_public(&self, node: &str, service: Service) -> (String, u16) {
        match &self.inner {
            Inner::Sim { services, next_port, .. } => {
                let port = next_port.get();
                next_port.set(port + 1);
                let ip = std::net::IpAddr::from([198, 51, 100, 7]);
                services.get(node).unwrap_or_else(|| panic!("没有节点 {node}")).add_public((ip, port).into(), service);
                (ip.to_string(), port)
            }
            Inner::Real(_) => {
                let ip = public_ip();
                (ip.to_string(), serve_on(std::net::SocketAddr::new(ip, 0), service))
            }
        }
    }

    /// 节点的本机统一入口（SOCKS5 / HTTP 代理）。
    pub fn proxy(&self, node: &str) -> ProxyClient {
        match &self.inner {
            Inner::Sim { proxies, .. } => {
                ProxyClient::Sim(proxies.get(node).unwrap_or_else(|| panic!("场景里没有节点 {node}")).clone())
            }
            Inner::Real(real) => ProxyClient::Real(real.proxy_port(node)),
        }
    }

    /// 一个现在没有程序在用的本机端口。
    pub fn unused_port(&self) -> u16 {
        match &self.inner {
            Inner::Sim { next_port, .. } => {
                let port = next_port.get();
                next_port.set(port + 1);
                port
            }
            Inner::Real(_) => std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port(),
        }
    }

    /// 在某个节点上执行一条 `nexus` 命令。
    pub async fn cli(&self, node: &str, args: &[&str]) -> Output {
        self.cli_with_stdin(node, args, b"").await
    }

    /// 执行一条命令，并把 `stdin` 作为它的标准输入。
    pub async fn cli_with_stdin(&self, node: &str, args: &[&str], stdin: &[u8]) -> Output {
        let out = match &self.inner {
            Inner::Sim { nodes, .. } => {
                let tx = nodes.get(node).unwrap_or_else(|| panic!("场景里没有节点 {node}"));
                let mut sink = Captured::default();
                let code = match parse(args.iter().copied(), &mut sink) {
                    Err(code) => code,
                    Ok(cli) => match cli.agent.as_deref().map(load_agent).transpose() {
                        Err(msg) => {
                            sink.err(format!("{}\n", serde_json::json!({ "error": "usage", "message": msg })).as_bytes());
                            EXIT_USAGE
                        }
                        Ok(agent) => {
                            let mut input = std::io::Cursor::new(stdin.to_vec());
                            run_client(&cli.cmd, &ChanCtl(tx.clone(), agent), &mut input, &mut sink).await
                        }
                    },
                };
                Output::from_captured(code, sink)
            }
            Inner::Real(real) => real.cli(node, args, stdin),
        };
        let at = turmoil::sim_elapsed().map(|d| d.as_millis()).unwrap_or(0);
        self.trace
            .borrow_mut()
            .push(format!("{at}ms {node} {args:?} -> {} {}{}", out.code, out.stdout, out.stderr));
        out
    }

    /// 让所有节点的时钟向前跳一段，期间什么都不发生。
    ///
    /// 仿真里瞬间完成，适合验证「四小时后授权过期」这类只取决于当前时间的行为。
    /// 它不会触发这段时间里本应到期的定时器；需要定时器真的走过去时用 [`World::run_for`]。
    /// 真实后端没有办法拨动时钟，只能真的等待，所以那里只应使用很短的时长。
    pub async fn advance(&self, d: Duration) {
        match &self.inner {
            Inner::Sim { clock, .. } => clock.jump(d),
            Inner::Real(_) => tokio::time::sleep(d).await,
        }
    }

    /// 让世界真实地运行一段时间：网络收发、定时器都照常进行。
    ///
    /// 仿真按毫秒步进，模拟一秒大约耗费几毫秒的真实时间，所以适合秒级时长，
    /// 不适合小时级（那种情况用 [`World::advance`]）。
    pub async fn run_for(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }

    pub fn is_sim(&self) -> bool {
        matches!(self.inner, Inner::Sim { .. })
    }

    /// 执行一条命令，要求成功，返回解析后的 JSON 输出。命令里会自动加上 `--json`。
    pub async fn cli_ok(&self, node: &str, args: &[&str]) -> serde_json::Value {
        let mut full: Vec<&str> = args.to_vec();
        full.push("--json");
        let out = self.cli(node, &full).await;
        assert_eq!(out.code, 0, "{node}: nexus {} 失败：{}", full.join(" "), out.stderr);
        serde_json::from_str(&out.stdout)
            .unwrap_or_else(|e| panic!("{node}: nexus {} 的输出不是 JSON（{e}）：{:?}", full.join(" "), out.stdout))
    }

    /// 用户的操作流程：在 `admin` 上建立网络，再把 `members` 逐个加入。
    pub async fn network(&self, admin: &str, members: &[&str]) {
        self.cli_ok(admin, &["init", "--name", admin]).await;
        for m in members {
            let code = self.cli_ok(m, &["join"]).await;
            let code = code["code"].as_str().expect("join 应当输出加入码").to_string();
            self.cli_ok(admin, &["device", "approve", &code, "--name", m]).await;
        }
    }

    /// 让一个节点从网络上消失。仿真里切断它与所有节点的网络；真实后端里杀掉它的进程。
    pub fn take_offline(&self, node: &str) {
        match &self.inner {
            Inner::Sim { nodes, .. } => {
                for other in nodes.keys().filter(|n| n.as_str() != node) {
                    turmoil::partition(node, other.as_str());
                }
            }
            Inner::Real(real) => real.kill(node),
        }
    }

    /// 恢复 [`World::take_offline`] 切断的网络。只在仿真后端可用。
    pub fn bring_online(&self, node: &str) {
        match &self.inner {
            Inner::Sim { nodes, .. } => {
                for other in nodes.keys().filter(|n| n.as_str() != node) {
                    turmoil::repair(node, other.as_str());
                }
            }
            Inner::Real(_) => panic!("真实后端无法让被杀掉的节点原样恢复"),
        }
    }
}

impl World {
    /// 在某个节点上启动一个交互式命令（`nexus ssh ...`），返回一个模拟的终端。
    ///
    /// 没有给出 `--size` 时使用 80x24。
    pub async fn term(&self, node: &str, args: &[&str]) -> Term {
        let mut full: Vec<&str> = args.to_vec();
        if !full.contains(&"--size") {
            full.extend(["--size", "80x24"]);
        }
        let mut ignored = Captured::default();
        let cli = parse(full.iter().copied(), &mut ignored).unwrap_or_else(|_| panic!("参数不对：{full:?}"));
        let ssh = ssh_args(&cli.cmd).unwrap_or_else(|e| panic!("{e}"));
        let state = Arc::new(TermState::new(ssh.size));
        let label = format!("{node}$ nexus {}", args.join(" "));
        self.trace.borrow_mut().push(format!("term {label}"));
        let io = match &self.inner {
            Inner::Sim { nodes, .. } => {
                let tx = nodes.get(node).unwrap_or_else(|| panic!("场景里没有节点 {node}")).clone();
                let agent = cli.agent.as_deref().map(load_agent).transpose().expect("合法凭据");
                TermIo::Sim(sim_term(tx, ssh, agent, state.clone()))
            }
            Inner::Real(real) => TermIo::Real(real.term(node, &full, state.clone())),
        };
        Term { io, state, label }
    }

    /// 让 `a` 和 `b` 之间的网络中断一段时间，然后恢复。
    ///
    /// 期间两者之间的数据都送不到，恢复后连接上重传的数据照常送达（就像 QUIC 和 TCP 的重传）。
    /// 仿真里把两者之间的消息全部扣住，到时放行；真实后端里把 `b` 的常驻程序暂停（SIGSTOP）这么久。
    pub async fn interrupt(&self, a: &str, b: &str, d: Duration) {
        match &self.inner {
            Inner::Sim { .. } => {
                turmoil::hold(a, b);
                tokio::time::sleep(d).await;
                turmoil::release(a, b);
            }
            Inner::Real(real) => {
                real.signal(b, nix::sys::signal::Signal::SIGSTOP);
                tokio::time::sleep(d).await;
                real.signal(b, nix::sys::signal::Signal::SIGCONT);
            }
        }
    }
}

impl World {
    /// 在某个节点所在的「机器」上运行一个普通程序（例如系统自带的 ssh、scp、git），只用于真实后端。
    ///
    /// 环境：`PATH` 最前面是 nexus 所在的目录，`NEXUS_DATA_DIR` 指向这个节点，
    /// `HOME` 是这个节点专用的空目录（没有任何 SSH 密钥和配置）。
    pub async fn host(&self, node: &str, argv: &[&str], stdin: &[u8]) -> Output {
        let Inner::Real(real) = &self.inner else { panic!("host 只能用于真实后端") };
        self.trace.borrow_mut().push(format!("host {node}$ {}", argv.join(" ")));
        real.host(node, None, argv, stdin)
    }

    /// 和 [`World::host`] 一样，但以另一个系统账号（`uid`）运行。`NEXUS_AGENT_SOCKET` 指向该节点的 agent 入口。
    pub async fn host_as(&self, node: &str, uid: u32, argv: &[&str], stdin: &[u8]) -> Output {
        let Inner::Real(real) = &self.inner else { panic!("host_as 只能用于真实后端") };
        self.trace.borrow_mut().push(format!("host {node} uid={uid}$ {}", argv.join(" ")));
        real.host(node, Some(uid), argv, stdin)
    }

    /// 在后台启动一条 nexus 命令，不等它结束，只用于真实后端。用来模拟调用方中途离开。
    pub fn spawn_cli(&self, node: &str, args: &[&str]) -> Background {
        self.spawn_cli_env(node, args, &[])
    }

    /// 同 [`World::spawn_cli`]，另外设置环境变量；标准输出可以用 [`Background::stdout`] 读。
    pub fn spawn_cli_env(&self, node: &str, args: &[&str], env: &[(&str, &str)]) -> Background {
        let Inner::Real(real) = &self.inner else { panic!("spawn_cli 只能用于真实后端") };
        self.trace.borrow_mut().push(format!("spawn {node}$ nexus {}", args.join(" ")));
        let dir = real.dirs.get(node).unwrap_or_else(|| panic!("场景里没有节点 {node}"));
        let n = SPAWNED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let err_path = dir.join(format!("spawn-{n}.stderr"));
        let err = std::fs::File::create(&err_path).expect("无法创建后台命令的错误输出文件");
        let out_path = dir.join(format!("spawn-{n}.stdout"));
        let out = std::fs::File::create(&out_path).expect("无法创建后台命令的输出文件");
        let child = Command::new(&real.bin)
            .args(["--data-dir", dir.to_str().unwrap()])
            .args(args)
            .env("NEXUS_AGENT_SOCKET", real.agent_socket(node))
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()
            .expect("无法执行 nexus");
        Background { child, err_path, out_path, done: None }
    }

    /// 某个节点的 agent 入口（套接字路径），只用于真实后端。
    pub fn agent_socket(&self, node: &str) -> PathBuf {
        let Inner::Real(real) = &self.inner else { panic!("agent_socket 只能用于真实后端") };
        real.agent_socket(node)
    }

    /// 某个节点的 `HOME`（见 [`World::host`]），只用于真实后端。
    pub fn home_dir(&self, node: &str) -> PathBuf {
        let Inner::Real(real) = &self.inner else { panic!("home_dir 只能用于真实后端") };
        real.home(node)
    }

    /// 节点常驻程序的数据目录（只用于真实后端）。
    pub fn data_dir(&self, node: &str) -> PathBuf {
        let Inner::Real(real) = &self.inner else { panic!("data_dir 只能用于真实后端") };
        real.dirs.get(node).unwrap_or_else(|| panic!("场景里没有节点 {node}")).clone()
    }
}

impl World {
    /// 让 `a` 和 `b` 之间断网这么久：期间发出的数据全部丢失，长到连接两端都认为对方已经失联。
    ///
    /// 仿真里切断两者之间的网络（发出的数据直接丢弃），到时恢复；
    /// 真实后端里把 `b` 的常驻程序暂停（SIGSTOP）这么久。
    pub async fn cut(&self, a: &str, b: &str, d: Duration) {
        match &self.inner {
            Inner::Sim { .. } => {
                turmoil::partition(a, b);
                tokio::time::sleep(d).await;
                turmoil::repair(a, b);
            }
            Inner::Real(real) => {
                real.signal(b, nix::sys::signal::Signal::SIGSTOP);
                tokio::time::sleep(d).await;
                real.signal(b, nix::sys::signal::Signal::SIGCONT);
            }
        }
    }
}

impl World {
    /// 让场景里的浏览器和 `device` 之间断网这么久，见 [`World::cut`]。
    pub async fn cut_browser(&self, device: &str, d: Duration) {
        self.cut(SCENARIO_HOST, device, d).await
    }
}

/// 仿真里场景本身（以及其中的浏览器）所在的主机。
const SCENARIO_HOST: &str = "scenario";

impl World {
    /// 场景里的当前时间：仿真里是仿真时钟，真实后端里是系统时间。浏览器用它签名。
    pub fn now(&self) -> nexus_core_proto::Timestamp {
        match &self.inner {
            Inner::Sim { clock, .. } => clock.now(),
            Inner::Real(_) => nexus_core_proto::Timestamp(
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64,
            ),
        }
    }

    /// 一个浏览器：只有一个临时的节点身份，不在名册里，不运行常驻程序。
    /// 它运行的是网页节点核心（浏览器里同一份代码），通行密钥由软件认证器扮演。
    pub async fn browser(&self) -> Browser {
        let n = {
            let mut trace = self.trace.borrow_mut();
            trace.push("browser".into());
            trace.len()
        };
        let transport = match &self.inner {
            Inner::Sim { .. } => {
                // 仿真里身份由种子决定，保持可复现。
                let key = nexus_core_proto::SecretKey::from_seed([(n % 200) as u8 + 50; 32]);
                BrowserTransport::Sim(SimTransport::client(nexus_core_proto::NodeId(key.public().0)))
            }
            Inner::Real(_) => {
                let mut seed = [0u8; 32];
                getrandom::fill(&mut seed).expect("无法取得随机数");
                let t = nexus_fabric_iroh::IrohTransport::bind(seed, nexus_fabric_iroh::Config::local_only())
                    .await
                    .expect("浏览器的传输无法启动");
                BrowserTransport::Real(t)
            }
        };
        Browser { state: RefCell::new(BrowserState::Fresh(transport)), label: format!("browser{n}") }
    }

    /// 和真正的浏览器一样只经中继 `relay` 通信的浏览器（只在真实后端）。
    pub async fn browser_via(&self, relay: &str) -> Browser {
        let Inner::Real(_) = &self.inner else { panic!("browser_via 只用于真实后端") };
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).expect("无法取得随机数");
        let config = nexus_fabric_iroh::Config {
            relay: nexus_fabric_iroh::Relay::Custom(relay.to_string()),
            discovery: false,
            bind: None,
            direct: false,
            scheduling: true,
            rendezvous: None,
        };
        let t = nexus_fabric_iroh::IrohTransport::bind(seed, config).await.expect("浏览器的传输无法启动");
        self.trace.borrow_mut().push("browser".into());
        Browser { state: RefCell::new(BrowserState::Fresh(BrowserTransport::Real(t))), label: "browser-relayed".into() }
    }

    /// 以新的参数（`daemon` 之后的部分，替换默认的 `--local-only`）重启节点的常驻程序，保留数据目录。
    /// 只在真实后端：用于锚点这样需要真实网络服务的设备。
    pub async fn restart(&self, node: &str, args: &[&str]) {
        let Inner::Real(real) = &self.inner else { panic!("restart 只用于真实后端") };
        real.kill(node);
        // 被杀掉的常驻程序留下的控制套接字文件不代表新的已经就绪。
        let _ = std::fs::remove_file(real.dirs[node].join("ctl.sock"));
        real.spawn_daemon(node, args);
        real.wait_ready(node, args);
    }

    /// 让节点的常驻程序用它自己的一份二进制（复制一份），并重新启动。返回那份二进制的路径。
    /// 升级的测试（M7.3）会替换它，不会改到构建出来的那个。只在真实后端。
    pub async fn use_own_binary(&self, node: &str, args: &[&str]) -> PathBuf {
        let Inner::Real(real) = &self.inner else { panic!("use_own_binary 只用于真实后端") };
        let dir = real.home(node).join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nexus");
        std::fs::copy(&real.bin, &path).unwrap();
        real.own_bins.borrow_mut().insert(node.to_string(), path.clone());
        self.restart(node, args).await;
        path
    }

    /// 构建出来的 nexus 二进制。
    pub fn nexus_bin(&self) -> PathBuf {
        let Inner::Real(real) = &self.inner else { panic!("nexus_bin 只用于真实后端") };
        real.bin.clone()
    }

    /// 把节点的常驻程序换成完整节点（虚拟网卡 + 名字服务）重新启动。只在真实后端；需要 CAP_NET_ADMIN。
    pub async fn full_node(&self, node: &str) -> FullNode {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // 同一台机器上并行的测试各用一张网卡、一个网段。
        let tag = (std::process::id() % 1000) * 64 + n % 64;
        let name = format!("nxt{tag}");
        let net = std::net::Ipv4Addr::new(198, 18, (tag % 250) as u8 + 1, 0);
        let net_arg = format!("{net}/24");
        self.restart(node, &["--local-only", "--tun", &name, "--tun-net", &net_arg]).await;
        let o = net.octets();
        FullNode { net, dns: std::net::Ipv4Addr::new(o[0], o[1], o[2], 2) }
    }

    /// 节点的常驻程序到目前为止的日志（只在真实后端）。
    pub fn daemon_log(&self, node: &str) -> String {
        let Inner::Real(real) = &self.inner else { return String::new() };
        std::fs::read_to_string(real.dirs[node].join("daemon.log")).unwrap_or_default()
    }
}

/// 场景里的一个浏览器，见 [`World::browser`]。登录前可以登记通行密钥，登录后访问各设备。
pub struct Browser {
    state: RefCell<BrowserState>,
    label: String,
}

#[allow(clippy::large_enum_variant)] // 测试工具：每个浏览器一个，大小无所谓。
enum BrowserState {
    Fresh(BrowserTransport),
    LoggedIn(Rc<BrowserNode>),
    /// 登录失败：临时身份已经用掉，换一个浏览器重来。
    Spent,
}

enum BrowserTransport {
    Sim(SimTransport),
    Real(nexus_fabric_iroh::IrohTransport),
}

enum BrowserNode {
    Sim(nexus_web_core::WebNode<SimTransport>),
    Real(nexus_web_core::WebNode<nexus_fabric_iroh::IrohTransport>),
}

/// 对两种传输各写一遍同样的调用。
macro_rules! on_node {
    ($node:expr, $n:ident => $body:expr) => {
        match &*$node {
            BrowserNode::Sim($n) => $body,
            BrowserNode::Real($n) => $body,
        }
    };
}

fn wire_err(code: nexus_wire::ErrorCode, m: impl Into<String>) -> nexus_wire::WireError {
    nexus_wire::WireError::new(code, m)
}

impl Browser {
    /// 打开登记链接，用 `auth` 创建通行密钥并登记。返回它在网络里的名字。
    pub async fn enroll(
        &self,
        link: &str,
        auth: &nexus_core_proto::webauthn::soft::SoftAuthenticator,
    ) -> Result<String, nexus_wire::WireError> {
        let link = nexus_web_core::EnrollLink::parse(link).map_err(|m| wire_err(nexus_wire::ErrorCode::InvalidInput, m))?;
        let credential = nexus_wire::NewCredential {
            credential_id: auth.credential_id.clone(),
            spki: auth.spki(),
            rp_id: auth.rp_id.clone(),
        };
        // 等待期间不持有借用：先取出传输，用完放回。
        let state = std::mem::replace(&mut *self.state.borrow_mut(), BrowserState::Spent);
        let BrowserState::Fresh(t) = state else { panic!("{}：登记只能在登录之前", self.label) };
        let result = match &t {
            BrowserTransport::Sim(t) => nexus_web_core::enroll(t, &link, credential).await,
            BrowserTransport::Real(t) => nexus_web_core::enroll(t, &link, credential).await,
        };
        *self.state.borrow_mut() = BrowserState::Fresh(t);
        result
    }

    /// 用通行密钥对这个浏览器的登录挑战签名，签名时间是 `at`。
    pub fn sign(
        &self,
        auth: &mut nexus_core_proto::webauthn::soft::SoftAuthenticator,
        at: nexus_core_proto::Timestamp,
    ) -> nexus_core_proto::webauthn::WebAuthnAssertion {
        let state = self.state.borrow();
        let BrowserState::Fresh(t) = &*state else { panic!("{}：已经登录过", self.label) };
        let challenge = match t {
            BrowserTransport::Sim(t) => nexus_web_core::challenge(t, at),
            BrowserTransport::Real(t) => nexus_web_core::challenge(t, at),
        };
        auth.sign(&challenge)
    }

    /// 签名并登录。入口设备和它的地址提示取自登记链接（真实的页面里入口写在通行密钥里，中继由页面给出）。
    pub async fn login(
        &self,
        w: &World,
        entry_link: &str,
        auth: &mut nexus_core_proto::webauthn::soft::SoftAuthenticator,
    ) -> Result<(), nexus_wire::WireError> {
        let at = w.now();
        let assertion = self.sign(auth, at);
        self.login_with(entry_link, at, assertion).await
    }

    /// 用给定的签名登录。
    pub async fn login_with(
        &self,
        entry_link: &str,
        at: nexus_core_proto::Timestamp,
        assertion: nexus_core_proto::webauthn::WebAuthnAssertion,
    ) -> Result<(), nexus_wire::WireError> {
        let link = nexus_web_core::EnrollLink::parse(entry_link).map_err(|m| wire_err(nexus_wire::ErrorCode::InvalidInput, m))?;
        let state = std::mem::replace(&mut *self.state.borrow_mut(), BrowserState::Spent);
        let BrowserState::Fresh(t) = state else { panic!("{}：每个浏览器只能登录一次", self.label) };
        let node = match t {
            BrowserTransport::Sim(t) => {
                nexus_fabric::Transport::add_hint(&t, link.node, &link.hint);
                BrowserNode::Sim(nexus_web_core::WebNode::login(t, link.node, at, assertion).await?)
            }
            BrowserTransport::Real(t) => {
                nexus_fabric::Transport::add_hint(&t, link.node, &link.hint);
                BrowserNode::Real(nexus_web_core::WebNode::login(t, link.node, at, assertion).await?)
            }
        };
        *self.state.borrow_mut() = BrowserState::LoggedIn(Rc::new(node));
        Ok(())
    }

    fn node(&self) -> Rc<BrowserNode> {
        match &*self.state.borrow() {
            BrowserState::LoggedIn(n) => n.clone(),
            _ => panic!("{}：还没有登录", self.label),
        }
    }

    /// 能看到的设备的名字，按字母排序。
    pub fn devices(&self) -> Vec<String> {
        let node = self.node();
        let mut names: Vec<String> = on_node!(node, n => n.devices().iter().map(|d| d.name.clone()).collect());
        names.sort();
        names
    }

    pub async fn status(&self, device: &str) -> Result<nexus_core_proto::NodeInfo, nexus_wire::WireError> {
        let node = self.node();
        on_node!(node, n => n.status(device).await)
    }

    /// 一台设备上会话的名字。
    pub async fn session_ls(&self, device: &str) -> Result<Vec<String>, nexus_wire::WireError> {
        let node = self.node();
        let list = on_node!(node, n => n.session_list(device).await)?;
        Ok(list.into_iter().map(|s| s.name).collect())
    }

    /// 能读的工作区：（名字，存储设备），按名字排序（M8.1）。
    pub async fn drive_workspaces(&self) -> Result<Vec<(String, String)>, nexus_wire::WireError> {
        let node = self.node();
        let list = on_node!(node, n => n.drive_workspaces().await)?;
        Ok(list.into_iter().map(|w| (w.name, w.device)).collect())
    }

    /// 列出工作区里的目录（M8.1）。
    pub async fn drive_ls(&self, ws: &str, path: &str) -> Result<Vec<nexus_web_core::DriveEntry>, nexus_wire::WireError> {
        let node = self.node();
        on_node!(node, n => n.drive_list(ws, path).await)
    }

    /// 读工作区里文件开头的最多 `max` 字节（M8.1）。
    pub async fn drive_read(&self, ws: &str, path: &str, max: u64) -> Result<nexus_web_core::DriveFile, nexus_wire::WireError> {
        let node = self.node();
        on_node!(node, n => n.drive_read(ws, path, max).await)
    }

    /// 设备上能用的服务（M8.2）。
    pub async fn services(&self, device: &str) -> Result<Vec<nexus_wire::ServiceInfo>, nexus_wire::WireError> {
        let node = self.node();
        on_node!(node, n => n.services(device).await)
    }

    /// 经 Nexus 向设备上的网页服务发一个 HTTP 请求（M8.2）。
    pub async fn http(
        &self,
        device: &str,
        service: &str,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<nexus_web_core::HttpResponse, nexus_wire::WireError> {
        let node = self.node();
        on_node!(node, n => n.http(device, service, method, path, headers, body).await)
    }

    /// 在页面里打开一台设备上的终端会话，80x24。运行的是网页节点核心里页面用的同一份终端代码。
    pub async fn term(&self, device: &str, session: &str) -> Result<Term, nexus_wire::WireError> {
        let node = self.node();
        let size = nexus_wire::Size::DEFAULT;
        let state = Arc::new(TermState::new(size));
        let label = format!("{}（网页）→ {device}/{session}", self.label);
        let (input, keys) = tokio::io::duplex(4096);
        let (resize_tx, resizes) = mpsc::channel(8);
        let mut ui = PageUi { state: state.clone(), keys, resizes, buf: vec![0u8; 4096] };
        // 第一次连接的结果同步返回，之后的事在后台进行。
        let (first_tx, first_rx) = tokio::sync::oneshot::channel();
        let (device, session) = (device.to_string(), session.to_string());
        let st = state.clone();
        tokio::task::spawn_local(async move {
            let mut first = Some(first_tx);
            let result = on_node!(node, n => n.run_terminal(&device, &session, size, &mut FirstOutput(&mut ui, &mut first)).await);
            let code = match result {
                Ok(nexus_web_core::TerminalEnd::Exited(code)) => code.unwrap_or(0),
                Ok(nexus_web_core::TerminalEnd::Left) => 0,
                Err(e) => {
                    if let Some(tx) = first.take() {
                        let _ = tx.send(Err(e));
                        return;
                    }
                    st.feed(format!("\r\n[{}]\r\n", e.message).as_bytes());
                    -1
                }
            };
            if let Some(tx) = first.take() {
                let _ = tx.send(Ok(()));
            }
            *st.exit.lock().unwrap() = Some(code);
        });
        first_rx.await.expect("页面终端的任务提前结束")?;
        Ok(Term { io: TermIo::Sim(SimTerm { input: tokio::sync::Mutex::new(Some(input)), resize: Some(resize_tx) }), state, label })
    }
}

/// 场景里的页面：会话输出喂给内存终端，按键来自 [`Term::send`]，尺寸来自 [`Term::resize`]。
struct PageUi {
    state: Arc<TermState>,
    keys: tokio::io::DuplexStream,
    resizes: mpsc::Receiver<nexus_wire::Size>,
    buf: Vec<u8>,
}

impl nexus_web_core::TerminalUi for PageUi {
    fn output(&mut self, bytes: &[u8]) {
        self.state.feed(bytes);
    }

    fn notice(&mut self, notice: nexus_web_core::Notice) {
        let text: &[u8] = match notice {
            nexus_web_core::Notice::Reconnecting => "\r\n[连接中断，正在重连…]\r\n".as_bytes(),
            nexus_web_core::Notice::Reconnected => b"",
        };
        self.state.feed(text);
    }

    async fn event(&mut self) -> nexus_web_core::UiEvent {
        // 尺寸变化优先：它总是先于之后的按键发生。
        tokio::select! {
            biased;
            Some(s) = self.resizes.recv() => nexus_web_core::UiEvent::Resize(s),
            n = self.keys.read(&mut self.buf) => {
                let data = &self.buf[..n.unwrap_or(0)];
                if data.is_empty() || data.contains(&nexus_cli::DETACH_KEY) {
                    nexus_web_core::UiEvent::Leave
                } else {
                    nexus_web_core::UiEvent::Keys(data.to_vec())
                }
            }
        }
    }
}

/// 第一段输出到来（第一次连接成功）时通知等待的一方。
struct FirstOutput<'a>(&'a mut PageUi, &'a mut Option<tokio::sync::oneshot::Sender<Result<(), nexus_wire::WireError>>>);

impl nexus_web_core::TerminalUi for FirstOutput<'_> {
    fn output(&mut self, bytes: &[u8]) {
        if let Some(tx) = self.1.take() {
            let _ = tx.send(Ok(()));
        }
        self.0.output(bytes);
    }
    fn notice(&mut self, notice: nexus_web_core::Notice) {
        self.0.notice(notice);
    }
    async fn event(&mut self) -> nexus_web_core::UiEvent {
        self.0.event().await
    }
}

/// 后台运行的一条 nexus 命令。
pub struct Background {
    child: Child,
    err_path: PathBuf,
    out_path: PathBuf,
    done: Option<i32>,
}

static SPAWNED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Background {
    /// 像在终端里按下 Ctrl-C 一样中断它，并等它退出。
    pub fn interrupt(mut self) {
        self.signal(nix::sys::signal::Signal::SIGINT);
        let _ = self.child.wait();
    }

    fn signal(&self, sig: nix::sys::signal::Signal) {
        let pid = nix::unistd::Pid::from_raw(self.child.id() as i32);
        let _ = nix::sys::signal::kill(pid, sig);
    }

    /// 它已经结束时返回退出码（被信号杀掉时是 -1），否则 `None`。
    pub fn exited(&mut self) -> Option<i32> {
        if self.done.is_none() {
            if let Ok(Some(st)) = self.child.try_wait() {
                self.done = Some(st.code().unwrap_or(-1));
            }
        }
        self.done
    }

    /// 等它结束，最多等 `limit`。
    pub fn wait_within(&mut self, limit: Duration) -> Option<i32> {
        let start = std::time::Instant::now();
        loop {
            if let Some(code) = self.exited() {
                return Some(code);
            }
            if start.elapsed() >= limit {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 发 SIGTERM，等它结束（最多 10 秒）。
    pub fn terminate(&mut self) -> Option<i32> {
        if self.exited().is_none() {
            self.signal(nix::sys::signal::Signal::SIGTERM);
        }
        self.wait_within(Duration::from_secs(10))
    }

    /// 它写到标准错误的内容。
    pub fn stderr(&self) -> String {
        std::fs::read_to_string(&self.err_path).unwrap_or_default()
    }

    /// 它写到标准输出的内容。
    pub fn stdout(&self) -> String {
        std::fs::read_to_string(&self.out_path).unwrap_or_default()
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        // 先让它自己收尾（例如卸载挂载），不行再杀掉。
        if self.exited().is_none() {
            self.signal(nix::sys::signal::Signal::SIGTERM);
            if self.wait_within(Duration::from_secs(5)).is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
}

/// 场景里的一个终端：把命令的输出喂给一个内存终端，可以输入、暂停读取、等待屏幕上出现内容。
pub struct Term {
    io: TermIo,
    state: Arc<TermState>,
    label: String,
}

enum TermIo {
    Sim(SimTerm),
    Real(RealTerm),
}

struct TermState {
    screen: Mutex<Screen>,
    paused: AtomicBool,
    exit: Mutex<Option<i32>>,
}

struct Screen {
    parser: vt100::Parser,
    /// 还没凑成完整字符或转义序列的输出尾巴。
    pending: Vec<u8>,
}

impl TermState {
    fn new(size: nexus_wire::Size) -> Self {
        let rows = std::num::NonZeroU16::new(size.rows).unwrap();
        let cols = std::num::NonZeroU16::new(size.cols).unwrap();
        Self {
            screen: Mutex::new(Screen { parser: vt100::Parser::new(rows, cols, 0), pending: Vec::new() }),
            paused: AtomicBool::new(false),
            exit: Mutex::new(None),
        }
    }

    fn feed(&self, bytes: &[u8]) {
        let mut s = self.screen.lock().unwrap();
        s.pending.extend_from_slice(bytes);
        // 只在完整的字符和转义序列之间切分，终端模拟库对半个字符的处理有缺陷。
        let cut = nexus_term_proto::safe_prefix(&s.pending);
        let ready: Vec<u8> = s.pending.drain(..cut).collect();
        s.parser.process(&ready);
    }
}

/// 等待屏幕满足条件的默认时限。
const EXPECT_TIMEOUT: Duration = Duration::from_secs(15);

impl Term {
    /// 输入一段文字（`\r` 是回车）。
    pub async fn send(&self, text: &str) {
        match &self.io {
            TermIo::Sim(t) => {
                let mut w = t.input.lock().await;
                if let Some(w) = w.as_mut() {
                    let _ = w.write_all(text.as_bytes()).await;
                }
            }
            TermIo::Real(t) => t.write(text.as_bytes()),
        }
    }

    /// 当前屏幕的文字。
    pub fn screen(&self) -> String {
        self.state.screen.lock().unwrap().parser.screen().contents()
    }

    /// 等到屏幕满足 `pred`，超时则带着屏幕内容失败。
    pub async fn wait_until(&self, what: &str, pred: impl Fn(&vt100::Screen) -> bool) {
        self.wait_until_within(what, EXPECT_TIMEOUT, pred).await
    }

    async fn wait_until_within(&self, what: &str, limit: Duration, pred: impl Fn(&vt100::Screen) -> bool) {
        let start = tokio::time::Instant::now();
        loop {
            if pred(self.state.screen.lock().unwrap().parser.screen()) {
                return;
            }
            if start.elapsed() >= limit {
                panic!("{}：{limit:?} 内没有等到{what}。屏幕：\n{}\n", self.label, self.screen());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// 等到屏幕上出现内容恰好是 `text` 的一行。
    pub async fn expect_line(&self, text: &str) {
        self.expect_line_within(text, EXPECT_TIMEOUT).await
    }

    pub async fn expect_line_within(&self, text: &str, limit: Duration) {
        self.wait_until_within(&format!("一行 {text:?}"), limit, |s| s.contents().lines().any(|l| l.trim_end() == text)).await
    }

    /// 等到屏幕上出现 `text`。
    pub async fn expect_contains(&self, text: &str) {
        self.expect_contains_within(text, EXPECT_TIMEOUT).await
    }

    pub async fn expect_contains_within(&self, text: &str, limit: Duration) {
        self.wait_until_within(&format!("{text:?}"), limit, |s| s.contents().contains(text)).await
    }

    /// 改变终端的尺寸（只用于页面里的终端）。
    pub async fn resize(&self, size: nexus_wire::Size) {
        match &self.io {
            TermIo::Sim(SimTerm { resize: Some(tx), .. }) => tx.send(size).await.expect("终端已经结束"),
            _ => panic!("{}：只有页面里的终端可以改变尺寸", self.label),
        }
    }

    /// 等到 shell 的提示符 `$ ` 出现：之前输入的内容可能在提示符之前回显，打乱屏幕。
    pub async fn expect_prompt(&self) {
        self.expect_contains("$ ").await
    }

    /// 停止读取输出，像一个卡住的终端。
    pub fn pause(&self) {
        self.state.paused.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.state.paused.store(false, Ordering::SeqCst);
    }

    /// 按 Ctrl-] 断开，返回命令的退出码。
    pub async fn detach(&self) -> i32 {
        self.send("\x1d").await;
        self.wait_exit().await
    }

    /// 等命令结束，返回它的退出码。
    pub async fn wait_exit(&self) -> i32 {
        self.wait_exit_within(EXPECT_TIMEOUT).await
    }

    pub async fn wait_exit_within(&self, limit: Duration) -> i32 {
        let start = tokio::time::Instant::now();
        loop {
            if let TermIo::Real(t) = &self.io {
                t.poll_exit(&self.state);
            }
            if let Some(code) = *self.state.exit.lock().unwrap() {
                return code;
            }
            if start.elapsed() >= limit {
                panic!("{}：{limit:?} 内没有结束。屏幕：\n{}\n", self.label, self.screen());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

// 仿真：在场景所在的任务里直接运行 `nexus ssh` 的客户端代码，用内存管道代替终端。
struct SimTerm {
    input: tokio::sync::Mutex<Option<tokio::io::DuplexStream>>,
    /// 只有页面里的终端可以改变尺寸。
    resize: Option<mpsc::Sender<nexus_wire::Size>>,
}

fn sim_term(tx: mpsc::Sender<CtlCall>, args: SshArgs, agent: Option<AgentCredential>, state: Arc<TermState>) -> SimTerm {
    let (input, mut stdin) = tokio::io::duplex(4096);
    // 输出管道很小：暂停读取时，客户端很快就写不进去，不再确认输出。
    let (mut stdout, mut out_rd) = tokio::io::duplex(16 * 1024);
    let st = state.clone();
    tokio::task::spawn_local(async move {
        let ctl = ChanCtl(tx, agent);
        let (_keep, mut resizes) = mpsc::channel(1);
        let code = match ssh(&ctl, &args, &mut stdin, &mut stdout, &mut resizes).await {
            Ok(code) => code,
            Err(e) => ssh_fail(&mut Captured::default(), &e),
        };
        drop(stdout);
        *st.exit.lock().unwrap() = Some(code);
    });
    tokio::task::spawn_local(async move {
        let mut buf = vec![0u8; 4096];
        loop {
            if state.paused.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            match out_rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => state.feed(&buf[..n]),
            }
        }
    });
    SimTerm { input: tokio::sync::Mutex::new(Some(input)), resize: None }
}

// 真实后端：真正的 `nexus ssh` 进程，标准输入和输出都是管道。
struct RealTerm {
    child: RefCell<Child>,
    stdin: RefCell<Option<std::process::ChildStdin>>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl RealTerm {
    fn write(&self, bytes: &[u8]) {
        use std::io::Write;
        if let Some(w) = self.stdin.borrow_mut().as_mut() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    fn poll_exit(&self, state: &TermState) {
        if let Ok(Some(status)) = self.child.borrow_mut().try_wait() {
            let mut exit = state.exit.lock().unwrap();
            if exit.is_none() {
                let code = status.code().unwrap_or(-1);
                if (100..120).contains(&code) {
                    eprintln!("nexus ssh 的错误输出：{}", String::from_utf8_lossy(&self.stderr.lock().unwrap()));
                }
                *exit = Some(code);
            }
        }
    }
}

impl Drop for RealTerm {
    fn drop(&mut self) {
        let mut child = self.child.borrow_mut();
        let _ = child.kill();
        let _ = child.wait();
    }
}

struct ChanCtl(mpsc::Sender<CtlCall>, Option<AgentCredential>);

impl Ctl for ChanCtl {
    async fn open(&self, req: CtlRequest) -> io::Result<CtlClient> {
        let (call, client) = ctl_session(req, self.1.clone());
        self.0
            .send(call)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::ConnectionRefused, "常驻程序没有在运行"))?;
        Ok(client)
    }
}

// ---------------------------------------------------------------- 仿真后端

/// 本次运行使用的种子。`NEXUS_SIM_SEED=n` 只跑一个指定的种子（用于复现），
/// `NEXUS_SIM_SEEDS=k` 指定种子个数（默认 20）。
pub fn default_seeds() -> Vec<u64> {
    if let Some(seed) = std::env::var("NEXUS_SIM_SEED").ok().and_then(|s| s.parse().ok()) {
        return vec![seed];
    }
    let count = std::env::var("NEXUS_SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
    (1..=count).collect()
}

/// 在仿真后端上运行场景。每个种子跑两遍，两遍的事件轨迹必须完全相同。
pub fn run_sim<F, Fut>(nodes: &[&str], seeds: Vec<u64>, scenario: F)
where
    F: Fn(World) -> Fut,
    Fut: Future<Output = ()> + 'static,
{
    for seed in seeds {
        let _hint = SeedHint(seed);
        let first = sim_once(nodes, seed, &scenario);
        let second = sim_once(nodes, seed, &scenario);
        assert_eq!(first, second, "同一个种子的两次运行出现了不同的事件轨迹");
    }
}

/// 运行一次仿真，返回事件轨迹的摘要。
pub fn sim_once<F, Fut>(nodes: &[&str], seed: u64, scenario: &F) -> u64
where
    F: Fn(World) -> Fut,
    Fut: Future<Output = ()> + 'static,
{
    let mut builder = turmoil::Builder::new();
    builder
        .rng_seed(seed)
        .min_message_latency(Duration::from_millis(1))
        .max_message_latency(Duration::from_millis(40))
        .simulation_duration(Duration::from_secs(30 * 24 * 3600));
    let mut sim = builder.build();

    // 每个节点从零开始：没有名册，互不认识，和刚装好的设备一样。
    let clock = SimClock::default();
    let mut ctl = HashMap::new();
    let mut all_services = HashMap::new();
    let mut proxies = HashMap::new();
    for (i, name) in nodes.iter().enumerate() {
        let (ptx, prx) = mpsc::unbounded_channel::<ProxyStream>();
        proxies.insert(name.to_string(), ptx);
        let prx = Rc::new(tokio::sync::Mutex::new(prx));
        let disk = SimDisk::default();
        let services = SimServices::default();
        all_services.insert(name.to_string(), services.clone());
        let (tx, rx) = mpsc::channel::<CtlCall>(64);
        ctl.insert(name.to_string(), tx);
        let rx = Rc::new(tokio::sync::Mutex::new(rx));
        let (name, clock) = (name.to_string(), clock.clone());
        sim.host(name.clone(), move || {
            let (disk, name, rx, clock, services, prx) = (disk.clone(), name.clone(), rx.clone(), clock.clone(), services.clone(), prx.clone());
            async move {
                let system = SimSystem::with_services(disk, node_seed(seed, i), clock, services);
                let key = load_or_create_device_key(&system)?;
                let transport = SimTransport::bind(nexus_core_proto::NodeId(key.public().0), &name).await?;
                let node = Node::new(transport, system, &name)?;
                let mut rx = rx.lock().await;
                // 入口上的连接转给这一次启动的节点（节点重启后换成新的节点）。
                let entry = node.proxy_entry();
                let forward = async {
                    let mut prx = prx.lock().await;
                    while let Some(conn) = prx.recv().await {
                        let _ = entry.send(conn);
                    }
                    std::future::pending::<()>().await
                };
                tokio::select! {
                    r = node.run(&mut rx) => r?,
                    _ = forward => {}
                }
                Ok(())
            }
        });
    }

    // 碰头服务（短加入码，M6.1）。
    sim.host(nexus_fabric_sim::RENDEZVOUS_HOST, || async {
        nexus_fabric_sim::serve_rendezvous().await?;
        Ok(())
    });

    let trace = Rc::new(RefCell::new(Vec::new()));
    let world = World {
        inner: Inner::Sim { nodes: ctl, clock, services: all_services, next_port: std::cell::Cell::new(8000), proxies },
        trace: trace.clone(),
    };
    let fut = scenario(world);
    sim.client(SCENARIO_HOST, async move {
        fut.await;
        Ok(())
    });
    sim.run().expect("仿真运行失败");

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    trace.borrow().hash(&mut hasher);
    hasher.finish()
}

fn node_seed(seed: u64, index: usize) -> u64 {
    seed.wrapping_mul(1_000_003).wrapping_add(index as u64)
}

struct SeedHint(u64);

impl Drop for SeedHint {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("\n仿真场景失败，种子 = {0}。复现：NEXUS_SIM_SEED={0} cargo test <测试名>\n", self.0);
        }
    }
}

// ---------------------------------------------------------------- 真实后端

struct RealWorld {
    bin: PathBuf,
    dirs: HashMap<String, PathBuf>,
    children: RefCell<HashMap<String, Child>>,
    /// 各节点统一入口的端口。
    proxy_ports: RefCell<HashMap<String, u16>>,
    _tmp: tempfile::TempDir,
    /// 各节点的 agent 入口放在这里：其他系统账号也能进入这个目录。
    sockets: tempfile::TempDir,
    /// 本机的碰头服务（短加入码，M6.1），所有常驻程序默认用它。
    rendezvous: String,
    /// 用自己的一份二进制的节点（升级的测试，M7.3）。
    own_bins: RefCell<HashMap<String, PathBuf>>,
}

impl RealWorld {
    fn start(nodes: &[&str], bin: &str) -> Self {
        let tmp = tempfile::tempdir().expect("无法创建临时目录");
        let sockets = tempfile::tempdir().expect("无法创建临时目录");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(sockets.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let rendezvous = format!("http://127.0.0.1:{}/pkarr", start_rendezvous());
        let mut world = RealWorld {
            bin: bin.into(),
            dirs: HashMap::new(),
            children: RefCell::default(),
            proxy_ports: RefCell::default(),
            _tmp: tmp,
            sockets,
            rendezvous,
            own_bins: RefCell::default(),
        };
        for name in nodes {
            let dir = world._tmp.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            world.dirs.insert(name.to_string(), dir);
            world.spawn_daemon(name, &["--local-only"]);
        }
        for name in nodes {
            world.wait_ready(name, &["--local-only"]);
        }
        world
    }

    /// 在本机回环地址上运行一个测试服务，直到测试进程结束。
    ///
    /// 服务跑在自己的线程里：执行命令行时测试线程会同步等待子进程，服务不能依赖测试线程。
    async fn serve(&self, service: Service) -> u16 {
        serve_on(([127, 0, 0, 1], 0).into(), service)
    }

    fn proxy_port(&self, node: &str) -> u16 {
        *self.proxy_ports.borrow().get(node).unwrap_or_else(|| panic!("节点 {node} 没有开统一入口"))
    }

    /// 启动节点的常驻程序，`extra` 是 `daemon` 之后的其他参数。没有给 `--proxy` 时开一个本机统一入口。
    fn spawn_daemon(&self, name: &str, extra: &[&str]) {
        let dir = &self.dirs[name];
        let mut proxy = Vec::new();
        if !extra.contains(&"--proxy") {
            let port = *self.proxy_ports.borrow_mut().entry(name.to_string()).or_insert_with(|| {
                std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
            });
            proxy = vec!["--proxy".to_string(), format!("127.0.0.1:{port}")];
        }
        let _ = std::fs::remove_file(dir.join("ctl.sock"));
        let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("daemon.log")).unwrap();
        let rendezvous: Vec<&str> = if extra.contains(&"--rendezvous") { vec![] } else { vec!["--rendezvous", &self.rendezvous] };
        let bin = self.own_bins.borrow().get(name).cloned().unwrap_or_else(|| self.bin.clone());
        let mut cmd = Command::new(&bin);
        cmd.args(["--data-dir", dir.to_str().unwrap(), "daemon", "--name", name])
            .args(extra)
            .args(&rendezvous)
            .args(&proxy)
            .arg("--agent-socket")
            .arg(self.sockets.path().join(format!("{name}.sock")))
            // 终端会话里用一个行为确定的 shell，提示符和仿真里一样是 `$ `。
            .env("SHELL", "/bin/sh")
            .env("PS1", "$ ")
            .env_remove("ENV")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        // 刚复制出来的二进制（use_own_binary）：并行的测试里别的线程 fork 出的子进程在 exec 之前还持有它的写句柄，
        // 这时执行会得到 ETXTBSY。等一下再试。
        let mut tries = 0;
        let child = loop {
            match cmd.spawn() {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 100 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => break other.expect("无法启动 nexus daemon"),
            }
        };
        self.children.borrow_mut().insert(name.to_string(), child);
    }

    /// 等常驻程序就绪。`extra` 和启动时的相同：统一入口的端口被别的进程抢先占了时，换一个端口重启
    /// （端口是先绑定再放开选出来的，并行的测试之间难免撞上）。
    fn wait_ready(&self, name: &str, extra: &[&str]) {
        let dir = &self.dirs[name];
        let mut deadline = Instant::now() + Duration::from_secs(10);
        let mut retries = 0;
        let log_len = || std::fs::metadata(dir.join("daemon.log")).map(|m| m.len()).unwrap_or(0);
        let mut log_from = 0;
        while !dir.join("ctl.sock").exists() {
            let exited = self.children.borrow_mut().get_mut(name).and_then(|c| c.try_wait().ok().flatten());
            if Instant::now() >= deadline || exited.is_some() {
                let log = std::fs::read_to_string(dir.join("daemon.log")).unwrap_or_default();
                let this_run = log.get(log_from as usize..).unwrap_or("");
                if exited.is_some() && retries < 5 && !extra.contains(&"--proxy") && this_run.contains("无法监听统一入口") {
                    retries += 1;
                    log_from = log_len();
                    self.proxy_ports.borrow_mut().remove(name);
                    self.spawn_daemon(name, extra);
                    deadline = Instant::now() + Duration::from_secs(10);
                    continue;
                }
                panic!("节点 {name} 没有就绪（{exited:?}）。日志：\n{log}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn kill(&self, node: &str) {
        if let Some(mut child) = self.children.borrow_mut().remove(node) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn home(&self, node: &str) -> PathBuf {
        let dir = self.dirs.get(node).unwrap_or_else(|| panic!("场景里没有节点 {node}")).join("home");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn agent_socket(&self, node: &str) -> PathBuf {
        assert!(self.dirs.contains_key(node), "场景里没有节点 {node}");
        self.sockets.path().join(format!("{node}.sock"))
    }

    fn host(&self, node: &str, uid: Option<u32>, argv: &[&str], stdin: &[u8]) -> Output {
        use std::io::Write;
        use std::os::unix::process::CommandExt;
        let dir = self.dirs.get(node).unwrap_or_else(|| panic!("场景里没有节点 {node}"));
        let bin_dir = self.bin.parent().unwrap();
        let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default());
        let mut cmd = Command::new(argv[0]);
        cmd.args(&argv[1..])
            .env("PATH", path)
            .env("NEXUS_AGENT_SOCKET", self.agent_socket(node))
            .env_remove("NEXUS_AGENT")
            .env_remove("SSH_AUTH_SOCK");
        match uid {
            // 本机上的另一个系统账号：进不了用户的数据目录，只能用 agent 入口。
            Some(uid) => {
                cmd.uid(uid)
                    .gid(uid)
                    .env("HOME", "/tmp")
                    .env("NEXUS_DATA_DIR", dir)
                    .current_dir("/");
            }
            None => {
                cmd.env("HOME", self.home(node)).env("NEXUS_DATA_DIR", dir).current_dir(self.home(node));
            }
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("无法运行 {}：{e}", argv[0]));
        let mut input = child.stdin.take().unwrap();
        let data = stdin.to_vec();
        let writer = std::thread::spawn(move || {
            let _ = input.write_all(&data);
        });
        let out = child.wait_with_output().expect("等待程序结束失败");
        let _ = writer.join();
        Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            stdout_raw: out.stdout,
        }
    }

    fn signal(&self, node: &str, sig: nix::sys::signal::Signal) {
        let children = self.children.borrow();
        let child = children.get(node).unwrap_or_else(|| panic!("节点 {node} 没有在运行"));
        let pid = nix::unistd::Pid::from_raw(child.id() as i32);
        nix::sys::signal::kill(pid, sig).expect("无法向常驻程序发送信号");
    }

    fn term(&self, node: &str, args: &[&str], state: Arc<TermState>) -> RealTerm {
        use std::io::Read;
        let dir = self.dirs.get(node).unwrap_or_else(|| panic!("场景里没有节点 {node}"));
        let mut child = Command::new(&self.bin)
            .args(["--data-dir", dir.to_str().unwrap()])
            .args(args)
            .env("NEXUS_AGENT_SOCKET", self.agent_socket(node))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("无法执行 nexus");
        let mut out = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            loop {
                // 暂停时不读：管道很快写满，客户端就不再确认输出。
                while state.paused.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                match out.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => state.feed(&buf[..n]),
                }
            }
        });
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let mut err = child.stderr.take().unwrap();
        let sink = stderr.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err.read_to_end(&mut buf);
            sink.lock().unwrap().extend(buf);
        });
        let stdin = child.stdin.take();
        RealTerm { child: RefCell::new(child), stdin: RefCell::new(stdin), stderr }
    }

    fn cli(&self, node: &str, args: &[&str], stdin: &[u8]) -> Output {
        use std::io::Write;
        let dir = self.dirs.get(node).unwrap_or_else(|| panic!("场景里没有节点 {node}"));
        // 标准输入是管道而不是终端：任何交互提示都会让命令失败。
        let mut child = Command::new(&self.bin)
            .args(["--data-dir", dir.to_str().unwrap()])
            .args(args)
            .env("NEXUS_AGENT_SOCKET", self.agent_socket(node))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("无法执行 nexus");
        let mut pipe = child.stdin.take().unwrap();
        let data = stdin.to_vec();
        let writer = std::thread::spawn(move || {
            let _ = pipe.write_all(&data);
        });
        let out = child.wait_with_output().expect("无法等待 nexus");
        let _ = writer.join();
        let code = out.status.code().unwrap_or(-1);
        Output::from_captured(code, Captured { stdout: out.stdout, stderr: out.stderr })
    }
}

impl Drop for RealWorld {
    fn drop(&mut self) {
        for child in self.children.borrow_mut().values_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // 终端会话的持有进程（M7.2）不随常驻程序结束：按它们留下的 pid 文件结束它们（连同 shell）。
        for dir in self.dirs.values() {
            let Ok(entries) = std::fs::read_dir(dir.join("sessions")) else { continue };
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().is_some_and(|x| x == "pid") {
                    if let Some(pid) = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<i32>().ok()) {
                        // 持有进程自成一个进程组，shell 在它下面：结束整个进程组和 shell 的进程组。
                        let _ = Command::new("pkill").args(["-KILL", "-P", &pid.to_string()]).status();
                        let _ = Command::new("kill").args(["-KILL", &pid.to_string()]).status();
                    }
                }
            }
        }
    }
}

/// 在真实后端上运行场景。`bin` 是 `nexus` 二进制的路径。
pub fn run_real<F, Fut>(nodes: &[&str], bin: &str, scenario: F)
where
    F: Fn(World) -> Fut,
    Fut: Future<Output = ()> + 'static,
{
    let world = World {
        inner: Inner::Real(RealWorld::start(nodes, bin)),
        trace: Rc::new(RefCell::new(Vec::new())),
    };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    // 和仿真一样在单线程的本地任务集里运行，场景可以启动本地任务（例如页面里的终端）。
    tokio::task::LocalSet::new().block_on(&rt, scenario(world));
}

/// 声明一个验收场景，并为它生成 `sim` 和 `real` 两个测试。
///
/// `covers` 列出它验证的需求编号，`tools/check.py` 据此检查需求是否都有测试。
/// `real: yes` 表示也在真实后端上运行；`real: (pending "原因")` 表示真实后端暂时无法运行。
#[macro_export]
macro_rules! scenario {
    // 只适用于真实后端的场景（例如需要系统自带的程序）：仿真版本标为忽略并写明原因。
    (name: $name:ident, covers: $covers:literal, nodes: [$($node:literal),+], sim: (pending $why:literal), real: yes, run: $body:expr $(,)?) => {
        mod $name {
            #[allow(unused_imports)]
            use super::*;

            #[test]
            #[ignore = $why]
            fn sim() {}

            $crate::__real_test!(yes, [$($node),+], $body);
        }
    };
    (name: $name:ident, covers: $covers:literal, nodes: [$($node:literal),+], real: $real:tt, run: $body:expr $(,)?) => {
        mod $name {
            #[allow(unused_imports)]
            use super::*;

            #[test]
            fn sim() {
                $crate::run_sim(&[$($node),+], $crate::default_seeds(), $body);
            }

            $crate::__real_test!($real, [$($node),+], $body);
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __real_test {
    (yes, [$($node:literal),+], $body:expr) => {
        #[test]
        fn real() {
            $crate::run_real(&[$($node),+], env!("CARGO_BIN_EXE_nexus"), $body);
        }
    };
    ((pending $why:literal), [$($node:literal),+], $body:expr) => {
        #[test]
        #[ignore = $why]
        fn real() {
            $crate::run_real(&[$($node),+], env!("CARGO_BIN_EXE_nexus"), $body);
        }
    };
}

/// 经统一入口的请求失败的方式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxyError {
    /// SOCKS5 回应了这个错误码。
    Socks5(u8),
    /// HTTP 代理的 CONNECT 回应了这个状态码。
    Http(u16),
    /// 连接意外断开或回应无法理解。
    Io(String),
}

/// 一个节点的本机统一入口，像普通程序那样使用它。
pub enum ProxyClient {
    Sim(mpsc::UnboundedSender<ProxyStream>),
    Real(u16),
}

trait Duplex: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin {}
impl<X: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin> Duplex for X {}

const PROXY_CLIENT_TIMEOUT: Duration = Duration::from_secs(20);

impl ProxyClient {
    async fn open(&self) -> Result<Box<dyn Duplex>, ProxyError> {
        match self {
            ProxyClient::Sim(tx) => {
                let (near, far) = tokio::io::duplex(256 * 1024);
                tx.send(Box::new(far)).map_err(|_| ProxyError::Io("节点没有在运行".into()))?;
                Ok(Box::new(near))
            }
            ProxyClient::Real(port) => {
                let s = tokio::net::TcpStream::connect(("127.0.0.1", *port)).await.map_err(io_err)?;
                Ok(Box::new(s))
            }
        }
    }

    /// 经 SOCKS5 连到 `host:port`，发一个 HTTP GET，返回正文。
    pub async fn socks5_http_get(&self, host: &str, port: u16, path: &str) -> Result<String, ProxyError> {
        let go = async {
            let mut s = self.open().await?;
            s.write_all(&[5, 1, 0]).await.map_err(io_err)?;
            let mut greet = [0u8; 2];
            s.read_exact(&mut greet).await.map_err(io_err)?;
            if greet != [5, 0] {
                return Err(ProxyError::Io(format!("问候的回应不对：{greet:?}")));
            }
            let mut req = vec![5, 1, 0];
            match host.parse::<std::net::IpAddr>() {
                Ok(std::net::IpAddr::V4(ip)) => {
                    req.push(1);
                    req.extend_from_slice(&ip.octets());
                }
                Ok(std::net::IpAddr::V6(ip)) => {
                    req.push(4);
                    req.extend_from_slice(&ip.octets());
                }
                Err(_) => {
                    req.push(3);
                    req.push(host.len() as u8);
                    req.extend_from_slice(host.as_bytes());
                }
            }
            req.extend_from_slice(&port.to_be_bytes());
            s.write_all(&req).await.map_err(io_err)?;
            let mut reply = [0u8; 10];
            s.read_exact(&mut reply).await.map_err(io_err)?;
            if reply[1] != 0 {
                return Err(ProxyError::Socks5(reply[1]));
            }
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).await.map_err(io_err)?;
            let raw = read_all(&mut s).await?;
            let (_, body) = split_response(&raw)?;
            Ok(body)
        };
        tokio::time::timeout(PROXY_CLIENT_TIMEOUT, go).await.unwrap_or_else(|_| Err(ProxyError::Io("超时".into())))
    }

    /// 经 HTTP 代理发一个 GET（绝对形式的地址），返回状态码和正文。
    pub async fn http_get(&self, url: &str) -> Result<(u16, String), ProxyError> {
        let go = async {
            let mut s = self.open().await?;
            let host = url.trim_start_matches("http://").split('/').next().unwrap_or_default();
            s.write_all(format!("GET {url} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).await.map_err(io_err)?;
            let raw = read_all(&mut s).await?;
            split_response(&raw)
        };
        tokio::time::timeout(PROXY_CLIENT_TIMEOUT, go).await.unwrap_or_else(|_| Err(ProxyError::Io("超时".into())))
    }

    /// 经 HTTP 代理的 CONNECT 建立隧道，送出 `data` 后关闭写方向，返回读到的全部字节。
    pub async fn connect_tunnel(&self, target: &str, data: &[u8]) -> Result<Vec<u8>, ProxyError> {
        let go = async {
            let mut s = self.open().await?;
            s.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes()).await.map_err(io_err)?;
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if s.read(&mut byte).await.map_err(io_err)? == 0 {
                    return Err(ProxyError::Io("CONNECT 没有回应".into()));
                }
                head.push(byte[0]);
            }
            let status = status_of(&head)?;
            if status != 200 {
                return Err(ProxyError::Http(status));
            }
            s.write_all(data).await.map_err(io_err)?;
            s.shutdown().await.map_err(io_err)?;
            read_all(&mut s).await
        };
        tokio::time::timeout(PROXY_CLIENT_TIMEOUT, go).await.unwrap_or_else(|_| Err(ProxyError::Io("超时".into())))
    }

    /// 送出任意字节，返回入口回的全部字节（它关闭连接为止）。
    pub async fn raw(&self, data: &[u8]) -> Vec<u8> {
        let go = async {
            let mut s = self.open().await?;
            s.write_all(data).await.map_err(io_err)?;
            read_all(&mut s).await
        };
        tokio::time::timeout(PROXY_CLIENT_TIMEOUT, go).await.expect("入口没有关闭连接").unwrap_or_default()
    }
}

fn io_err(e: io::Error) -> ProxyError {
    ProxyError::Io(e.to_string())
}

async fn read_all(s: &mut Box<dyn Duplex>) -> Result<Vec<u8>, ProxyError> {
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.map_err(io_err)?;
    Ok(out)
}

fn status_of(head: &[u8]) -> Result<u16, ProxyError> {
    let text = String::from_utf8_lossy(head);
    text.split(' ').nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| ProxyError::Io(format!("回应无法理解：{text:?}")))
}

fn split_response(raw: &[u8]) -> Result<(u16, String), ProxyError> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| ProxyError::Io(format!("回应没有头部：{:?}", String::from_utf8_lossy(raw))))?;
    Ok((status_of(&raw[..split])?, String::from_utf8_lossy(&raw[split + 4..]).into_owned()))
}

/// 一个 HTTP GET（Connection: close），返回正文。用于验证端口转发这类真实的本机网络。
pub fn http_get(port: u16, path: &str) -> io::Result<String> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| io::Error::other("回应没有头部"))?;
    Ok(String::from_utf8_lossy(&raw[split + 4..]).into_owned())
}

/// 一个完整节点，见 [`World::full_node`]。用系统的网络栈访问它，就像普通程序那样。
pub struct FullNode {
    net: std::net::Ipv4Addr,
    dns: std::net::Ipv4Addr,
}

impl FullNode {
    pub fn net(&self) -> (std::net::Ipv4Addr, u8) {
        (self.net, 24)
    }

    pub fn dns(&self) -> std::net::Ipv4Addr {
        self.dns
    }

    /// 地址是否在完整节点的网段里（名字服务、本机一侧除外）。
    pub fn contains(&self, ip: std::net::IpAddr) -> bool {
        match ip {
            std::net::IpAddr::V4(v4) => {
                let (a, b) = (v4.octets(), self.net.octets());
                a[..3] == b[..3] && a[3] > 2 && a[3] < 255
            }
            _ => false,
        }
    }

    /// 向名字服务查询（qtype 1 是 IPv4，28 是 IPv6）。返回地址，或出错的 rcode。
    pub fn resolve(&self, name: &str, qtype: u16) -> Result<Vec<std::net::IpAddr>, u8> {
        let mut q = vec![0xab, 0xcd, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.extend_from_slice(&[0, (qtype >> 8) as u8, qtype as u8, 0, 1]);
        let sock = std::net::UdpSocket::bind("0.0.0.0:0").expect("UDP");
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 1500];
        let n = (0..5)
            .find_map(|_| {
                sock.send_to(&q, (self.dns, 53)).ok()?;
                sock.recv(&mut buf).ok()
            })
            .unwrap_or_else(|| panic!("名字服务 {} 没有回应 {name}", self.dns));
        let r = &buf[..n];
        assert_eq!(&r[..2], &[0xab, 0xcd], "回应的 id 不对");
        let rcode = r[3] & 0x0f;
        if rcode != 0 {
            return Err(rcode);
        }
        let answers = u16::from_be_bytes([r[6], r[7]]);
        // 跳过问题段，逐条读回答（名字都是压缩指针）。
        let mut pos = q.len();
        let mut out = Vec::new();
        for _ in 0..answers {
            assert_eq!(r[pos] & 0xc0, 0xc0, "回答的名字应当是压缩指针");
            let rtype = u16::from_be_bytes([r[pos + 2], r[pos + 3]]);
            let len = u16::from_be_bytes([r[pos + 10], r[pos + 11]]) as usize;
            let data = &r[pos + 12..pos + 12 + len];
            match (rtype, len) {
                (1, 4) => out.push(std::net::IpAddr::from(<[u8; 4]>::try_from(data).unwrap())),
                (28, 16) => out.push(std::net::IpAddr::from(<[u8; 16]>::try_from(data).unwrap())),
                _ => panic!("意外的回答类型 {rtype}"),
            }
            pos += 12 + len;
        }
        Ok(out)
    }

    /// 用系统的网络栈连 `ip:port`，发一个 HTTP GET，返回正文；连接被关闭时正文为空。
    pub fn http_get(&self, ip: std::net::IpAddr, port: u16, path: &str) -> io::Result<String> {
        http_get_at((ip, port).into(), path)
    }
}

/// 在本机临时目录里建一棵文件树（测试 `nexus drive import` 用）。`files` 是（相对路径，内容）。
///
/// 路径只取决于进程、测试线程和内容：仿真里同一个种子跑两遍时命令行参数相同，事件轨迹才能一致。
/// 返回值被丢弃时删除目录。
pub fn local_tree(files: &[(&str, &[u8])]) -> LocalTree {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    files.hash(&mut h);
    let thread = std::thread::current().name().unwrap_or("main").replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let dir = std::env::temp_dir().join(format!("nexus-tree-{}-{thread}-{:016x}", std::process::id(), h.finish()));
    let _ = std::fs::remove_dir_all(&dir);
    for (path, content) in files {
        let p = dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    LocalTree(dir)
}

/// 本机临时目录里一个空目录，名字取决于进程、测试线程和 `tag`（同一个测试里要几个时用不同的 `tag`）。
pub fn local_dir(tag: &str) -> LocalTree {
    let thread = std::thread::current().name().unwrap_or("main").replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let dir = std::env::temp_dir().join(format!("nexus-dir-{}-{thread}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    LocalTree(dir)
}

pub struct LocalTree(std::path::PathBuf);

impl LocalTree {
    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for LocalTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 可重复的伪随机字节（测试大文件用）。
pub fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// 一个 HTTP GET（Connection: close），返回正文；对方不回应就关闭时正文为空。
pub fn http_get_at(addr: std::net::SocketAddr, path: &str) -> io::Result<String> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    if raw.is_empty() {
        return Ok(String::new());
    }
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| io::Error::other("回应没有头部"))?;
    Ok(String::from_utf8_lossy(&raw[split + 4..]).into_owned())
}

/// 在自己的线程里运行一个测试服务（测试的命令行是同步等待的，不能和它共用线程）。
/// 在本机起一个碰头服务，返回端口。服务跑到测试进程结束。
fn start_rendezvous() -> u16 {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("无法监听");
    let port = std_listener.local_addr().unwrap().port();
    std_listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().expect("碰头服务的运行时");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(std_listener).expect("无法监听");
            let _ = nexus_fabric_iroh::rendezvous::serve(listener, Default::default()).await;
        });
    });
    port
}

fn serve_on(addr: std::net::SocketAddr, service: Service) -> u16 {
    let std_listener = std::net::TcpListener::bind(addr).expect("无法监听");
    let port = std_listener.local_addr().unwrap().port();
    std_listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("测试服务的运行时");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(std_listener).expect("无法监听");
            loop {
                let Ok((s, _)) = listener.accept().await else { return };
                tokio::spawn(run_test_service(service.clone(), s));
            }
        });
    });
    port
}

/// 这台机器对外的地址（不发任何数据包：UDP 的 connect 只选路由）。
fn public_ip() -> std::net::IpAddr {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").expect("无法打开 UDP");
    s.connect("203.0.113.1:9").expect("这台机器没有对外的路由，无法测试出口");
    let ip = s.local_addr().unwrap().ip();
    assert!(!ip.is_loopback() && !ip.is_unspecified(), "这台机器没有非本机地址：{ip}");
    ip
}

async fn run_test_service(service: Service, mut s: tokio::net::TcpStream) {
    match service {
        Service::Socks5Upstream => {
            use nexus_net_core::socks5;
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            let mut read_more = async |s: &mut tokio::net::TcpStream, buf: &mut Vec<u8>| match s.read(&mut chunk).await {
                Ok(0) | Err(_) => false,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    true
                }
            };
            let used = loop {
                match socks5::parse_greeting(&buf) {
                    Ok(Some((n, _))) => break n,
                    Ok(None) if read_more(&mut s, &mut buf).await => {}
                    _ => return,
                }
            };
            buf.drain(..used);
            let _ = s.write_all(&socks5::NO_AUTH).await;
            let target = loop {
                match socks5::parse_request(&buf) {
                    Ok(Some((n, t))) => {
                        buf.drain(..n);
                        break t;
                    }
                    Ok(None) if read_more(&mut s, &mut buf).await => {}
                    _ => return,
                }
            };
            if matches!(&target.host, nexus_net_core::Host::Name(h) if h.starts_with("refuse.")) {
                let _ = s.write_all(&socks5::reply(socks5::Reply::ConnectionRefused)).await;
                let _ = s.shutdown().await;
                return;
            }
            let _ = s.write_all(&socks5::reply(socks5::Reply::Succeeded)).await;
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                if !read_more(&mut s, &mut buf).await {
                    return;
                }
            }
            let body = format!("via upstream to {target}");
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(reply.as_bytes()).await;
            let _ = s.shutdown().await;
        }
        Service::Echo => {
            let (mut r, mut w) = s.split();
            let _ = tokio::io::copy(&mut r, &mut w).await;
            let _ = w.shutdown().await;
        }
        Service::HttpEcho => {
            let mut req = Vec::new();
            let mut buf = [0u8; 1024];
            let reply = loop {
                if let Some(r) = nexus_sys_sim::http_echo_reply(&req) {
                    break r;
                }
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            };
            let _ = s.write_all(&reply).await;
            let _ = s.shutdown().await;
        }
        Service::Http(body) => {
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(reply.as_bytes()).await;
            let _ = s.shutdown().await;
        }
    }
}
