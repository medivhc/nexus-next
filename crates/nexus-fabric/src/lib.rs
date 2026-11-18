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
    /// 网页节点：登记通行密钥、网页登录。对端是浏览器，不在名册里。
    Web,
}

impl Proto {
    pub const ALL: [Proto; 3] = [Proto::Rpc, Proto::Join, Proto::Web];

    pub fn alpn(self) -> &'static [u8] {
        match self {
            Proto::Rpc => b"nexus/rpc/4",
            Proto::Join => b"nexus/join/4",
            Proto::Web => b"nexus/web/3",
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
    /// 改用 `url` 这个中继（网络的锚点）：之后经它可达，地址提示里也带上它。
    /// 不使用中继的传输（仿真）忽略。
    fn use_relay(&self, url: &str) -> impl Future<Output = io::Result<()>> {
        let _ = url;
        std::future::ready(Ok(()))
    }
    /// 到 `node` 的连接现在的路径（M5.1）。没有连接时为空；不区分路径的传输（仿真）给出一条。
    fn paths(&self, node: NodeId) -> Vec<nexus_wire::PathInfo> {
        let _ = node;
        Vec::new()
    }
    /// 是否知道本机的网络接口（M5.2）。仿真不知道。
    fn has_interfaces(&self) -> bool {
        false
    }
    /// 重新查看本机的网络接口，按 `overrides`（接口名 → 是否计费）和自动识别决定哪些计费，并让传输按此调度。
    fn interfaces(&self, overrides: &std::collections::BTreeMap<String, bool>) -> Vec<nexus_wire::IfaceInfo> {
        let _ = overrides;
        Vec::new()
    }
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
    /// 设置流在这一端的发送优先级，越大越先发（同一条连接上）。不支持的传输忽略。
    fn set_priority(&self, stream: &mut Self::Stream, priority: i32) {
        let _ = (stream, priority);
    }
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

/// 可以安全地放进 `select!` 的帧读取器：被取消时不会丢掉已经读到一半的帧。
pub struct FrameReader<R> {
    inner: R,
    decoder: nexus_wire::FrameDecoder,
    buf: Vec<u8>,
    eof: bool,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner, decoder: nexus_wire::FrameDecoder::default(), buf: vec![0; 64 * 1024], eof: false }
    }

    /// 下一帧。对方正常结束且没有残留数据时返回 `Ok(None)`。
    pub async fn next<T: DeserializeOwned>(&mut self) -> io::Result<Option<T>> {
        loop {
            if let Some(msg) = self.decoder.next_frame().map_err(invalid)? {
                return Ok(Some(msg));
            }
            if self.eof {
                return Ok(None);
            }
            let n = self.inner.read(&mut self.buf).await?;
            if n == 0 {
                self.eof = true;
            } else {
                self.decoder.push(&self.buf[..n]);
            }
        }
    }

    /// 读到对方的结束标记为止，最多等两秒。
    pub async fn drain(&mut self) {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !self.eof {
                match self.inner.read(&mut self.buf).await {
                    Ok(0) | Err(_) => self.eof = true,
                    Ok(_) => {}
                }
            }
        })
        .await;
    }
}
