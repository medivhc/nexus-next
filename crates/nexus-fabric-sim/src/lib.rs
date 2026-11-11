//! 驱动：基于 turmoil 虚拟网络的仿真传输。
//!
//! 简化：每条双向流是一条独立的虚拟 TCP 连接，开头由发起方写入自己的节点标识。
//! 仿真里信任这个声明；真实传输里对端身份由握手的公钥保证。
#![forbid(unsafe_code)]

use nexus_core_proto::{NodeId, PathKind};
use nexus_fabric::{Connection, Transport};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::rc::Rc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use turmoil::net::{TcpListener, TcpStream};

const PORT: u16 = 7000;

/// 节点标识到虚拟主机名的对应表，代替真实世界里的地址发现。
#[derive(Clone, Default)]
pub struct SimDirectory(Rc<RefCell<HashMap<NodeId, String>>>);

impl SimDirectory {
    pub fn register(&self, id: NodeId, host: &str) {
        self.0.borrow_mut().insert(id, host.to_string());
    }
    fn host_of(&self, id: NodeId) -> Option<String> {
        self.0.borrow().get(&id).cloned()
    }
}

pub struct SimTransport {
    me: NodeId,
    dir: SimDirectory,
    listener: TcpListener,
}

impl SimTransport {
    /// 必须在 turmoil 的主机上下文里调用。
    pub async fn bind(me: NodeId, dir: SimDirectory) -> io::Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", PORT)).await?;
        Ok(Self { me, dir, listener })
    }
}

pub enum SimConn {
    Outgoing { me: NodeId, remote: NodeId, host: String },
    Incoming { remote: NodeId, stream: RefCell<Option<TcpStream>> },
}

impl Transport for SimTransport {
    type Conn = SimConn;

    fn node_id(&self) -> NodeId { self.me }

    async fn connect(&self, to: NodeId) -> io::Result<SimConn> {
        let host = self
            .dir
            .host_of(to)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "找不到该节点的地址"))?;
        Ok(SimConn::Outgoing { me: self.me, remote: to, host })
    }

    async fn accept(&self) -> io::Result<SimConn> {
        let (mut stream, _) = self.listener.accept().await?;
        let mut id = [0u8; 32];
        stream.read_exact(&mut id).await?;
        Ok(SimConn::Incoming { remote: NodeId(id), stream: RefCell::new(Some(stream)) })
    }

    fn path_kind(&self, to: NodeId) -> Option<PathKind> {
        self.dir.host_of(to).map(|_| PathKind::Direct)
    }
}

impl Connection for SimConn {
    type Stream = TcpStream;

    fn remote(&self) -> NodeId {
        match self {
            SimConn::Outgoing { remote, .. } | SimConn::Incoming { remote, .. } => *remote,
        }
    }

    async fn open_bi(&self) -> io::Result<TcpStream> {
        match self {
            SimConn::Outgoing { me, host, .. } => {
                let mut s = TcpStream::connect((turmoil::lookup(host.as_str()), PORT)).await?;
                s.write_all(&me.0).await?;
                Ok(s)
            }
            SimConn::Incoming { .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "仿真传输只支持由发起方开流",
            )),
        }
    }

    async fn accept_bi(&self) -> io::Result<Option<TcpStream>> {
        match self {
            SimConn::Incoming { stream, .. } => Ok(stream.borrow_mut().take()),
            SimConn::Outgoing { .. } => Ok(None),
        }
    }
}
