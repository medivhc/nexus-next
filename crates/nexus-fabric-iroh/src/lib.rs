//! 驱动：基于 iroh 的真实传输。
//!
//! 节点标识就是 iroh 的端点标识（Ed25519 公钥），连接握手保证对端确实持有对应的私钥。
//! 地址提示是 iroh 端点地址（直连地址和中继地址）的编码；在打开了地址发现的情况下，
//! 没有提示也能按标识拨号。
#![forbid(unsafe_code)]

use iroh::endpoint::{presets, Connection as IrohConnection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, RelayMode, RelayUrl, SecretKey};
use nexus_core_proto::{NodeId, PathKind};
use nexus_fabric::{Connection, Proto, Transport};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use tokio::io::Join;

/// 中继的选择。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Relay {
    /// iroh 的公共中继。
    Default,
    /// 不使用中继：只有能直连的节点之间才能通信。
    Disabled,
    /// 自己部署的中继。
    Custom(String),
}

#[derive(Clone, Debug)]
pub struct Config {
    pub relay: Relay,
    /// 是否通过 iroh 的公共地址发现服务发布和查找地址。
    pub discovery: bool,
    /// 绑定的本地地址；`None` 时由 iroh 决定。
    pub bind: Option<SocketAddr>,
    /// 是否使用直连（UDP）。`false` 时不开任何 UDP 套接字，只经中继通信，
    /// 和浏览器里的网页节点相同。
    pub direct: bool,
}

impl Config {
    /// 只在本机回环地址上通信，不连任何外部服务。用于测试。
    pub fn local_only() -> Self {
        Self { relay: Relay::Disabled, discovery: false, bind: Some(([127, 0, 0, 1], 0).into()), direct: true }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self { relay: Relay::Default, discovery: true, bind: None, direct: true }
    }
}

pub struct IrohTransport {
    ep: Endpoint,
    me: NodeId,
    /// 自建中继：它总在本节点的地址提示里，哪怕还没连上它。
    relay: Option<RelayUrl>,
    hints: RefCell<HashMap<NodeId, EndpointAddr>>,
    /// 复用到同一节点的连接，避免每次调用都重新握手。
    conns: RefCell<HashMap<NodeId, IrohConnection>>,
}

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

impl IrohTransport {
    /// `seed` 是设备私钥。
    pub async fn bind(seed: [u8; 32], config: Config) -> io::Result<Self> {
        let mut builder = if config.discovery {
            Endpoint::builder(presets::N0)
        } else {
            Endpoint::builder(presets::Minimal)
        };
        builder = builder
            .secret_key(SecretKey::from_bytes(&seed))
            .alpns(Proto::ALL.iter().map(|p| p.alpn().to_vec()).collect());
        let mut relay = None;
        builder = match &config.relay {
            Relay::Default => builder.relay_mode(RelayMode::Default),
            Relay::Disabled => builder.relay_mode(RelayMode::Disabled),
            Relay::Custom(url) => {
                let url: RelayUrl = url.parse().map_err(|e| io_err(format!("中继地址无效：{e}")))?;
                relay = Some(url.clone());
                builder.relay_mode(RelayMode::custom([url]))
            }
        };
        if !config.direct {
            builder = builder.clear_ip_transports();
        } else if let Some(addr) = config.bind {
            builder = builder.clear_ip_transports().bind_addr(addr).map_err(io_err)?;
        }
        let ep = builder.bind().await.map_err(io_err)?;
        let me = NodeId(*ep.id().as_bytes());
        Ok(Self { ep, me, relay, hints: RefCell::default(), conns: RefCell::default() })
    }

    /// 本端点当前的完整地址。直连地址在没有外部服务时也包含绑定的本地套接字。
    fn current_addr(&self) -> EndpointAddr {
        let mut addr = self.ep.addr();
        if let Some(url) = &self.relay {
            addr = addr.with_relay_url(url.clone());
        }
        for sock in self.ep.bound_sockets() {
            if !sock.ip().is_unspecified() {
                addr = addr.with_ip_addr(sock);
            }
        }
        addr
    }
}

impl Transport for IrohTransport {
    type Conn = IrohConn;

    fn node_id(&self) -> NodeId {
        self.me
    }

    fn local_hint(&self) -> String {
        let bytes = postcard::to_allocvec(&self.current_addr()).expect("地址总能编码");
        nexus_wire::hex_encode(&bytes)
    }

    fn add_hint(&self, node: NodeId, hint: &str) {
        let Some(bytes) = nexus_wire::hex_decode(hint) else { return };
        let Ok(addr) = postcard::from_bytes::<EndpointAddr>(&bytes) else { return };
        if addr.id.as_bytes() == &node.0 && node != self.me {
            self.hints.borrow_mut().insert(node, addr);
        }
    }

    async fn connect(&self, to: NodeId, proto: Proto) -> io::Result<IrohConn> {
        if proto == Proto::Rpc {
            let cached = self.conns.borrow().get(&to).cloned();
            if let Some(conn) = cached {
                if conn.close_reason().is_none() {
                    return Ok(IrohConn { conn, remote: to });
                }
                self.conns.borrow_mut().remove(&to);
            }
        }
        let id = iroh::EndpointId::from_bytes(&to.0).map_err(io_err)?;
        let addr = self.hints.borrow().get(&to).cloned().unwrap_or_else(|| EndpointAddr::new(id));
        let conn = self.ep.connect(addr, proto.alpn()).await.map_err(io_err)?;
        if proto == Proto::Rpc {
            self.conns.borrow_mut().insert(to, conn.clone());
        }
        Ok(IrohConn { conn, remote: to })
    }

    async fn accept(&self) -> io::Result<(IrohConn, Proto)> {
        loop {
            let incoming = self
                .ep
                .accept()
                .await
                .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "端点已关闭"))?;
            // 单个连接握手失败不影响继续接受其他连接。
            let Ok(conn) = incoming.await else { continue };
            let Some(proto) = Proto::from_alpn(conn.alpn()) else { continue };
            let remote = NodeId(*conn.remote_id().as_bytes());
            return Ok((IrohConn { conn, remote }, proto));
        }
    }
}

pub struct IrohConn {
    conn: IrohConnection,
    remote: NodeId,
}

impl Connection for IrohConn {
    type Stream = Join<RecvStream, SendStream>;

    fn remote(&self) -> NodeId {
        self.remote
    }

    fn path_kind(&self) -> Option<PathKind> {
        let paths = self.conn.paths();
        let selected = paths.iter().find(|p| p.is_selected())?;
        Some(if selected.is_relay() { PathKind::Relay } else { PathKind::Direct })
    }

    async fn open_bi(&self) -> io::Result<Self::Stream> {
        let (send, recv) = self.conn.open_bi().await.map_err(io_err)?;
        Ok(tokio::io::join(recv, send))
    }

    async fn accept_bi(&self) -> io::Result<Option<Self::Stream>> {
        match self.conn.accept_bi().await {
            Ok((send, recv)) => Ok(Some(tokio::io::join(recv, send))),
            Err(_) => Ok(None),
        }
    }
}
