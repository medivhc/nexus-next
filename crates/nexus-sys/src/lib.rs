//! 接缝：运行时接触操作系统的唯一入口。
//!
//! 真实实现在 `nexus-sys-real`，仿真实现在 `nexus-sys-sim`。
#![forbid(unsafe_code)]

use nexus_core_proto::Timestamp;
use std::future::Future;
use std::io;

/// 要启动的进程。不经过 shell。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    /// 工作目录；`None` 时由实现决定（真实实现用当前用户的主目录）。
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// 进程已退出，并且标准输出和标准错误都已读完。之后不会再有事件。
    Exited(ExitStatus),
}

/// 一个运行中的进程。
pub trait Proc {
    /// 下一个事件。必须可以安全地被取消（放进 `select!`）。
    fn next_event(&mut self) -> impl Future<Output = ProcEvent>;
    /// 向标准输入写入。不会阻塞：数据排队，由实现异步写入。
    fn write_stdin(&mut self, data: Vec<u8>);
    /// 关闭标准输入。
    fn close_stdin(&mut self);
    /// 终止进程及其派生的进程。之后 `next_event` 很快会给出 `Exited`。
    fn kill(&mut self);
}

pub use nexus_term_proto::Size;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PtyEvent {
    Output(Vec<u8>),
    /// 终端里的程序已退出，它的输出都已读完。之后不会再有事件。
    Exited(ExitStatus),
}

/// 上一次运行留下的终端会话（M7.2）。
pub struct AdoptedPty<P> {
    pub name: String,
    pub size: Size,
    pub created_at: Timestamp,
    pub pty: P,
}

/// 一个运行在伪终端里的程序（用户的 shell）。
pub trait Pty {
    /// 下一个事件。必须可以安全地被取消（放进 `select!`）。
    fn next_event(&mut self) -> impl Future<Output = PtyEvent>;
    /// 写入终端（相当于键盘输入）。不会阻塞：数据排队，由实现异步写入。
    fn write(&mut self, data: Vec<u8>);
    /// 改变终端尺寸，程序会收到通知。
    fn resize(&mut self, size: Size);
    /// 终止程序及其派生的进程。之后 `next_event` 很快会给出 `Exited`。
    fn kill(&mut self);
}

pub trait System {
    type Proc: Proc;
    type Pty: Pty;
    type Tcp: TcpConn;

    /// 当前时间。
    fn now(&self) -> Timestamp;
    /// 填充密码学强度的随机字节（仿真实现由种子决定）。
    fn random(&self, buf: &mut [u8]);
    /// 读取一个小的持久化值。
    fn store_get(&self, key: &str) -> io::Result<Option<Vec<u8>>>;
    /// 原子地写入一个小的持久化值。
    fn store_put(&self, key: &str, value: &[u8]) -> io::Result<()>;
    /// 在一份只追加的日志末尾写一条记录（审计用）。记录本身由调用方编码。
    ///
    /// 写入后会超过 `max_bytes` 时，先把当前内容轮转为旧日志（替换更早的旧日志），
    /// 再写进一份新的。因此日志最多占用约两倍 `max_bytes`，而最近 `max_bytes` 的记录总在。
    fn append_log(&self, name: &str, record: &[u8], max_bytes: u64) -> io::Result<()>;
    /// 读出一份日志的全部内容（旧日志在前）；不存在时为空。
    fn read_log(&self, name: &str) -> io::Result<Vec<u8>>;
    /// 启动一个进程，标准输入、输出、错误都通过管道连接。
    fn spawn(&self, cmd: Cmd) -> io::Result<Self::Proc>;
    /// 在一个新的伪终端里启动当前用户的登录 shell。`name` 是会话名，`created_at` 是会话的创建时间：
    /// 有独立持有进程的实现（M7.2）用它们标识会话，常驻程序重启后由 [`System::adopt_ptys`] 交回。
    fn spawn_pty(&self, name: &str, size: Size, created_at: Timestamp) -> io::Result<Self::Pty>;
    /// 常驻程序启动时：上一次运行留下的、还活着的终端会话（M7.2）。没有独立持有进程的实现总是空的。
    fn adopt_ptys(&self) -> impl Future<Output = Vec<AdoptedPty<Self::Pty>>> {
        std::future::ready(Vec::new())
    }
    /// 连接本机（或本机能直接到达）的一个 TCP 服务：发布的服务就这样接到它真正监听的地址。
    fn tcp_connect(&self, addr: std::net::SocketAddr) -> impl Future<Output = io::Result<Self::Tcp>>;
    /// 按内容寻址的对象存储（Drive）：取出哈希为 `hash` 的对象。
    fn cas_get(&self, hash: &[u8; 32]) -> io::Result<Option<Vec<u8>>>;
    /// 存入一个对象；调用方保证 `hash` 是它的哈希。已有时什么也不做。
    fn cas_put(&self, hash: &[u8; 32], bytes: &[u8]) -> io::Result<()>;
    fn cas_has(&self, hash: &[u8; 32]) -> io::Result<bool>;
    /// 解析主机名（统一入口直连时用）。
    fn resolve(&self, host: &str) -> impl Future<Output = io::Result<Vec<std::net::IpAddr>>>;
}

/// 本机 TCP 连接。
pub trait TcpConn: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + 'static {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + 'static> TcpConn for T {}

impl<S: System + ?Sized> System for std::rc::Rc<S> {
    type Proc = S::Proc;
    type Pty = S::Pty;
    type Tcp = S::Tcp;
    fn tcp_connect(&self, addr: std::net::SocketAddr) -> impl Future<Output = io::Result<S::Tcp>> { (**self).tcp_connect(addr) }
    fn resolve(&self, host: &str) -> impl Future<Output = io::Result<Vec<std::net::IpAddr>>> { (**self).resolve(host) }
    fn now(&self) -> Timestamp { (**self).now() }
    fn cas_get(&self, hash: &[u8; 32]) -> io::Result<Option<Vec<u8>>> { (**self).cas_get(hash) }
    fn cas_put(&self, hash: &[u8; 32], bytes: &[u8]) -> io::Result<()> { (**self).cas_put(hash, bytes) }
    fn cas_has(&self, hash: &[u8; 32]) -> io::Result<bool> { (**self).cas_has(hash) }
    fn random(&self, buf: &mut [u8]) { (**self).random(buf) }
    fn store_get(&self, key: &str) -> io::Result<Option<Vec<u8>>> { (**self).store_get(key) }
    fn store_put(&self, key: &str, value: &[u8]) -> io::Result<()> { (**self).store_put(key, value) }
    fn append_log(&self, name: &str, record: &[u8], max_bytes: u64) -> io::Result<()> { (**self).append_log(name, record, max_bytes) }
    fn read_log(&self, name: &str) -> io::Result<Vec<u8>> { (**self).read_log(name) }
    fn spawn(&self, cmd: Cmd) -> io::Result<Self::Proc> { (**self).spawn(cmd) }
    fn spawn_pty(&self, name: &str, size: Size, created_at: Timestamp) -> io::Result<Self::Pty> { (**self).spawn_pty(name, size, created_at) }
    fn adopt_ptys(&self) -> impl Future<Output = Vec<AdoptedPty<Self::Pty>>> { (**self).adopt_ptys() }
}

/// 尚未实现进程支持时使用的占位类型。
pub enum NoProc {}

impl Proc for NoProc {
    async fn next_event(&mut self) -> ProcEvent { match *self {} }
    fn write_stdin(&mut self, _: Vec<u8>) { match *self {} }
    fn close_stdin(&mut self) { match *self {} }
    fn kill(&mut self) { match *self {} }
}

/// 日志轮转的约定，真实和仿真两种实现都用它测试。
#[doc(hidden)]
pub fn check_log_rotation<S: System>(sys: &S) {
    let read = |name: &str| String::from_utf8(sys.read_log(name).unwrap()).unwrap();
    assert_eq!(read("audit"), "", "不存在的日志读出为空");
    sys.append_log("audit", b"aaaa", 10).unwrap();
    sys.append_log("audit", b"bbbb", 10).unwrap();
    assert_eq!(read("audit"), "aaaabbbb");
    // 再写就超过上限：当前内容成为旧日志。
    sys.append_log("audit", b"cccc", 10).unwrap();
    assert_eq!(read("audit"), "aaaabbbbcccc");
    sys.append_log("audit", b"dddd", 10).unwrap();
    assert_eq!(read("audit"), "aaaabbbbccccdddd");
    // 第二次轮转：更早的旧日志被丢弃。
    sys.append_log("audit", b"eeee", 10).unwrap();
    assert_eq!(read("audit"), "ccccddddeeee");
    // 比上限还大的一条记录照样写下，独占一份日志。
    sys.append_log("audit", b"0123456789xx", 10).unwrap();
    assert_eq!(read("audit"), "eeee0123456789xx");
    // 不同名字的日志互不影响。
    sys.append_log("other", b"z", 10).unwrap();
    assert_eq!(read("other"), "z");
    assert_eq!(read("audit"), "eeee0123456789xx");
}
