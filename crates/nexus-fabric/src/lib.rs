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

/// 连接上承载的协议。真实传输里对应不同的 ALPN。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Proto {
    /// 名册内设备之间的调用。
    Rpc,
    /// 加入流程：管理设备向等待加入的设备发出邀请。
    Join,
}

impl Proto {
    pub const ALL: [Proto; 2] = [Proto::Rpc, Proto::Join];

    pub fn alpn(self) -> &'static [u8] {
        match self {
            Proto::Rpc => b"nexus/rpc/2",
            Proto::Join => b"nexus/join/2",
        }
    }

    pub fn from_alpn(alpn: &[u8]) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.alpn() == alpn)
    }
}

pub trait Transport {
    type Conn: Connection;

    fn node_id(&self) -> NodeId;
    /// 本节点当前的地址提示，别的节点拿到后可以用来拨号。内容只有同类传输能解释。
    fn local_hint(&self) -> String;
    /// 记住某个节点的地址提示。无法解释的提示被忽略。
    fn add_hint(&self, node: NodeId, hint: &str);
    /// 按节点标识拨号。
    fn connect(&self, to: NodeId, proto: Proto) -> impl Future<Output = io::Result<Self::Conn>>;
    /// 等待下一个进入的连接。
    fn accept(&self) -> impl Future<Output = io::Result<(Self::Conn, Proto)>>;
}

pub trait Connection {
    type Stream: AsyncRead + AsyncWrite + Unpin;

    /// 对端的节点标识。实现必须保证它已经过传输层验证。
    fn remote(&self) -> NodeId;
    /// 这条连接当前使用的路径类型，未知时为 `None`。
    fn path_kind(&self) -> Option<PathKind>;
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
