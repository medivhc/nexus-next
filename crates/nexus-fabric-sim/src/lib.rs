//! 驱动：基于 turmoil 虚拟网络的仿真传输。
//!
//! 每条双向流是一条独立的虚拟 TCP 连接，开头由发起方写入自己的节点标识和协议。
//! 仿真里信任这个声明；真实传输里对端身份由握手的公钥保证。
//!
//! 地址提示就是虚拟主机名。和真实传输一样，节点只能拨通它拿到过提示的节点。
#![forbid(unsafe_code)]

use nexus_core_proto::{NodeId, PathKind};
use nexus_fabric::{Connection, Proto, Transport};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use turmoil::net::{TcpListener, TcpStream};

const PORT: u16 = 7000;

pub struct SimTransport {
    me: NodeId,
    host: String,
    hints: RefCell<HashMap<NodeId, String>>,
    /// 只发起连接的一方（浏览器）没有监听。
    listener: Option<TcpListener>,
}

impl SimTransport {
    /// 必须在 turmoil 的主机上下文里调用，`host` 是本主机的名字。
    pub async fn bind(me: NodeId, host: &str) -> io::Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", PORT)).await?;
        Ok(Self { me, host: host.to_string(), hints: RefCell::default(), listener: Some(listener) })
    }

    /// 只发起连接、不接受连接的一方，例如浏览器。同一台仿真主机上可以有任意多个。
    pub fn client(me: NodeId) -> Self {
        Self { me, host: String::new(), hints: RefCell::default(), listener: None }
    }
}

pub enum SimConn {
    Outgoing { me: NodeId, remote: NodeId, host: String, proto: Proto },
    Incoming { remote: NodeId, stream: RefCell<Option<TcpStream>> },
}

fn proto_byte(p: Proto) -> u8 {
    match p {
        Proto::Rpc => 1,
        Proto::Join => 2,
        Proto::Web => 3,
    }
}

impl Transport for SimTransport {
    type Conn = SimConn;

    fn node_id(&self) -> NodeId {
        self.me
    }

    fn local_hint(&self) -> String {
        self.host.clone()
    }

    fn add_hint(&self, node: NodeId, hint: &str) {
        if node != self.me && !hint.is_empty() {
            self.hints.borrow_mut().insert(node, hint.to_string());
        }
    }

    async fn connect(&self, to: NodeId, proto: Proto) -> io::Result<SimConn> {
        let host = self.hints.borrow().get(&to).cloned().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("不知道节点 {} 的地址", to.short()))
        })?;
        Ok(SimConn::Outgoing { me: self.me, remote: to, host, proto })
    }

    async fn accept(&self) -> io::Result<(SimConn, Proto)> {
        let Some(listener) = &self.listener else { return std::future::pending().await };
        loop {
            let (mut stream, _) = listener.accept().await?;
            let mut head = [0u8; 33];
            if stream.read_exact(&mut head).await.is_err() {
                continue;
            }
            let proto = match head[32] {
                1 => Proto::Rpc,
                2 => Proto::Join,
                3 => Proto::Web,
                _ => continue,
            };
            let mut id = [0u8; 32];
            id.copy_from_slice(&head[..32]);
            let conn = SimConn::Incoming { remote: NodeId(id), stream: RefCell::new(Some(stream)) };
            return Ok((conn, proto));
        }
    }
}

impl Connection for SimConn {
    type Stream = TcpStream;

    fn remote(&self) -> NodeId {
        match self {
            SimConn::Outgoing { remote, .. } | SimConn::Incoming { remote, .. } => *remote,
        }
    }

    fn path_kind(&self) -> Option<PathKind> {
        Some(PathKind::Direct)
    }

    async fn open_bi(&self) -> io::Result<TcpStream> {
        match self {
            SimConn::Outgoing { me, host, proto, .. } => {
                let mut s = TcpStream::connect((turmoil::lookup(host.as_str()), PORT)).await?;
                let mut head = [0u8; 33];
                head[..32].copy_from_slice(&me.0);
                head[32] = proto_byte(*proto);
                s.write_all(&head).await?;
                Ok(s)
            }
            SimConn::Incoming { .. } => {
                Err(io::Error::new(io::ErrorKind::Unsupported, "仿真传输只支持由发起方开流"))
            }
        }
    }

    async fn accept_bi(&self) -> io::Result<Option<TcpStream>> {
        match self {
            SimConn::Incoming { stream, .. } => Ok(stream.borrow_mut().take()),
            SimConn::Outgoing { .. } => Ok(None),
        }
    }
}
