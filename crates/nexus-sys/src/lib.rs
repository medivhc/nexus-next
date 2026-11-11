//! 接缝：运行时接触操作系统的唯一入口。
//!
//! 真实实现在 `nexus-sys-real`，仿真实现在 `nexus-sys-sim`。
//! 进程与 PTY 相关的方法在 M1.3、M1.4 加入。
#![forbid(unsafe_code)]

use nexus_core_proto::Timestamp;
use std::io;

pub trait System {
    /// 当前时间。
    fn now(&self) -> Timestamp;
    /// 填充密码学强度的随机字节（仿真实现由种子决定）。
    fn random(&self, buf: &mut [u8]);
    /// 读取一个小的持久化值。
    fn store_get(&self, key: &str) -> io::Result<Option<Vec<u8>>>;
    /// 原子地写入一个小的持久化值。
    fn store_put(&self, key: &str, value: &[u8]) -> io::Result<()>;
}

impl<S: System + ?Sized> System for std::rc::Rc<S> {
    fn now(&self) -> Timestamp { (**self).now() }
    fn random(&self, buf: &mut [u8]) { (**self).random(buf) }
    fn store_get(&self, key: &str) -> io::Result<Option<Vec<u8>>> { (**self).store_get(key) }
    fn store_put(&self, key: &str, value: &[u8]) -> io::Result<()> { (**self).store_put(key, value) }
}
