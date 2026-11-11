//! 接缝：运行时接触网络的唯一入口。
//!
//! 对上层只暴露三样东西：互不阻塞的双向流、（以后的）数据报、已验证的对端身份。
//! 真实实现在 M1.2 的 `nexus-fabric-iroh`，仿真实现在 `nexus-fabric-sim`。
#![forbid(unsafe_code)]

use nexus_core_proto::{NodeId, PathKind};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::future::Future;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub trait Transport {
    type Conn: Connection;

    fn node_id(&self) -> NodeId;
    /// 按节点标识拨号。
    fn connect(&self, to: NodeId) -> impl Future<Output = io::Result<Self::Conn>>;
    /// 等待下一个进入的连接。
    fn accept(&self) -> impl Future<Output = io::Result<Self::Conn>>;
    /// 到某个节点当前使用的路径类型，未知时为 `None`。
    fn path_kind(&self, to: NodeId) -> Option<PathKind>;
}

pub trait Connection {
    type Stream: AsyncRead + AsyncWrite + Unpin;

    /// 对端的节点标识。实现必须保证它已经过传输层验证。
    fn remote(&self) -> NodeId;
    fn open_bi(&self) -> impl Future<Output = io::Result<Self::Stream>>;
    /// 对端关闭连接后返回 `Ok(None)`。
    fn accept_bi(&self) -> impl Future<Output = io::Result<Option<Self::Stream>>>;
}

/// 在流上写一帧。
pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let frame = nexus_wire::encode(msg).map_err(invalid)?;
    w.write_all(&frame).await?;
    w.flush().await
}

/// 从流上读一帧。
pub async fn read_msg<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let mut header = [0u8; 4];
    r.read_exact(&mut header).await?;
    let n = nexus_wire::body_len(header).map_err(invalid)?;
    let mut body = vec![0u8; n];
    r.read_exact(&mut body).await?;
    nexus_wire::decode_body(&body).map_err(invalid)
}

fn invalid(e: nexus_wire::FrameError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

/// 不联网的传输：拨号总是失败，也不会有连接进来。
/// M0 的真实常驻程序使用它，M1.2 换成 iroh 适配器。
pub struct OfflineTransport {
    pub node_id: NodeId,
}

pub enum NoConn {}

impl Connection for NoConn {
    type Stream = tokio::io::DuplexStream;
    fn remote(&self) -> NodeId { match *self {} }
    async fn open_bi(&self) -> io::Result<Self::Stream> { match *self {} }
    async fn accept_bi(&self) -> io::Result<Option<Self::Stream>> { match *self {} }
}

impl Transport for OfflineTransport {
    type Conn = NoConn;
    fn node_id(&self) -> NodeId { self.node_id }
    async fn connect(&self, _to: NodeId) -> io::Result<NoConn> {
        Err(io::Error::new(io::ErrorKind::NotConnected, "本构建尚未包含网络传输"))
    }
    async fn accept(&self) -> io::Result<NoConn> {
        std::future::pending().await
    }
    fn path_kind(&self, _to: NodeId) -> Option<PathKind> { None }
}
