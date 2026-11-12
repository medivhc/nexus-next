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
use nexus_runtime::{ctl_session, load_or_create_device_key, CtlCall, CtlClient, Node};
use nexus_sys_sim::{SimClock, SimDisk, SimSystem};
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
    Sim { nodes: HashMap<String, mpsc::Sender<CtlCall>>, clock: SimClock },
    Real(RealWorld),
}

impl World {
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
                TermIo::Sim(sim_term(tx, ssh, state.clone()))
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
        self.wait_until(&format!("{text:?}"), |s| s.contents().contains(text)).await
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
        let start = tokio::time::Instant::now();
        loop {
            if let TermIo::Real(t) = &self.io {
                t.poll_exit(&self.state);
            }
            if let Some(code) = *self.state.exit.lock().unwrap() {
                return code;
            }
            if start.elapsed() >= EXPECT_TIMEOUT {
                panic!("{}：{EXPECT_TIMEOUT:?} 内没有结束。屏幕：\n{}\n", self.label, self.screen());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

// 仿真：在场景所在的任务里直接运行 `nexus ssh` 的客户端代码，用内存管道代替终端。
struct SimTerm {
    input: tokio::sync::Mutex<Option<tokio::io::DuplexStream>>,
}

fn sim_term(tx: mpsc::Sender<CtlCall>, args: SshArgs, state: Arc<TermState>) -> SimTerm {
    let (input, mut stdin) = tokio::io::duplex(4096);
    // 输出管道很小：暂停读取时，客户端很快就写不进去，不再确认输出。
    let (mut stdout, mut out_rd) = tokio::io::duplex(16 * 1024);
    let st = state.clone();
    tokio::task::spawn_local(async move {
        let ctl = ChanCtl(tx, None);
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
    SimTerm { input: tokio::sync::Mutex::new(Some(input)) }
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
    for (i, name) in nodes.iter().enumerate() {
        let disk = SimDisk::default();
        let (tx, rx) = mpsc::channel::<CtlCall>(64);
        ctl.insert(name.to_string(), tx);
        let rx = Rc::new(tokio::sync::Mutex::new(rx));
        let (name, clock) = (name.to_string(), clock.clone());
        sim.host(name.clone(), move || {
            let (disk, name, rx, clock) = (disk.clone(), name.clone(), rx.clone(), clock.clone());
            async move {
                let system = SimSystem::new(disk, node_seed(seed, i), clock);
                let key = load_or_create_device_key(&system)?;
                let transport = SimTransport::bind(nexus_core_proto::NodeId(key.public().0), &name).await?;
                let node = Node::new(transport, system, &name)?;
                let mut rx = rx.lock().await;
                node.run(&mut rx).await?;
                Ok(())
            }
        });
    }

    let trace = Rc::new(RefCell::new(Vec::new()));
    let world = World { inner: Inner::Sim { nodes: ctl, clock }, trace: trace.clone() };
    let fut = scenario(world);
    sim.client("scenario", async move {
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
    _tmp: tempfile::TempDir,
}

impl RealWorld {
    fn start(nodes: &[&str], bin: &str) -> Self {
        let tmp = tempfile::tempdir().expect("无法创建临时目录");
        let mut world = RealWorld { bin: bin.into(), dirs: HashMap::new(), children: RefCell::default(), _tmp: tmp };
        for name in nodes {
            let dir = world._tmp.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let log = std::fs::File::create(dir.join("daemon.log")).unwrap();
            let child = Command::new(&world.bin)
                .args(["--data-dir", dir.to_str().unwrap(), "daemon", "--name", name, "--local-only"])
                // 终端会话里用一个行为确定的 shell，提示符和仿真里一样是 `$ `。
                .env("SHELL", "/bin/sh")
                .env("PS1", "$ ")
                .env_remove("ENV")
                .stdin(Stdio::null())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .expect("无法启动 nexus daemon");
            world.children.borrow_mut().insert(name.to_string(), child);
            world.dirs.insert(name.to_string(), dir);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        for (name, dir) in &world.dirs {
            while !dir.join("ctl.sock").exists() {
                if Instant::now() >= deadline {
                    let log = std::fs::read_to_string(dir.join("daemon.log")).unwrap_or_default();
                    panic!("节点 {name} 在 10 秒内没有就绪。日志：\n{log}");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        world
    }

    fn kill(&self, node: &str) {
        if let Some(mut child) = self.children.borrow_mut().remove(node) {
            let _ = child.kill();
            let _ = child.wait();
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
    rt.block_on(scenario(world));
}

/// 声明一个验收场景，并为它生成 `sim` 和 `real` 两个测试。
///
/// `covers` 列出它验证的需求编号，`tools/check.py` 据此检查需求是否都有测试。
/// `real: yes` 表示也在真实后端上运行；`real: (pending "原因")` 表示真实后端暂时无法运行。
#[macro_export]
macro_rules! scenario {
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
