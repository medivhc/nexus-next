//! 驱动：基于 iroh 的真实传输。
//!
//! 节点标识就是 iroh 的端点标识（Ed25519 公钥），连接握手保证对端确实持有对应的私钥。
//! 地址提示是 iroh 端点地址（直连地址和中继地址）的编码；在打开了地址发现的情况下，
//! 没有提示也能按标识拨号。
#![forbid(unsafe_code)]

use iroh::endpoint::{presets, Connection as IrohConnection, QuicTransportConfig, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, RelayMode, RelayUrl, SecretKey};
use nexus_core_proto::{NodeId, PathKind};
use nexus_fabric::{Connection, Proto, Transport};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use tokio::io::Join;

#[cfg(not(target_arch = "wasm32"))]
mod metered;
#[cfg(not(target_arch = "wasm32"))]
pub mod rendezvous;

/// 每条路径的心跳间隔。
const PATH_KEEP_ALIVE: std::time::Duration = std::time::Duration::from_secs(1);
/// 一条路径这么久没有回应就放弃它。
const PATH_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// 流量调度（M3.5）：按带宽和时延控制发送量（BBR），不把排队推到网络里；
    /// 同一条连接上按流的优先级发送。关掉只用于对比测量。
    pub scheduling: bool,
    /// 碰头服务（短加入码，M6.1）的地址；`None` 时不用。
    pub rendezvous: Option<String>,
}

impl Config {
    /// 只在本机回环地址上通信，不连任何外部服务。用于测试。
    pub fn local_only() -> Self {
        Self { relay: Relay::Disabled, discovery: false, bind: Some(([127, 0, 0, 1], 0).into()), direct: true, scheduling: true, rendezvous: None }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            relay: Relay::Default,
            discovery: true,
            bind: None,
            direct: true,
            scheduling: true,
            #[cfg(not(target_arch = "wasm32"))]
            rendezvous: Some(rendezvous::DEFAULT_URL.to_string()),
            #[cfg(target_arch = "wasm32")]
            rendezvous: None,
        }
    }
}

pub struct IrohTransport {
    ep: Endpoint,
    me: NodeId,
    /// 自建中继（启动时指定的，或网络的锚点）：它总在本节点的地址提示里，哪怕还没连上它。
    relay: RefCell<Option<RelayUrl>>,
    hints: RefCell<HashMap<NodeId, EndpointAddr>>,
    /// 复用到同一节点的连接，避免每次调用都重新握手。对方连过来的连接也放在这里：两台设备之间一条连接，
    /// 两个方向的请求都走它（M7.2：重启后地址变了的设备先连回来，另一边就不用再按新地址去连）。
    conns: RefCell<HashMap<NodeId, IrohConnection>>,
    /// 本机连出去的连接：对方也会在上面发请求，交给 `accept` 一起服务。
    own_tx: tokio::sync::mpsc::UnboundedSender<IrohConnection>,
    own_rx: RefCell<Option<tokio::sync::mpsc::UnboundedReceiver<IrohConnection>>>,
    /// 只经中继（浏览器）：地址提示里的 IP 地址用不上，留着反而让建立连接先在它们上面空等。
    relay_only: bool,
    /// 测试升级后连不上网络时的回滚（M7.3，`NEXUS_TEST_NO_NETWORK`）：端点照常建，但不连任何设备、不接受任何连接。
    offline: bool,
    scheduling: bool,
    /// 按流量计费的本机地址（M5.2），最近一次查看接口时定下的。
    metered_ips: RefCell<std::collections::BTreeSet<std::net::IpAddr>>,
    #[cfg(not(target_arch = "wasm32"))]
    rendezvous: Option<rendezvous::Client>,
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
        // 多路径（M5.1）：每条路径每秒一次心跳，5 秒没有回应就放弃这条路径。
        // 路径断了不靠它发现（丢的包在别的路径上重传），它让空闲的路径保持打开、让死掉的路径及时关掉。
        let mut quic = QuicTransportConfig::builder()
            .default_path_keep_alive_interval(PATH_KEEP_ALIVE)
            .default_path_max_idle_timeout(PATH_IDLE_TIMEOUT);
        if config.scheduling {
            // 默认的拥塞控制（CUBIC）一直加大发送量直到丢包，排队就推到了瓶颈处的缓冲里：
            // 大下载时终端的每个字都要排在几百毫秒的队尾。BBR 按测得的带宽和最小时延发送，队列留在本端，
            // 在本端再按流的优先级发。
            quic = quic.congestion_controller_factory(std::sync::Arc::new(noq_proto::congestion::Bbr3Config::default()));
        }
        builder = builder.transport_config(quic.build());
        let mut relay = None;
        builder = match &config.relay {
            Relay::Default => builder.relay_mode(RelayMode::Default),
            Relay::Disabled => builder.relay_mode(RelayMode::Disabled),
            Relay::Custom(url) => {
                let url: RelayUrl = url.parse().map_err(|e| io_err(format!("中继地址无效：{e}")))?;
                relay = Some(url.clone());
                // 自建中继（锚点）不提供 QUIC 地址探测：不登记它，否则启动时要等探测超时（3 秒）
                // 才选定它为本端的中继，这期间别人经它连不上本端。
                builder.relay_mode(RelayMode::Custom(iroh::RelayMap::from(iroh::RelayConfig::new(url, None))))
            }
        };
        // 浏览器里本来就只有中继。
        #[cfg(not(target_arch = "wasm32"))]
        if !config.direct {
            builder = builder.clear_ip_transports();
        } else if let Some(addr) = config.bind {
            builder = builder.clear_ip_transports().bind_addr(addr).map_err(io_err)?;
        }
        let ep = builder.bind().await.map_err(io_err)?;
        let me = NodeId(*ep.id().as_bytes());
        let (own_tx, own_rx) = tokio::sync::mpsc::unbounded_channel();
        #[cfg(not(target_arch = "wasm32"))]
        let rendezvous = config.rendezvous.as_deref().map(|url| rendezvous::Client::new(url, &ep)).transpose()?;
        Ok(Self {
            ep,
            me,
            relay: RefCell::new(relay),
            hints: RefCell::default(),
            conns: RefCell::default(),
            own_tx,
            own_rx: RefCell::new(Some(own_rx)),
            relay_only: !config.direct,
            offline: std::env::var_os("NEXUS_TEST_NO_NETWORK").is_some(),
            scheduling: config.scheduling,
            metered_ips: RefCell::default(),
            #[cfg(not(target_arch = "wasm32"))]
            rendezvous,
        })
    }

    /// 本端点当前的完整地址。直连地址在没有外部服务时也包含绑定的本地套接字。
    fn current_addr(&self) -> EndpointAddr {
        let mut addr = self.ep.addr();
        if let Some(url) = self.relay.borrow().clone() {
            addr = addr.with_relay_url(url);
        }
        #[cfg(not(target_arch = "wasm32"))]
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
        encode_hint(&self.current_addr())
    }

    fn add_hint(&self, node: NodeId, hint: &str) {
        let Some(mut addr) = decode_hint(node, hint) else { return };
        if self.relay_only {
            addr.addrs.retain(|a| a.is_relay());
        }
        if node != self.me {
            self.hints.borrow_mut().insert(node, addr);
        }
    }

    async fn connect(&self, to: NodeId, proto: Proto) -> io::Result<IrohConn> {
        if self.offline {
            return Err(io::Error::new(io::ErrorKind::NetworkUnreachable, "NEXUS_TEST_NO_NETWORK：不用网络"));
        }
        if proto == Proto::Rpc {
            let cached = self.conns.borrow().get(&to).cloned();
            if let Some(conn) = cached {
                if conn.close_reason().is_none() {
                    return Ok(IrohConn { conn, remote: to, scheduling: self.scheduling });
                }
                self.conns.borrow_mut().remove(&to);
            }
        }
        let id = iroh::EndpointId::from_bytes(&to.0).map_err(io_err)?;
        let addr = self.hints.borrow().get(&to).cloned().unwrap_or_else(|| EndpointAddr::new(id));
        let conn = self.ep.connect(addr, proto.alpn()).await.map_err(io_err)?;
        if proto == Proto::Rpc {
            self.conns.borrow_mut().insert(to, conn.clone());
            let _ = self.own_tx.send(conn.clone());
        }
        Ok(IrohConn { conn, remote: to, scheduling: self.scheduling })
    }

    async fn accept(&self) -> io::Result<(IrohConn, Proto)> {
        // 同一时刻只有一个 accept 在等（运行时让同一个 future 跨循环存活）。
        let mut own = self.own_rx.borrow_mut().take();
        let result = loop {
            let next = async {
                match own.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                // 本机连出去的连接：对方在上面发的请求也要服务。
                Some(conn) = next => {
                    let remote = NodeId(*conn.remote_id().as_bytes());
                    break Ok((IrohConn { conn, remote, scheduling: self.scheduling }, Proto::Rpc));
                }
                incoming = self.ep.accept() => {
                    let Some(incoming) = incoming else {
                        break Err(io::Error::new(io::ErrorKind::BrokenPipe, "端点已关闭"));
                    };
                    if self.offline {
                        incoming.refuse();
                        continue;
                    }
                    // 单个连接握手失败不影响继续接受其他连接。
                    let Ok(conn) = incoming.await else { continue };
                    let Some(proto) = Proto::from_alpn(conn.alpn()) else { continue };
                    let remote = NodeId(*conn.remote_id().as_bytes());
                    if proto == Proto::Rpc {
                        // 对方连过来的：本机到它没有可用的连接时，之后的请求也走这条。
                        let mut conns = self.conns.borrow_mut();
                        if conns.get(&remote).is_none_or(|c| c.close_reason().is_some()) {
                            conns.insert(remote, conn.clone());
                        }
                    }
                    break Ok((IrohConn { conn, remote, scheduling: self.scheduling }, proto));
                }
            }
        };
        *self.own_rx.borrow_mut() = own;
        result
    }

    fn peer_announced(&self, conn: &IrohConn) {
        let old = self.conns.borrow_mut().insert(conn.remote, conn.conn.clone());
        if let Some(old) = old {
            if old.stable_id() != conn.conn.stable_id() {
                old.close(0u32.into(), b"peer restarted");
            }
        }
    }

    fn paths(&self, node: NodeId) -> Vec<nexus_wire::PathInfo> {
        let Some(conn) = self.conns.borrow().get(&node).cloned() else { return Vec::new() };
        if conn.close_reason().is_some() {
            return Vec::new();
        }
        let list = conn.paths();
        list.iter()
            .map(|p| {
                let stats = p.stats();
                let local = match p.local_addr() {
                    iroh::endpoint::LocalTransportAddr::Ip(Some(ip)) => ip.to_string(),
                    iroh::endpoint::LocalTransportAddr::Ip(None) => "?".to_string(),
                    iroh::endpoint::LocalTransportAddr::Relay(_) => "relay".to_string(),
                    _ => "custom".to_string(),
                };
                let remote = match p.remote_addr() {
                    iroh::TransportAddr::Ip(a) => a.to_string(),
                    iroh::TransportAddr::Relay(u) => u.to_string(),
                    other => other.to_string(),
                };
                let metered = local.parse::<std::net::IpAddr>().is_ok_and(|ip| self.metered_ips.borrow().contains(&ip));
                nexus_wire::PathInfo {
                    local,
                    remote,
                    kind: if p.is_relay() { PathKind::Relay } else { PathKind::Direct },
                    rtt_us: p.rtt().as_micros() as u64,
                    tx_bytes: stats.udp_tx.bytes,
                    rx_bytes: stats.udp_rx.bytes,
                    lost_packets: stats.lost_packets,
                    metered,
                }
            })
            .collect()
    }

    fn has_interfaces(&self) -> bool {
        cfg!(not(target_arch = "wasm32")) && !self.relay_only
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn interfaces(&self, overrides: &std::collections::BTreeMap<String, bool>) -> Vec<nexus_wire::IfaceInfo> {
        let list = metered::scan();
        let mut ips = std::collections::BTreeSet::new();
        let out = list
            .into_iter()
            .map(|i| {
                let setting = overrides.get(&i.name).copied();
                let metered = setting.unwrap_or(i.auto_metered);
                if metered {
                    ips.extend(i.addrs.iter().copied());
                }
                nexus_wire::IfaceInfo {
                    addrs: i.addrs.iter().map(|a| a.to_string()).collect(),
                    name: i.name,
                    metered,
                    auto_metered: i.auto_metered,
                    setting,
                }
            })
            .collect();
        iroh::nexus_set_metered_ips(ips.iter().copied());
        *self.metered_ips.borrow_mut() = ips;
        out
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn rendezvous_put(&self, seed: [u8; 32], data: Vec<u8>) -> io::Result<()> {
        match &self.rendezvous {
            Some(c) => c.put(seed, data).await,
            None => Err(io::Error::new(io::ErrorKind::Unsupported, "启动时关闭了地址发现，也没有用 --rendezvous 指定")),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn rendezvous_get(&self, seed: [u8; 32]) -> io::Result<Option<Vec<u8>>> {
        match &self.rendezvous {
            Some(c) => c.get(seed).await,
            None => Err(io::Error::new(io::ErrorKind::Unsupported, "启动时关闭了地址发现，也没有用 --rendezvous 指定")),
        }
    }

    async fn use_relay(&self, url: &str) -> io::Result<()> {
        let url: RelayUrl = url.parse().map_err(|e| io_err(format!("中继地址无效：{e}")))?;
        if self.relay.borrow().as_ref() == Some(&url) {
            return Ok(());
        }
        // 锚点只提供中继，不提供 QUIC 地址发现。
        self.ep.insert_relay(url.clone(), std::sync::Arc::new(iroh::RelayConfig::new(url.clone(), None))).await;
        *self.relay.borrow_mut() = Some(url);
        Ok(())
    }
}

/// 以 `seed` 为私钥的节点的标识（与 [`IrohTransport::bind`] 得到的一致），不需要先建立端点。
pub fn node_id_from_seed(seed: &[u8; 32]) -> NodeId {
    NodeId(*SecretKey::from_bytes(seed).public().as_bytes())
}

/// 只经中继 `relay` 联系 `node` 的地址提示（浏览器从通行密钥里只知道入口设备和它的中继）。
pub fn relay_hint(node: NodeId, relay: &str) -> Result<String, String> {
    let id = iroh::EndpointId::from_bytes(&node.0).map_err(|e| format!("设备标识无效：{e}"))?;
    let url: RelayUrl = relay.parse().map_err(|e| format!("中继地址无效：{e}"))?;
    Ok(encode_hint(&EndpointAddr::new(id).with_relay_url(url)))
}

/// 地址提示里的中继地址（有多个时取第一个）。
pub fn hint_relay(hint: &str) -> Option<String> {
    let bytes = nexus_wire::hex_decode(hint)?;
    let addrs = decode_addrs(&bytes).or_else(|| postcard::from_bytes::<EndpointAddr>(&bytes).ok().map(|a| a.addrs))?;
    let first = addrs.iter().find_map(|a| match a {
        iroh::TransportAddr::Relay(u) => Some(u.to_string()),
        _ => None,
    });
    first
}

/// 地址提示的第一个字节：之后只有地址，不带设备标识（M6.1，标识在提示之外总是已知的）。
const HINT_V2: u8 = 0xa2;

fn encode_hint(addr: &EndpointAddr) -> String {
    let mut bytes = vec![HINT_V2];
    bytes.extend(postcard::to_allocvec(&addr.addrs).expect("地址总能编码"));
    nexus_wire::hex_encode(&bytes)
}

fn decode_addrs(bytes: &[u8]) -> Option<std::collections::BTreeSet<iroh::TransportAddr>> {
    let rest = bytes.strip_prefix(&[HINT_V2])?;
    match postcard::take_from_bytes(rest) {
        Ok((addrs, [])) => Some(addrs),
        _ => None,
    }
}

fn decode_hint(node: NodeId, hint: &str) -> Option<EndpointAddr> {
    let id = iroh::EndpointId::from_bytes(&node.0).ok()?;
    let bytes = nexus_wire::hex_decode(hint)?;
    if let Some(addrs) = decode_addrs(&bytes) {
        let mut addr = EndpointAddr::new(id);
        addr.addrs = addrs;
        return Some(addr);
    }
    // 旧的提示（数据目录里存着的）：完整的端点地址，标识必须对得上。
    postcard::from_bytes::<EndpointAddr>(&bytes).ok().filter(|a| a.id == id)
}

pub struct IrohConn {
    conn: IrohConnection,
    remote: NodeId,
    scheduling: bool,
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

    fn set_priority(&self, stream: &mut Self::Stream, priority: i32) {
        if self.scheduling {
            let _ = stream.writer().set_priority(priority);
        }
    }

    async fn accept_bi(&self) -> io::Result<Option<Self::Stream>> {
        match self.conn.accept_bi().await {
            Ok((send, recv)) => Ok(Some(tokio::io::join(recv, send))),
            Err(_) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_hints_round_trip() {
        let node = NodeId(*SecretKey::from_bytes(&[3; 32]).public().as_bytes());
        let hint = relay_hint(node, "https://bewith.link").unwrap();
        assert_eq!(hint_relay(&hint).as_deref(), Some("https://bewith.link/"));
        assert!(relay_hint(node, "not a url").is_err());
        assert_eq!(hint_relay("zz"), None);
        assert_eq!(node_id_from_seed(&[3; 32]), node);
    }

    /// 地址提示不带设备标识（M6.1）；数据目录里存着的旧提示照样能用；标识对不上的旧提示不用。
    #[test]
    fn hints_omit_the_node_id() {
        let node = NodeId(*SecretKey::from_bytes(&[3; 32]).public().as_bytes());
        let id = iroh::EndpointId::from_bytes(&node.0).unwrap();
        let addr = EndpointAddr::new(id)
            .with_relay_url("https://bewith.link".parse().unwrap())
            .with_ip_addr("192.0.2.7:4433".parse().unwrap());
        let hint = encode_hint(&addr);
        let legacy = nexus_wire::hex_encode(&postcard::to_allocvec(&addr).unwrap());
        assert!(hint.len() + 2 * 31 <= legacy.len(), "{} 对 {}", hint.len(), legacy.len());
        assert_eq!(decode_hint(node, &hint), Some(addr.clone()));
        assert_eq!(decode_hint(node, &legacy), Some(addr));
        let other = NodeId(*SecretKey::from_bytes(&[4; 32]).public().as_bytes());
        assert_eq!(decode_hint(other, &legacy), None);
        assert_eq!(hint_relay(&legacy).as_deref(), Some("https://bewith.link/"));
    }
}
