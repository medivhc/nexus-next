//! 测试工具箱：同一份验收场景，跑在两种后端上。
//!
//! - 仿真后端：所有节点在同一个线程里，网络和时间都是模拟的，由种子决定，可精确复现。
//! - 真实后端：每个节点是一个真正的 `nexus daemon` 进程，命令行是真正的 `nexus` 二进制。
//!
//! 场景只通过命令行这一层操作节点，和用户、agent 看到的完全一样。
#![forbid(unsafe_code)]

pub use nexus_cli::Output;

use nexus_cli::{parse, run_client, Ctl};
use nexus_core_proto::PeerBook;
use nexus_fabric_sim::{SimDirectory, SimTransport};
use nexus_runtime::{load_or_create_node_id, save_peers, CtlCall, Node};
use nexus_sys_sim::{SimClock, SimDisk, SimSystem};
use nexus_wire::{CtlRequest, CtlResponse};
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

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
        let out = match &self.inner {
            Inner::Sim { nodes, .. } => {
                let tx = nodes.get(node).unwrap_or_else(|| panic!("场景里没有节点 {node}"));
                match parse(args.iter().copied()) {
                    Err(out) => out,
                    Ok(cli) => run_client(&cli.cmd, &ChanCtl(tx.clone())).await,
                }
            }
            Inner::Real(real) => real.cli(node, args),
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
}

struct ChanCtl(mpsc::Sender<CtlCall>);

impl Ctl for ChanCtl {
    async fn call(&self, req: CtlRequest) -> io::Result<CtlResponse> {
        let gone = || io::Error::new(io::ErrorKind::ConnectionRefused, "常驻程序没有在运行");
        let (tx, rx) = oneshot::channel();
        self.0.send((req, tx)).await.map_err(|_| gone())?;
        rx.await.map_err(|_| gone())
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

    // 先确定每个节点的标识，并让它们互相知道。M1.2 起这一步由加入流程代替。
    let clock = SimClock::default();
    let directory = SimDirectory::default();
    let mut disks = Vec::new();
    let mut book = PeerBook::default();
    for (i, name) in nodes.iter().enumerate() {
        let disk = SimDisk::default();
        let id = load_or_create_node_id(&SimSystem::new(disk.clone(), node_seed(seed, i), clock.clone()))
            .expect("内存存储不会失败");
        directory.register(id, name);
        book.peers.push((name.to_string(), id));
        disks.push(disk);
    }

    let mut ctl = HashMap::new();
    for (i, name) in nodes.iter().enumerate() {
        save_peers(&SimSystem::new(disks[i].clone(), 0, clock.clone()), &book).expect("内存存储不会失败");
        let (tx, rx) = mpsc::channel::<CtlCall>(64);
        ctl.insert(name.to_string(), tx);
        let rx = Rc::new(tokio::sync::Mutex::new(rx));
        let (disk, directory, name, clock) = (disks[i].clone(), directory.clone(), name.to_string(), clock.clone());
        sim.host(name.clone(), move || {
            let (disk, directory, name, rx, clock) = (disk.clone(), directory.clone(), name.clone(), rx.clone(), clock.clone());
            async move {
                let system = SimSystem::new(disk, node_seed(seed, i) ^ 0x5eed, clock);
                let id = load_or_create_node_id(&system)?;
                let transport = SimTransport::bind(id, directory).await?;
                let node = Node::new(transport, system, &name);
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
    children: Vec<Child>,
    _tmp: tempfile::TempDir,
}

impl RealWorld {
    fn start(nodes: &[&str], bin: &str) -> Self {
        let tmp = tempfile::tempdir().expect("无法创建临时目录");
        let mut world = RealWorld { bin: bin.into(), dirs: HashMap::new(), children: Vec::new(), _tmp: tmp };
        for name in nodes {
            let dir = world._tmp.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let log = std::fs::File::create(dir.join("daemon.log")).unwrap();
            let child = Command::new(&world.bin)
                .args(["--data-dir", dir.to_str().unwrap(), "daemon", "--name", name])
                .stdin(Stdio::null())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .expect("无法启动 nexus daemon");
            world.children.push(child);
            world.dirs.insert(name.to_string(), dir);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        for (name, dir) in &world.dirs {
            while !dir.join("ctl.sock").exists() {
                assert!(Instant::now() < deadline, "节点 {name} 在 5 秒内没有就绪");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        world
    }

    fn cli(&self, node: &str, args: &[&str]) -> Output {
        let dir = self.dirs.get(node).unwrap_or_else(|| panic!("场景里没有节点 {node}"));
        let out = Command::new(&self.bin)
            .args(["--data-dir", dir.to_str().unwrap()])
            .args(args)
            .stdin(Stdio::null()) // 没有终端：任何交互提示都会让命令失败
            .output()
            .expect("无法执行 nexus");
        Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

impl Drop for RealWorld {
    fn drop(&mut self) {
        for child in &mut self.children {
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
