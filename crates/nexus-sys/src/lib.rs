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

pub trait System {
    type Proc: Proc;

    /// 当前时间。
    fn now(&self) -> Timestamp;
    /// 填充密码学强度的随机字节（仿真实现由种子决定）。
    fn random(&self, buf: &mut [u8]);
    /// 读取一个小的持久化值。
    fn store_get(&self, key: &str) -> io::Result<Option<Vec<u8>>>;
    /// 原子地写入一个小的持久化值。
    fn store_put(&self, key: &str, value: &[u8]) -> io::Result<()>;
    /// 启动一个进程，标准输入、输出、错误都通过管道连接。
    fn spawn(&self, cmd: Cmd) -> io::Result<Self::Proc>;
}

impl<S: System + ?Sized> System for std::rc::Rc<S> {
    type Proc = S::Proc;
    fn now(&self) -> Timestamp { (**self).now() }
    fn random(&self, buf: &mut [u8]) { (**self).random(buf) }
    fn store_get(&self, key: &str) -> io::Result<Option<Vec<u8>>> { (**self).store_get(key) }
    fn store_put(&self, key: &str, value: &[u8]) -> io::Result<()> { (**self).store_put(key, value) }
    fn spawn(&self, cmd: Cmd) -> io::Result<Self::Proc> { (**self).spawn(cmd) }
}

/// 尚未实现进程支持时使用的占位类型。
pub enum NoProc {}

impl Proc for NoProc {
    async fn next_event(&mut self) -> ProcEvent { match *self {} }
    fn write_stdin(&mut self, _: Vec<u8>) { match *self {} }
    fn close_stdin(&mut self) { match *self {} }
    fn kill(&mut self) { match *self {} }
}
