//! 运行时：把纯逻辑接到 `Transport` 和 `System` 两个接缝上。
//!
//! 本 crate 不直接引用任何具体的传输或系统实现，也不自行创建任务：
//! 所有并发都在 `Node::run` 这一个 future 里完成，因此同一份代码既能跑在
//! 真实的异步运行时上，也能跑在单线程的仿真里。
#![forbid(unsafe_code)]

use futures_util::future::join_all;
use futures_util::stream::{FuturesUnordered, StreamExt};
use nexus_core_proto::joincode::{self, ShortCode};
use nexus_core_proto::authz::{
    self, Action, AgentCert, Assertion, AuditEvent, Context, DenyReason, Grant, Outcome, Principal, ReplayGuard, WebSession,
};
use nexus_core_proto::path::{CapabilityPattern, ResourcePattern};
use nexus_core_proto::roster::{RevokedAgent, WebKey};
use nexus_core_proto::webauthn::{login_challenge, verify_assertion, P256Key, WebAuthnAssertion};
use nexus_core_proto::path::{valid_segment, Capability, ResourcePath};
use nexus_core_proto::roster::{accept_update, Device, DeviceRole, Roster, SignedRoster};
use nexus_core_proto::{NodeId, NodeInfo, PublicKey, SecretKey, Timestamp};
use nexus_fabric::{read_msg, read_response, write_msg, write_response, Connection, FrameReader, Proto, Transport};
use nexus_net_core::http::{self, BadRequest, ProxyRequest};
use nexus_net_core::route::{self, Decision, Outbound, Rule};
use nexus_net_core::fakeip::FakePool;
use nexus_net_core::{dns, sniff, socks5, Entry, Host, RouteTable, Target, SERVICE_SUFFIX};
use nexus_sys::{Cmd, Proc, ProcEvent, Pty, PtyEvent, System};
use nexus_term_proto::{Action as TermAction, ClientId, Event as TermEvent, Session as TermSession, DEFAULT_WINDOW};
use nexus_wire::{
    hex_encode, AddrHints, AgentCredential, AnchorReport, NewCredential, WebDevice, WebEnrollReport, WebKeyInfo, AgentInfo, AgentIssueReport, AuditRecord, ClientMsg, CtlRequest, CtlResponse, ErrorCode, ExecEvent, ExecResult, ExecSpec, InitReport,
    Input, JoinCode, SessionFrame, ListEntry, MembershipReport, Request, RequestHeader, Response, ServiceInfo, SessionInfo, Size, StatusReport,
    RouteExplanation, WireError, WIRE_VERSION,
};
mod drive;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// 常驻程序一侧的控制会话：向命令行发送回应，接收命令行的输入。
pub struct CtlServer {
    pub tx: mpsc::Sender<CtlResponse>,
    pub rx: mpsc::Receiver<Input>,
}

/// 命令行一侧的控制会话。
pub struct CtlClient {
    pub tx: mpsc::Sender<Input>,
    pub rx: mpsc::Receiver<CtlResponse>,
}

/// 一次本机控制调用：请求、以谁的身份（`None` 为本机设备），以及这次会话常驻程序一侧的通道。
pub type CtlCall = (CtlRequest, Option<AgentCredential>, CtlServer);

/// 建立一对控制会话通道。通道有界：命令行读得慢时，输出会一路反压到远端进程。
pub fn ctl_session(req: CtlRequest, agent: Option<AgentCredential>) -> (CtlCall, CtlClient) {
    let (resp_tx, resp_rx) = mpsc::channel(16);
    let (in_tx, in_rx) = mpsc::channel(16);
    ((req, agent, CtlServer { tx: resp_tx, rx: in_rx }), CtlClient { tx: in_tx, rx: resp_rx })
}

/// 收到请求后怎么处理：直接回应，或者进入流式的执行或终端会话。
enum Dispatch {
    Reply(Response),
    Exec(ExecSpec),
    Session { name: String, create: bool, size: Size, guard: Option<Box<SessionGuard>> },
    Pipe { addr: std::net::SocketAddr, name: String },
    Exit { target: Target },
    DriveGet { hashes: Vec<[u8; 32]> },
    DrivePush { ws: String, head: [u8; 32], count: u32 },
    WebEnroll { token: [u8; 16], credential: NewCredential },
}

/// 终端连接的主体：名册变化时重新判定，凭据到期时断开。
struct SessionGuard {
    peer: NodeId,
    principal: Principal,
    action: Action,
    not_after: Option<Timestamp>,
}

impl SessionGuard {
    fn new(peer: NodeId, principal: Principal, action: Action) -> Self {
        let not_after = match &principal {
            Principal::Device => None,
            Principal::Agent { cert, .. } => Some(cert.cert.not_after),
            Principal::Web { session } => Some(session.session.not_after),
        };
        Self { peer, principal, action, not_after }
    }

    fn allowed(&self, roster: &Roster, me: NodeId, now: Timestamp) -> bool {
        if roster.device(me).is_none() {
            return false;
        }
        if let Principal::Agent { cert, .. } = &self.principal {
            // 初始请求的签名、nonce 和时间已验证；长期连接复查证书和当前授权，
            // 不把原断言的五分钟签署容差误当成会话有效期。
            roster.device(self.peer).is_some()
                && cert.validate(roster, now).is_ok()
                && cert.cert.grants.iter().any(|g| g.allows(&self.action))
        } else {
            authz::decide(&Context { roster, me, now }, self.peer, &self.principal, &self.action).allowed()
        }
    }
}

/// 一次等待浏览器完成的通行密钥登记。
struct PendingWebEnroll {
    label: String,
    grants: Vec<Grant>,
    rp_id: String,
    expires_at: Timestamp,
}

/// 网页登录默认的权限：所有设备的终端会话和状态查看。
const DEFAULT_WEB_GRANTS: [&str; 2] = ["term.session.*@**", "node.info@**"];
/// 名册里没有锚点、也没有指定页面时，登记链接指向的页面。
pub const DEFAULT_WEB_PAGE: &str = "https://bewith.link";

/// 发给一个终端会话任务的命令。
enum SessCmd {
    Attach { client: ClientId, size: Size, tx: mpsc::UnboundedSender<ClientMsg> },
    Event(TermEvent),
    Kill,
}

/// 节点上一个终端会话的句柄。会话本身由一个独立的任务驱动。
struct SessionHandle {
    cmd: mpsc::UnboundedSender<SessCmd>,
    created_at: Timestamp,
    clients: Rc<Cell<u32>>,
    size: Rc<Cell<Size>>,
}

/// 请 `run` 启动的一个新会话任务。
struct NewSession<P> {
    name: String,
    pty: P,
    size: Size,
    cmds: mpsc::UnboundedReceiver<SessCmd>,
    clients: Rc<Cell<u32>>,
    current_size: Rc<Cell<Size>>,
}

/// 一个运行中的会话：状态机，以及连着它的客户端。
struct LiveSession {
    term: TermSession,
    clients: BTreeMap<ClientId, mpsc::UnboundedSender<ClientMsg>>,
    client_count: Rc<Cell<u32>>,
    current_size: Rc<Cell<Size>>,
}

impl LiveSession {
    /// 把事件交给状态机，执行它给出的动作。发不出去的客户端视为已经离开。
    fn apply<P: Pty>(&mut self, pty: &mut P, event: TermEvent) {
        let mut queue = vec![event];
        while let Some(event) = queue.pop() {
            for action in self.term.handle(event) {
                match action {
                    TermAction::Send { client, msg } => {
                        let gone = self.clients.get(&client).is_none_or(|tx| tx.send(msg).is_err());
                        if gone && self.clients.remove(&client).is_some() {
                            queue.push(TermEvent::Detach { client });
                        }
                    }
                    TermAction::WritePty(bytes) => pty.write(bytes),
                    TermAction::ResizePty(size) => pty.resize(size),
                }
            }
        }
        self.client_count.set(self.term.client_count() as u32);
        self.current_size.set(self.term.size());
    }

    fn command<P: Pty>(&mut self, pty: &mut P, cmd: SessCmd) {
        match cmd {
            SessCmd::Attach { client, size, tx } => {
                self.clients.insert(client, tx);
                self.apply(pty, TermEvent::Attach { client, size });
            }
            SessCmd::Event(event) => {
                if let TermEvent::Detach { client } = &event {
                    self.clients.remove(client);
                }
                self.apply(pty, event);
            }
            SessCmd::Kill => pty.kill(),
        }
    }
}

/// 终端会话流上两端发送心跳的间隔。
const HEARTBEAT: Duration = Duration::from_secs(5);
/// 这么久收不到对方的任何消息，就认为连接已经失效。写入卡住这么久也一样。
const PEER_SILENT: Duration = Duration::from_secs(15);

/// 会话里的程序结束后，等落后的客户端追上最终屏幕的最长时间。
const SESSION_LINGER: Duration = Duration::from_secs(10);

/// 入口接不到出口的原因。
enum ProxyFail {
    /// 路由规则不允许。
    Rejected,
    /// 出口或上游代理不允许。
    Denied(String),
    /// 服务或主机不可达。
    Unreachable(String),
    /// 直连时对方拒绝。
    Refused(String),
}

/// 入口接到的出口。
enum Upstream<A, B> {
    Tcp(A),
    Remote(B),
}

impl<A: AsyncRead + AsyncWrite + Unpin, B: AsyncRead + AsyncWrite + Unpin> Upstream<A, B> {
    /// 先把已经读到的 `first` 送过去，然后双向转发，直到两个方向都结束。
    async fn splice(self, client: ProxyStream, first: Vec<u8>) -> io::Result<()> {
        async fn go<U: AsyncRead + AsyncWrite + Unpin>(mut client: ProxyStream, mut up: U, first: Vec<u8>) -> io::Result<()> {
            if !first.is_empty() {
                up.write_all(&first).await?;
            }
            tokio::io::copy_bidirectional(&mut client, &mut up).await.map(|_| ())
        }
        match self {
            Upstream::Tcp(a) => go(client, a, first).await,
            Upstream::Remote(b) => go(client, b, first).await,
        }
    }
}

/// 出口设备自己的本机地址。
fn is_own_address(ip: &std::net::IpAddr) -> bool {
    let ip = match ip {
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map(std::net::IpAddr::V4).unwrap_or(*ip),
        v4 => *v4,
    };
    ip.is_loopback() || ip.is_unspecified()
}

/// 再读一些到 `buf` 末尾；连接结束或出错时返回 false。
async fn fill<R: AsyncRead + Unpin + ?Sized>(r: &mut R, buf: &mut Vec<u8>) -> bool {
    let mut chunk = [0u8; 4096];
    match r.read(&mut chunk).await {
        Ok(0) | Err(_) => false,
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            true
        }
    }
}

/// 打开远端执行失败的原因。
enum OpenError {
    StaleRoster,
    Remote(WireError),
    Io(io::Error),
}

/// 等待对端响应的上限。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// 加入码的有效期。
pub const JOIN_CODE_TTL_MS: u64 = 10 * 60 * 1000;

const KEY_DEVICE: &str = "device_key";
const KEY_ROOT_PUB: &str = "root_pub";
const KEY_ROOT_KEY: &str = "root_key";
const KEY_ROSTER: &str = "roster";
const KEY_ADDRS: &str = "addrs";
const KEY_AGENTS: &str = "agents";
const KEY_SERVICES: &str = "services";
const KEY_ROUTES: &str = "routes";
const KEY_EXIT: &str = "exit";
/// 用户对网络接口是否计费的设置（M5.2）：接口名 → 是否计费。
const KEY_METERED: &str = "metered-v1";
/// 多长时间重新查看一次本机的网络接口（地址和计费的接口可能变了）。
const INTERFACES_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// 统一入口上的一条连接（SOCKS5 或 HTTP 代理），由常驻程序从本机监听的端口交给节点。
pub type ProxyStream = Box<dyn ProxyIo + Send>;

/// 完整节点的虚拟网卡上收到的东西，由常驻程序交给节点（M3.4）。
pub enum TunInbound {
    /// 一条 TCP 连接，`dst` 是程序连的地址。
    Tcp { stream: ProxyStream, dst: std::net::SocketAddr },
    /// 一个发给名字服务的查询；回应从 `reply` 送回（为空时不回应）。
    Dns { query: Vec<u8>, reply: tokio::sync::oneshot::Sender<Vec<u8>> },
}

/// 完整节点的虚拟网卡。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunInfo {
    pub name: String,
    pub net: (std::net::Ipv4Addr, u8),
    pub dns: std::net::Ipv4Addr,
}

/// 名字服务给出的回答的有效期（秒）。
const DNS_TTL: u32 = 30;
pub trait ProxyIo: AsyncRead + AsyncWrite + Unpin {}
impl<X: AsyncRead + AsyncWrite + Unpin> ProxyIo for X {}

/// 入口握手（读出目标）的时间上限。
const PROXY_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// HTTP 代理请求头的长度上限。
const PROXY_MAX_HEAD: usize = 64 * 1024;

/// 本机发布的一个服务。`addr` 只留在本机，不出现在任何发给别人的消息里。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PublishedService {
    name: String,
    addr: std::net::SocketAddr,
    proto: String,
    env: Vec<(String, String)>,
}

impl PublishedService {
    fn info(&self) -> ServiceInfo {
        ServiceInfo { name: self.name.clone(), proto: self.proto.clone(), env: self.env.clone() }
    }
}

/// 服务的协议：只用来告诉使用方怎么用。
const SERVICE_PROTOS: [&str; 2] = ["tcp", "http"];
const LOG_AUDIT: &str = "audit";
/// 审计日志的轮转上限：最近这么多字节（约 4 万条）的记录总在，总占用不超过约两倍。
const AUDIT_LOG_MAX: u64 = 8 << 20;

/// 一个请求需要的能力和资源。名册推送和加入邀请不走授权判定，返回 `None`。
/// 调用方和被调用方用同一个函数算，agent 签署的动作因此与被调用方判定的动作一致。
fn required_action(body: &Request, node: &str) -> Result<Option<Action>, WireError> {
    let (capability, session) = match body {
        Request::NodeInfo => ("node.info", None),
        Request::RosterSync => ("roster.sync", None),
        Request::Exec(_) => ("term.exec", None),
        Request::SessionAttach { name, .. } => ("term.session.attach", Some(name)),
        Request::SessionList => ("term.session.list", None),
        Request::SessionKill { name } => ("term.session.kill", Some(name)),
        Request::SvcList | Request::DriveInfo => ("node.info", None),
        Request::DriveRegister { ws }
        | Request::DriveHave { ws, .. }
        | Request::DrivePush { ws, .. }
        | Request::DriveGet { ws, .. }
        | Request::DriveProfilePut { ws, .. }
        | Request::DriveProfileGet { ws, .. } => {
            if !nexus_drive_core::path::valid_workspace(ws) {
                return Err(err(ErrorCode::InvalidInput, format!("工作区名不合法：{ws}")));
            }
            let read = matches!(body, Request::DriveGet { .. } | Request::DriveProfileGet { .. });
            let cap = if read { "drive.read" } else { "drive.write" };
            return Ok(Some(Action {
                capability: Capability::parse(cap).expect("内置能力名合法"),
                resource: ResourcePath::parse(&format!("{node}/drive/{ws}")).map_err(|_| internal("本机不在名册里"))?,
            }));
        }
        Request::HardwareList | Request::HardwareCapture { .. } => {
            return Err(err(ErrorCode::Denied, "常驻设备不提供网页硬件能力"));
        }
        Request::SvcConnect { name } => {
            if !valid_segment(name) {
                return Err(err(ErrorCode::InvalidInput, format!("服务名 {name:?} 不合法：只能用字母、数字、- 和 _")));
            }
            return Ok(Some(Action {
                capability: Capability::parse("svc.connect").expect("内置能力名合法"),
                resource: ResourcePath::parse(&format!("{node}/svc/{name}")).map_err(|_| internal("本机不在名册里"))?,
            }));
        }
        Request::ExitConnect { .. } => {
            return Ok(Some(Action {
                capability: Capability::parse("net.exit").expect("内置能力名合法"),
                resource: ResourcePath::parse(&format!("{node}/exit")).map_err(|_| internal("本机不在名册里"))?,
            }));
        }
        Request::RosterPush { .. } | Request::JoinOffer { .. } | Request::WebEnroll { .. } | Request::WebLogin { .. } => {
            return Ok(None)
        }
    };
    if let Some(name) = session {
        if !valid_segment(name) {
            return Err(err(ErrorCode::InvalidInput, format!("会话名 {name:?} 不合法：只能用字母、数字、- 和 _")));
        }
    }
    let resource = match session {
        Some(name) => format!("{node}/session/{name}"),
        None => node.to_string(),
    };
    Ok(Some(Action {
        capability: Capability::parse(capability).expect("内置能力名合法"),
        resource: ResourcePath::parse(&resource).map_err(|_| internal("本机不在名册里"))?,
    }))
}

/// 拒绝原因的说明，给人看。
fn deny_text(reason: &DenyReason) -> &'static str {
    match reason {
        DenyReason::PeerNotInRoster => "发出请求的设备不在名册里（可能已被移出）",
        DenyReason::CertBadSignature => "凭据的证书签名无效（凭据被改动过，或不属于这个网络）",
        DenyReason::CertNotYetValid => "凭据还没有生效",
        DenyReason::CertExpired => "凭据已过期",
        DenyReason::AgentRevoked => "这个 agent 已被撤销",
        DenyReason::AssertionBadSignature => "请求签名无效（私钥与证书不匹配）",
        DenyReason::WrongTarget => "请求是发给另一台设备的",
        DenyReason::ActionMismatch => "签署的操作与实际请求不一致",
        DenyReason::Stale => "请求的签署时间与当前时间相差太远（检查两台设备的时钟）",
        DenyReason::NoMatchingGrant => "没有授权做这件事",
        DenyReason::Replayed => "同一个请求被重复使用",
        DenyReason::IssuerNotInRoster => "网页登录凭证的签发设备不在名册里",
        DenyReason::SessionBadSignature => "网页登录凭证无效",
        DenyReason::SessionNotYetValid => "网页登录凭证还没有生效（检查时钟）",
        DenyReason::SessionExpired => "网页登录已过期，请重新登录",
        DenyReason::SessionTooLong => "网页登录凭证的有效期超过上限",
        DenyReason::WrongBrowser => "网页登录凭证属于另一个浏览器",
        DenyReason::WebKeyRevoked => "这个通行密钥已被撤销",
    }
}

fn denied(reason: &DenyReason) -> WireError {
    err(ErrorCode::Denied, format!("访问被拒绝：{}", deny_text(reason)))
}

/// 解析 `能力@资源` 形式的授权条目。
fn parse_grant(text: &str) -> Result<Grant, WireError> {
    let bad = |why: String| err(ErrorCode::InvalidInput, format!("授权条目 {text:?} 不合法：{why}。格式为 能力@资源，例如 term.exec@home"));
    let (cap, res) = text.split_once('@').ok_or_else(|| bad("缺少 @".into()))?;
    Ok(Grant {
        capability: CapabilityPattern::parse(cap).map_err(|e| bad(e.to_string()))?,
        resource: ResourcePattern::parse(res).map_err(|e| bad(e.to_string()))?,
    })
}

fn grant_text(g: &Grant) -> String {
    format!("{}@{}", g.capability.as_str(), g.resource.as_str())
}

/// 关闭一条流之前读到对方的结束标记。
///
/// 一次调用里双方都只发一帧，然后关闭自己的写方向。如果在对方的结束标记到达之前就丢弃流，
/// 底层会把它当成「还有没读的数据就关闭」，向对方发送重置，可能让对方丢掉还没读完的回应。
async fn drain<R: tokio::io::AsyncRead + Unpin>(stream: &mut R) {
    let _ = tokio::time::timeout(Duration::from_secs(2), tokio::io::copy(stream, &mut tokio::io::sink())).await;
}

/// 读取本设备的私钥；第一次调用时生成并保存。节点标识就是它的公钥。
pub fn load_or_create_device_key<S: System>(sys: &S) -> io::Result<SecretKey> {
    if let Some(bytes) = sys.store_get(KEY_DEVICE)? {
        let seed: [u8; 32] = bytes
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "设备密钥文件损坏"))?;
        return Ok(SecretKey::from_seed(seed));
    }
    let mut seed = [0u8; 32];
    sys.random(&mut seed);
    sys.store_put(KEY_DEVICE, &seed)?;
    Ok(SecretKey::from_seed(seed))
}

fn load<S: System, T: DeserializeOwned>(sys: &S, key: &str) -> io::Result<Option<T>> {
    match sys.store_get(key)? {
        None => Ok(None),
        Some(bytes) => postcard::from_bytes(&bytes)
            .map(Some)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("{key} 文件损坏"))),
    }
}

fn save<S: System, T: Serialize>(sys: &S, key: &str, value: &T) -> io::Result<()> {
    sys.store_put(key, &postcard::to_allocvec(value).map_err(io::Error::other)?)
}

fn err(code: ErrorCode, message: impl Into<String>) -> WireError {
    WireError::new(code, message)
}

fn internal(e: impl std::fmt::Display) -> WireError {
    err(ErrorCode::Internal, e.to_string())
}

/// 本机关于「属于哪个网络」的全部状态，都持久化。
#[derive(Default)]
struct Trust {
    root: Option<PublicKey>,
    roster: Option<SignedRoster>,
    /// 只有管理设备持有。
    root_key: Option<[u8; 32]>,
    addrs: BTreeMap<NodeId, String>,
}

struct PendingJoin {
    nonce: [u8; 16],
    expires_at: Timestamp,
    /// 短加入码在碰头服务上的位置（M6.1）。重新开始加入时让它失效。
    short_seed: Option<[u8; 32]>,
}

type Task<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

pub struct Node<T, S: System> {
    transport: T,
    system: S,
    roster_write: tokio::sync::Mutex<()>,
    /// 尚未加入网络时使用的名字，也是加入时建议的名字。
    configured_name: String,
    started_at: Timestamp,
    trust: RefCell<Trust>,
    pending_join: RefCell<Option<PendingJoin>>,
    /// 需要在后台把名册推过去的节点。
    push_tx: mpsc::UnboundedSender<NodeId>,
    push_rx: RefCell<Option<mpsc::UnboundedReceiver<NodeId>>>,
    sessions: RefCell<BTreeMap<String, SessionHandle>>,
    next_client: Cell<ClientId>,
    spawn_tx: mpsc::UnboundedSender<NewSession<S::Pty>>,
    spawn_rx: RefCell<Option<mpsc::UnboundedReceiver<NewSession<S::Pty>>>>,
    replay: RefCell<ReplayGuard>,
    web_enrolls: RefCell<HashMap<[u8; 16], PendingWebEnroll>>,
    /// 本机发布的服务（M3.1）。
    services: RefCell<Vec<PublishedService>>,
    /// 本机是否开放出口（M3.3）。
    exit_enabled: Cell<bool>,
    /// 本机的路由规则（M3.2）。
    routes: RefCell<RouteTable>,
    proxy_tx: mpsc::UnboundedSender<ProxyStream>,
    proxy_rx: RefCell<Option<mpsc::UnboundedReceiver<ProxyStream>>>,
    /// 完整节点：虚拟网卡的名字和本机临时地址。
    tun: RefCell<Option<(String, FakePool)>>,
    tun_tx: mpsc::UnboundedSender<TunInbound>,
    tun_rx: RefCell<Option<mpsc::UnboundedReceiver<TunInbound>>>,
    /// 本机的工作区（M4.1）。
    drive_state: RefCell<drive::DriveState>,
    /// 用户对网络接口是否计费的设置（M5.2）。
    metered: RefCell<BTreeMap<String, bool>>,
    /// 名册每换一次加一：网页会话据此重新判定（例如通行密钥被撤销时立即断开）。
    roster_epoch: tokio::sync::watch::Sender<u64>,
}

impl<T: Transport, S: System> Node<T, S> {
    pub fn new(transport: T, system: S, name: &str) -> io::Result<Self> {
        let trust = Trust {
            root: load(&system, KEY_ROOT_PUB)?,
            roster: match system.store_get(KEY_ROSTER)? {
                None => None,
                Some(bytes) => Some(SignedRoster::decode_stored(&bytes).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("{KEY_ROSTER} 文件损坏"))
                })?),
            },
            root_key: load(&system, KEY_ROOT_KEY)?,
            addrs: load::<_, Vec<(NodeId, String)>>(&system, KEY_ADDRS)?.unwrap_or_default().into_iter().collect(),
        };
        for (node, hint) in &trust.addrs {
            transport.add_hint(*node, hint);
        }
        let (push_tx, push_rx) = mpsc::unbounded_channel();
        let (spawn_tx, spawn_rx) = mpsc::unbounded_channel();
        let services = load::<_, Vec<PublishedService>>(&system, KEY_SERVICES)?.unwrap_or_default();
        let routes = load::<_, RouteTable>(&system, KEY_ROUTES)?.unwrap_or_default();
        let exit_enabled = load::<_, bool>(&system, KEY_EXIT)?.unwrap_or(false);
        let (proxy_tx, proxy_rx) = mpsc::unbounded_channel();
        let (tun_tx, tun_rx) = mpsc::unbounded_channel();
        let drive_state = RefCell::new(drive::DriveState::load(&system)?);
        let metered = RefCell::new(load(&system, KEY_METERED)?.unwrap_or_default());
        Ok(Self {
            routes: RefCell::new(routes),
            exit_enabled: Cell::new(exit_enabled),
            proxy_tx,
            proxy_rx: RefCell::new(Some(proxy_rx)),
            tun: RefCell::new(None),
            drive_state,
            metered,
            tun_tx,
            tun_rx: RefCell::new(Some(tun_rx)),
            services: RefCell::new(services),
            sessions: RefCell::default(),
            next_client: Cell::new(1),
            spawn_tx,
            spawn_rx: RefCell::new(Some(spawn_rx)),
            replay: RefCell::default(),
            web_enrolls: RefCell::default(),
            roster_epoch: tokio::sync::watch::Sender::new(0),
            started_at: system.now(),
            transport,
            system,
            roster_write: tokio::sync::Mutex::new(()),
            configured_name: name.to_string(),
            trust: RefCell::new(trust),
            pending_join: RefCell::new(None),
            push_tx,
            push_rx: RefCell::new(Some(push_rx)),
        })
    }

    fn ws_exists(&self, ws: &str) -> bool {
        self.drive_state.borrow().workspaces.contains_key(ws)
    }

    /// 统一入口：常驻程序把本机入口上接受的连接从这里交给节点。
    pub fn proxy_entry(&self) -> mpsc::UnboundedSender<ProxyStream> {
        self.proxy_tx.clone()
    }

    /// 成为完整节点：常驻程序建好虚拟网卡后调用，之后把网卡上收到的东西从返回的通道交给节点。
    pub fn enable_tun(&self, name: &str, pool: FakePool) -> mpsc::UnboundedSender<TunInbound> {
        *self.tun.borrow_mut() = Some((name.to_string(), pool));
        self.tun_tx.clone()
    }

    pub fn tun_info(&self) -> Option<TunInfo> {
        self.tun.borrow().as_ref().map(|(name, pool)| TunInfo { name: name.clone(), net: pool.network(), dns: pool.dns() })
    }

    fn me(&self) -> NodeId {
        self.transport.node_id()
    }

    fn roster(&self) -> Option<Roster> {
        self.trust.borrow().roster.as_ref().map(|s| s.roster.clone())
    }

    fn roster_version(&self) -> u64 {
        self.trust.borrow().roster.as_ref().map_or(0, |s| s.roster.version)
    }

    /// 本节点的名字：在名册里时用名册中的名字。
    fn name(&self) -> String {
        self.roster()
            .and_then(|r| r.device(self.me()).map(|d| d.name.clone()))
            .unwrap_or_else(|| self.configured_name.clone())
    }

    /// 本节点此刻的自述。
    pub fn info(&self) -> NodeInfo {
        NodeInfo {
            node_id: self.me(),
            name: self.name(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: self.started_at,
            now: self.system.now(),
            roster_version: self.trust.borrow().roster.as_ref().map(|s| s.roster.version),
        }
    }

    /// 已知的地址提示，加上本机自己的。随名册一起发给别的节点。
    fn addr_hints(&self) -> AddrHints {
        let mut hints: AddrHints = self.trust.borrow().addrs.iter().map(|(k, v)| (*k, v.clone())).collect();
        hints.retain(|(id, _)| *id != self.me());
        hints.push((self.me(), self.transport.local_hint()));
        hints
    }

    fn learn_hints(&self, hints: &AddrHints) -> io::Result<()> {
        let mut trust = self.trust.borrow_mut();
        for (node, hint) in hints {
            if *node != self.me() {
                self.transport.add_hint(*node, hint);
                trust.addrs.insert(*node, hint.clone());
            }
        }
        let list: Vec<_> = trust.addrs.iter().map(|(k, v)| (*k, v.clone())).collect();
        save(&self.system, KEY_ADDRS, &list)
    }

    /// 采用一份新名册：校验根、签名和版本，然后落盘。
    fn adopt_roster(&self, candidate: &SignedRoster) -> Result<(), WireError> {
        let root = self.trust.borrow().root.ok_or_else(|| err(ErrorCode::InvalidState, "本机还没有加入任何网络"))?;
        let current = self.trust.borrow().roster.clone();
        accept_update(current.as_ref(), candidate, &root).map_err(|e| err(ErrorCode::InvalidInput, e.to_string()))?;
        save(&self.system, KEY_ROSTER, candidate).map_err(internal)?;
        self.trust.borrow_mut().roster = Some(candidate.clone());
        self.roster_epoch.send_modify(|v| *v += 1);
        Ok(())
    }

    /// 驱动整个节点：接受连接、处理请求、处理本机控制调用。正常情况下不返回。
    pub async fn run(&self, ctl: &mut mpsc::Receiver<CtlCall>) -> io::Result<()> {
        let mut pushes = self.push_rx.borrow_mut().take().expect("run 只能调用一次");
        let mut spawns = self.spawn_rx.borrow_mut().take().expect("run 只能调用一次");
        let mut proxied = self.proxy_rx.borrow_mut().take().expect("run 只能调用一次");
        let mut tunneled = self.tun_rx.borrow_mut().take().expect("run 只能调用一次");
        self.adopt_sessions().await;
        let mut tasks: FuturesUnordered<Task<'_>> = FuturesUnordered::new();
        tasks.push(Box::pin(self.follow_anchor()));
        // 启动时把名册和本机现在的地址推给其他设备：重启后地址变了，它们才连得回来（M7.2）。
        tasks.push(Box::pin(async {
            if self.roster().is_some() {
                let _ = self.broadcast_roster(None).await;
            }
        }));
        if self.transport.has_interfaces() {
            tasks.push(Box::pin(self.follow_interfaces()));
        }
        // accept 不保证可以安全取消，所以让同一个 future 跨循环存活。
        let mut accept = Box::pin(self.transport.accept());
        loop {
            tokio::select! {
                biased;
                Some(()) = tasks.next(), if !tasks.is_empty() => {}
                conn = &mut accept => {
                    let (conn, proto) = conn?;
                    accept = Box::pin(self.transport.accept());
                    tasks.push(Box::pin(self.serve_conn(conn, proto)));
                }
                Some((req, agent, server)) = ctl.recv() => {
                    tasks.push(Box::pin(self.handle_ctl(req, agent, server)));
                }
                Some(peer) = pushes.recv() => {
                    tasks.push(Box::pin(async move {
                        let _ = tokio::time::timeout(REQUEST_TIMEOUT, self.push_roster(peer)).await;
                    }));
                }
                Some(new) = spawns.recv() => {
                    tasks.push(Box::pin(self.run_session(new)));
                }
                Some(conn) = proxied.recv() => {
                    tasks.push(Box::pin(self.serve_proxy(conn)));
                }
                Some(inbound) = tunneled.recv() => match inbound {
                    TunInbound::Tcp { stream, dst } => tasks.push(Box::pin(self.serve_tun(stream, dst))),
                    TunInbound::Dns { query, reply } => tasks.push(Box::pin(async move {
                        let _ = reply.send(self.answer_dns(&query).await);
                    })),
                },
            }
        }
    }

    /// 定期重新查看本机的网络接口，让传输知道哪些接口计费（M5.2）。
    async fn follow_interfaces(&self) {
        loop {
            let overrides = self.metered.borrow().clone();
            self.transport.interfaces(&overrides);
            tokio::time::sleep(INTERFACES_EVERY).await;
        }
    }

    /// 本机的网络接口；`set` 时先改设置（并保存）。
    fn interfaces(&self, set: Option<(String, Option<bool>)>) -> Result<Vec<nexus_wire::IfaceInfo>, WireError> {
        if let Some((name, value)) = set {
            let mut m = self.metered.borrow_mut();
            match value {
                Some(v) => m.insert(name, v),
                None => m.remove(&name),
            };
            save(&self.system, KEY_METERED, &*m).map_err(internal)?;
        }
        let overrides = self.metered.borrow().clone();
        Ok(self.transport.interfaces(&overrides))
    }

    /// 名册里有锚点时把它用作中继；名册每次变化都检查一遍。清除锚点后，已经加入的中继保留到重启。
    async fn follow_anchor(&self) {
        let mut epoch = self.roster_epoch.subscribe();
        let mut applied: Option<String> = None;
        loop {
            let anchor = self.roster().and_then(|r| r.anchor);
            if let Some(url) = anchor.as_deref().filter(|_| anchor != applied) {
                if self.transport.use_relay(url).await.is_ok() {
                    applied = anchor.clone();
                }
            }
            if epoch.changed().await.is_err() {
                return;
            }
        }
    }

    /// 一个连接上的请求并发处理：终端会话这样的长流不会挡住同一连接上的其他请求。
    async fn serve_conn(&self, conn: T::Conn, proto: Proto) {
        let peer = conn.remote();
        let mut streams: FuturesUnordered<Task<'_>> = FuturesUnordered::new();
        let mut accept = Box::pin(conn.accept_bi());
        loop {
            tokio::select! {
                biased;
                Some(()) = streams.next(), if !streams.is_empty() => {}
                stream = &mut accept => match stream {
                    Ok(Some(stream)) => {
                        streams.push(Box::pin(self.serve_stream(&conn, peer, proto, stream)));
                        accept = Box::pin(conn.accept_bi());
                    }
                    _ => break,
                },
            }
        }
        while streams.next().await.is_some() {}
    }

    async fn serve_stream(&self, conn: &T::Conn, peer: NodeId, proto: Proto, mut stream: <T::Conn as Connection>::Stream) {
        let Ok(header) = read_msg::<_, RequestHeader>(&mut stream).await else { return };
        let body = if header.version != WIRE_VERSION {
            Err(err(ErrorCode::UnsupportedVersion, format!("协议版本不兼容：对方 {}，本机 {WIRE_VERSION}", header.version)))
        } else {
            header.request(&self.describe_self())
        };
        let body = match body {
            Ok(body) => body,
            Err(e) => {
                // 旧版本不认识新请求（M7.1）：回应说明，不断开。
                if write_response(&mut stream, &Response::Error(e)).await.is_ok() {
                    let _ = stream.shutdown().await;
                    drain(&mut stream).await;
                }
                return;
            }
        };
        // 回应方向的优先级由这一端定：和请求同一类。
        conn.set_priority(&mut stream, body.class().priority());
        if matches!(body, Request::RosterPush { .. }) && self.roster().is_some_and(|r| r.device(peer).is_some()) {
            self.transport.peer_announced(conn);
        }
        let dispatch = match proto {
            Proto::Rpc => self.handle_rpc(peer, header, body),
            Proto::Join => Dispatch::Reply(self.handle_join(body)),
            Proto::Web => match body {
                Request::WebEnroll { token, credential } => Dispatch::WebEnroll { token, credential },
                Request::WebLogin { issued_at, assertion } => Dispatch::Reply(self.web_login(peer, issued_at, &assertion)),
                _ => Dispatch::Reply(Response::Error(err(ErrorCode::InvalidInput, "网页协议上不接受这个请求"))),
            },
        };
        let response = match dispatch {
            Dispatch::Reply(r) => r,
            Dispatch::Exec(spec) => return self.serve_exec(stream, spec).await,
            Dispatch::Session { name, create, size, guard } => {
                return self.serve_session(stream, &name, create, size, guard).await
            }
            Dispatch::Pipe { addr, name } => return self.serve_pipe(stream, addr, &name).await,
            Dispatch::Exit { target } => return self.serve_exit(stream, target).await,
            Dispatch::DriveGet { hashes } => return self.serve_drive_get(stream, hashes).await,
            Dispatch::DrivePush { ws, head, count } => return self.serve_drive_push(stream, ws, head, count).await,
            Dispatch::WebEnroll { token, credential } => match self.web_enroll_finish(token, credential).await {
                Ok(label) => Response::WebEnrolled { label },
                Err(e) => Response::Error(e),
            },
        };
        if write_response(&mut stream, &response).await.is_err() {
            return;
        }
        let _ = stream.shutdown().await;
        drain(&mut stream).await;
    }

    /// 写进版本说明里的本机：名字和软件版本。
    fn describe_self(&self) -> String {
        let name = self.roster().and_then(|r| r.device(self.me()).map(|d| d.name.clone())).unwrap_or_else(|| self.configured_name.clone());
        format!("{name}（Nexus {}）", env!("CARGO_PKG_VERSION"))
    }

    fn handle_rpc(&self, peer: NodeId, header: RequestHeader, body: Request) -> Dispatch {
        // 名册推送只靠签名证明自己，不要求发送方已经在本机的旧名册里。
        if let Request::RosterPush { roster, addrs } = &body {
            return Dispatch::Reply(match self.adopt_roster(roster) {
                Ok(()) => {
                    let _ = self.learn_hints(addrs);
                    Response::Ok
                }
                Err(e) if matches!(e.code, ErrorCode::InvalidInput) => {
                    // 不比本机新：名册不用更新，但名册里的设备重启后地址可能变了（M7.2），记下它给的地址。
                    if self.roster().is_some_and(|r| r.device(peer).is_some()) {
                        let _ = self.learn_hints(addrs);
                    }
                    Response::Ok
                }
                Err(e) => Response::Error(e),
            });
        }
        let mine = self.roster_version();
        // 浏览器没有名册：不交换版本。
        let web = matches!(header.principal, Principal::Web { .. });
        if !web && header.roster_version > mine {
            return Dispatch::Reply(Response::Error(err(ErrorCode::StaleRoster, format!("本机名册版本 {mine} 较旧"))));
        }
        let Some(roster) = self.roster() else {
            return Dispatch::Reply(Response::Error(err(ErrorCode::Denied, "本机还没有加入任何网络")));
        };
        let me = roster.device(self.me()).map(|d| d.name.clone()).unwrap_or_default();
        let action = match required_action(&body, &me) {
            Ok(Some(action)) => action,
            Ok(None) => return Dispatch::Reply(Response::Error(err(ErrorCode::InvalidInput, "这个请求不能在此协议上使用"))),
            Err(e) => return Dispatch::Reply(Response::Error(e)),
        };
        if let Err(e) = self.authorize(&roster, peer, &header.principal, &action) {
            return Dispatch::Reply(Response::Error(e));
        }
        if !web && mine > header.roster_version {
            // 对方的名册旧了，稍后在后台推过去。
            let _ = self.push_tx.send(peer);
        }
        Dispatch::Reply(match body {
            Request::NodeInfo => Response::NodeInfo(self.info()),
            Request::Exec(spec) => return Dispatch::Exec(spec),
            Request::SessionAttach { name, create, size } => {
                let guard = Some(Box::new(SessionGuard::new(peer, header.principal.clone(), action)));
                return Dispatch::Session { name, create, size, guard };
            }
            Request::SessionList => Response::Sessions(self.session_list(|p| principal_sees(&roster, &header.principal, p))),
            Request::SvcList => Response::Services(self.service_list(&me, |a| principal_may(&roster, &header.principal, a))),
            Request::DriveInfo => self.drive_info(|ws| {
                let action = Action {
                    capability: Capability::parse("drive.read").expect("内置能力名合法"),
                    resource: match ResourcePath::parse(&format!("{me}/drive/{ws}")) {
                        Ok(r) => r,
                        Err(_) => return false,
                    },
                };
                principal_may(&roster, &header.principal, &action)
            }),
            Request::DriveRegister { ws } => self.drive_register(&ws),
            Request::DriveHave { ws, hashes } => self.drive_have(&ws, hashes),
            Request::DriveProfilePut { ws, name, files } => self.drive_profile_put(&ws, &name, files),
            Request::DriveProfileGet { ws, name } => self.drive_profile_get(&ws, &name),
            Request::HardwareList | Request::HardwareCapture { .. } => {
                Response::Error(err(ErrorCode::Denied, "常驻设备不提供网页硬件能力"))
            }
            Request::DriveGet { ws, hashes } => match self.ws_exists(&ws) {
                true => return Dispatch::DriveGet { hashes },
                false => Response::Error(err(ErrorCode::NotFound, format!("{me} 上没有工作区 {ws}"))),
            },
            Request::DrivePush { ws, head, count } => match self.ws_exists(&ws) {
                true => return Dispatch::DrivePush { ws, head, count },
                false => Response::Error(err(ErrorCode::NotFound, format!("{me} 上没有工作区 {ws}"))),
            },
            Request::SvcConnect { name } => match self.service_addr(&name) {
                Some(addr) => return Dispatch::Pipe { addr, name },
                None => Response::Error(err(ErrorCode::NotFound, format!("{me} 上没有发布服务 {name}"))),
            },
            Request::ExitConnect { target } => {
                if !self.exit_enabled.get() {
                    Response::Error(err(ErrorCode::Denied, format!("{me} 没有开放出口（在 {me} 上执行 nexus exit on）")))
                } else {
                    match Target::parse(&target) {
                        Some(target) => return Dispatch::Exit { target },
                        None => Response::Error(err(ErrorCode::InvalidInput, format!("目标应当写成 主机:端口：{target}"))),
                    }
                }
            }
            Request::SessionKill { name } => match self.session_kill(&name) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error(e),
            },
            _ => Response::Roster {
                roster: (mine > header.roster_version).then(|| self.trust.borrow().roster.clone()).flatten(),
                addrs: self.addr_hints(),
            },
        })
    }

    /// 处理加入协议上的邀请。邀请方不在本机名册里（本机还没有名册），
    /// 能证明它有权邀请的只有一次性随机数：只有看到加入码的人才知道它。
    fn handle_join(&self, body: Request) -> Response {
        let Request::JoinOffer { nonce, roster, addrs } = body else {
            return Response::Error(err(ErrorCode::InvalidInput, "加入协议上只接受邀请"));
        };
        let result = (|| {
            if self.trust.borrow().root.is_some() {
                return Err(err(ErrorCode::InvalidState, "本机已经属于一个网络"));
            }
            let pending = self.pending_join.borrow();
            let pending = pending.as_ref().ok_or_else(|| err(ErrorCode::InvalidState, "本机没有在等待加入"))?;
            if self.system.now() >= pending.expires_at {
                return Err(err(ErrorCode::InvalidState, "加入码已过期"));
            }
            if pending.nonce != nonce {
                return Err(err(ErrorCode::Denied, "加入码不匹配"));
            }
            let root = roster.roster.root;
            let verified = roster.verify(&root).map_err(|e| err(ErrorCode::InvalidInput, e.to_string()))?;
            if verified.device(self.me()).is_none() {
                return Err(err(ErrorCode::InvalidInput, "邀请的名册里没有本机"));
            }
            Ok(root)
        })();
        let root = match result {
            Ok(root) => root,
            Err(e) => return Response::Error(e),
        };
        let persist = || -> io::Result<()> {
            save(&self.system, KEY_ROOT_PUB, &root)?;
            save(&self.system, KEY_ROSTER, &roster)?;
            Ok(())
        };
        if let Err(e) = persist() {
            return Response::Error(internal(e));
        }
        {
            let mut trust = self.trust.borrow_mut();
            trust.root = Some(root);
            trust.roster = Some(roster);
            self.roster_epoch.send_modify(|v| *v += 1);
        }
        *self.pending_join.borrow_mut() = None;
        let _ = self.learn_hints(&addrs);
        Response::Ok
    }

    /// 向另一个节点发起一次调用。对方的名册比本机旧时，先把名册推过去再重试一次。
    async fn request(&self, to: &Device, body: Request, agent: Option<&AgentCredential>) -> io::Result<(Response, Option<nexus_core_proto::PathKind>)> {
        // 每次发送都重新签署：agent 的请求不能重复使用。
        let principal = self.principal_for(agent, to, &body).map_err(|e| io::Error::other(e.message))?;
        let (resp, path) = self.request_once(to.node_id, Proto::Rpc, principal, body.clone()).await?;
        if let Response::Error(WireError { code: ErrorCode::StaleRoster, .. }) = resp {
            self.push_roster(to.node_id).await?;
            let principal = self.principal_for(agent, to, &body).map_err(|e| io::Error::other(e.message))?;
            return self.request_once(to.node_id, Proto::Rpc, principal, body).await;
        }
        Ok((resp, path))
    }

    /// 本次请求的主体：本机设备，或代 agent 签署这一次具体的操作。
    fn principal_for(&self, agent: Option<&AgentCredential>, to: &Device, body: &Request) -> Result<Principal, WireError> {
        let Some(agent) = agent else { return Ok(Principal::Device) };
        let action = required_action(body, &to.name)?.ok_or_else(|| denied(&DenyReason::NoMatchingGrant))?;
        let mut nonce = [0u8; 16];
        self.system.random(&mut nonce);
        let assertion = Assertion { target: to.node_id, action, issued_at: self.system.now(), nonce };
        let signature = assertion.sign(&SecretKey::from_seed(agent.secret));
        Ok(Principal::Agent { cert: agent.cert.clone(), assertion, signature })
    }

    /// 判定一次访问并记入审计。agent 的请求还要通过重放检测。
    fn authorize(&self, roster: &Roster, peer: NodeId, principal: &Principal, action: &Action) -> Result<(), WireError> {
        let now = self.system.now();
        let ctx = Context { roster, me: self.me(), now };
        let mut verdict = authz::decide(&ctx, peer, principal, action);
        if let (Outcome::Allow, Principal::Agent { cert, assertion, .. }) = (&verdict.outcome, principal) {
            if !self.replay.borrow_mut().first_use(&cert.cert.agent, assertion, now) {
                verdict.outcome = Outcome::Deny(DenyReason::Replayed);
                verdict.audit.outcome = verdict.outcome.clone();
            }
        }
        self.audit(roster, &verdict.audit);
        match verdict.outcome {
            Outcome::Allow => Ok(()),
            Outcome::Deny(reason) => Err(denied(&reason)),
        }
    }

    fn audit(&self, roster: &Roster, event: &AuditEvent) {
        let peer_name = roster.device(event.peer).map(|d| d.name.clone()).unwrap_or_else(|| event.peer.short());
        let record = AuditRecord { event: event.clone(), peer_name };
        if let Ok(frame) = nexus_wire::encode(&record) {
            let _ = self.system.append_log(LOG_AUDIT, &frame, AUDIT_LOG_MAX);
        }
    }

    fn read_audit(&self, limit: Option<u32>) -> Result<Vec<AuditRecord>, WireError> {
        let bytes = self.system.read_log(LOG_AUDIT).map_err(internal)?;
        let mut decoder = nexus_wire::FrameDecoder::default();
        decoder.push(&bytes);
        let mut records = Vec::new();
        while let Ok(Some(r)) = decoder.next_frame::<AuditRecord>() {
            records.push(r);
        }
        let keep = limit.map_or(records.len(), |n| n as usize);
        let skip = records.len().saturating_sub(keep);
        Ok(records.split_off(skip))
    }

    async fn request_once(
        &self,
        to: NodeId,
        proto: Proto,
        principal: Principal,
        body: Request,
    ) -> io::Result<(Response, Option<nexus_core_proto::PathKind>)> {
        let conn = self.transport.connect(to, proto).await?;
        let mut stream = conn.open_bi().await?;
        conn.set_priority(&mut stream, body.class().priority());
        let header = RequestHeader::new(self.roster_version(), principal, &body);
        write_msg(&mut stream, &header).await?;
        stream.shutdown().await?;
        let resp = read_response(&mut stream).await?;
        drain(&mut stream).await;
        Ok((resp, conn.path_kind()))
    }

    async fn push_roster(&self, to: NodeId) -> io::Result<()> {
        let Some(roster) = self.trust.borrow().roster.clone() else { return Ok(()) };
        let body = Request::RosterPush { roster, addrs: self.addr_hints() };
        match self.request_once(to, Proto::Rpc, Principal::Device, body).await?.0 {
            Response::Ok => Ok(()),
            other => Err(io::Error::other(format!("推送名册失败：{other:?}"))),
        }
    }

    /// 把当前名册推给除自己和 `except` 之外的所有设备，返回 (送达数, 未送达的设备名)。
    async fn broadcast_roster(&self, except: Option<NodeId>) -> (u32, Vec<String>) {
        let Some(roster) = self.roster() else { return (0, vec![]) };
        let targets: Vec<Device> = roster
            .devices
            .into_iter()
            .filter(|d| d.node_id != self.me() && Some(d.node_id) != except)
            .collect();
        let results = join_all(targets.iter().map(|d| async move {
            matches!(tokio::time::timeout(REQUEST_TIMEOUT, self.push_roster(d.node_id)).await, Ok(Ok(())))
        }))
        .await;
        let mut unreachable = Vec::new();
        let mut pushed = 0;
        for (d, ok) in targets.iter().zip(results) {
            if ok {
                pushed += 1;
            } else {
                unreachable.push(d.name.clone());
            }
        }
        (pushed, unreachable)
    }

    /// 处理一次本机控制调用。`agent` 不为空时，以该 agent 的身份执行，并且只能看到和做它被授权的事。
    pub async fn handle_ctl(&self, req: CtlRequest, agent: Option<AgentCredential>, server: CtlServer) {
        if let Some(a) = &agent {
            if let Err(e) = self.check_credential(a, &req) {
                let _ = server.tx.send(CtlResponse::Error(e)).await;
                return;
            }
        }
        let agent_ref = agent.as_ref();
        let result = match req {
            CtlRequest::Exec { node, spec } => return self.exec(&node, spec, agent, server).await,
            CtlRequest::Drive(req) => return self.drive(req, agent, server).await,
            CtlRequest::Attach { node, session, size, create } => return self.attach(&node, session, size, create, agent, server).await,
            CtlRequest::Status => Ok(CtlResponse::Status(self.local_status())),
            CtlRequest::Paths { device } => self.paths(device.as_deref()).map(CtlResponse::Paths),
            CtlRequest::Upgrade { .. } | CtlRequest::UpgradeStatus | CtlRequest::UpgradeRollback => {
                Err(err(ErrorCode::InvalidState, "这个常驻程序不支持升级（只有命令行的 nexus daemon 支持）"))
            }
            CtlRequest::Interfaces { set } => self.interfaces(set).map(CtlResponse::Interfaces),
            CtlRequest::RemoteStatus { node } => self.remote_status(&node, agent_ref).await.map(CtlResponse::Status),
            CtlRequest::Init { name } => self.init(name).map(CtlResponse::Init),
            CtlRequest::JoinStart => self.join_start().await,
            CtlRequest::Approve { code, name } => self.approve(&code, name).await.map(CtlResponse::Membership),
            CtlRequest::Revoke { name } => self.revoke(&name).await.map(CtlResponse::Membership),
            CtlRequest::Recover { code } => self.recover(&code).await.map(CtlResponse::Membership),
            CtlRequest::List => Ok(CtlResponse::List(self.list(agent_ref).await)),
            CtlRequest::SessionList { node } => self.remote_session_list(&node, agent_ref).await.map(CtlResponse::Sessions),
            CtlRequest::SessionKill { node, session } => {
                self.remote_session_kill(&node, &session, agent_ref).await.map(|()| CtlResponse::Ok)
            }
            CtlRequest::AgentIssue { label, grants, ttl_ms } => self.agent_issue(label, &grants, ttl_ms).map(CtlResponse::AgentIssued),
            CtlRequest::AgentRevoke { agent } => self.agent_revoke(&agent).await.map(CtlResponse::AgentRevoked),
            CtlRequest::AgentList => self.agent_list().map(CtlResponse::Agents),
            CtlRequest::Audit { limit } => self.read_audit(limit).map(CtlResponse::Audit),
            CtlRequest::SshHostKey { node } => self.ssh_host_key(&node, agent_ref).map(CtlResponse::SshHostKey),
            CtlRequest::WebEnroll { label, grants, ttl_ms, page } => {
                let page = page
                    .or_else(|| self.roster().and_then(|r| r.anchor))
                    .unwrap_or_else(|| DEFAULT_WEB_PAGE.to_string());
                self.web_enroll_start(label, &grants, ttl_ms, page.trim_end_matches('/')).map(CtlResponse::WebEnroll)
            }
            CtlRequest::WebList => self.web_list().map(CtlResponse::WebKeys),
            CtlRequest::WebRevoke { label } => self.web_revoke(&label).await.map(CtlResponse::WebRevoked),
            CtlRequest::HardwareList { node_id, hint } => {
                self.hardware_rpc(node_id, &hint, Request::HardwareList).await.and_then(|r| match r {
                    Response::HardwareList(list) => Ok(CtlResponse::HardwareList(list)),
                    Response::Error(e) => Err(e),
                    other => Err(internal(format!("网页节点的回应无法理解：{other:?}"))),
                })
            }
            CtlRequest::HardwareCapture { node_id, hint, kind } => {
                self.hardware_rpc(node_id, &hint, Request::HardwareCapture { kind }).await.and_then(|r| match r {
                    Response::HardwareCapture(cap) => Ok(CtlResponse::HardwareCapture(cap)),
                    Response::Error(e) => Err(e),
                    other => Err(internal(format!("网页节点的回应无法理解：{other:?}"))),
                })
            }
            CtlRequest::AnchorSet { url } => self.anchor_set(url).await.map(CtlResponse::Anchor),
            CtlRequest::Connect { node, name } => return self.connect(&node, &name, agent, server).await,
            CtlRequest::Publish { name, addr, proto, env } => self.publish(name, addr, proto, env).map(|path| CtlResponse::Published { path }),
            CtlRequest::Unpublish { name } => self.unpublish(&name).map(|()| CtlResponse::Ok),
            CtlRequest::Services { node } => {
                self.remote_services(&node, agent_ref).await.map(|(node, services)| CtlResponse::Services { node, services })
            }
            CtlRequest::RouteAdd { rule } => self.route_add(rule).map(|index| CtlResponse::RouteAdded { index }),
            CtlRequest::RouteRm { index } => self.route_rm(index).map(|()| CtlResponse::Ok),
            CtlRequest::ExitSet { enabled } => {
                self.exit_enabled.set(enabled);
                save(&self.system, KEY_EXIT, &enabled).map_err(internal).map(|()| CtlResponse::Exit { enabled })
            }
            CtlRequest::TunShow => Ok(CtlResponse::Tun(self.tun_info().map(|t| nexus_wire::TunReport {
                name: t.name,
                net: format!("{}/{}", t.net.0, t.net.1),
                dns: t.dns.to_string(),
            }))),
            CtlRequest::ExitShow => Ok(CtlResponse::Exit { enabled: self.exit_enabled.get() }),
            CtlRequest::RouteList => Ok(CtlResponse::Routes(self.routes.borrow().rules.clone())),
            CtlRequest::RouteExplain { target } => self.route_explain(&target).map(CtlResponse::Explained),
            CtlRequest::AnchorShow => Ok(CtlResponse::Anchor(AnchorReport {
                anchor: self.roster().and_then(|r| r.anchor),
                roster_version: self.roster_version(),
                pushed: 0,
                unreachable: vec![],
            })),
        };
        let _ = server.tx.send(result.unwrap_or_else(CtlResponse::Error)).await;
    }

    /// 带凭据的控制调用：凭据必须属于本网络，而且 agent 不能做管理操作。
    fn check_credential(&self, agent: &AgentCredential, req: &CtlRequest) -> Result<(), WireError> {
        let admin = matches!(
            req,
            CtlRequest::Init { .. }
                | CtlRequest::Recover { .. }
                | CtlRequest::JoinStart
                | CtlRequest::Approve { .. }
                | CtlRequest::Revoke { .. }
                | CtlRequest::AgentIssue { .. }
                | CtlRequest::AgentRevoke { .. }
                | CtlRequest::AgentList
                | CtlRequest::Audit { .. }
                | CtlRequest::WebEnroll { .. }
                | CtlRequest::WebList
                | CtlRequest::WebRevoke { .. }
                | CtlRequest::HardwareList { .. }
                | CtlRequest::HardwareCapture { .. }
                | CtlRequest::AnchorSet { .. }
                | CtlRequest::AnchorShow
                | CtlRequest::Publish { .. }
                | CtlRequest::Unpublish { .. }
                | CtlRequest::RouteAdd { .. }
                | CtlRequest::RouteRm { .. }
                | CtlRequest::ExitSet { .. }
                | CtlRequest::Interfaces { set: Some(_) }
        );
        if admin {
            return Err(err(ErrorCode::Denied, "agent 不能执行管理操作"));
        }
        if matches!(req, CtlRequest::Drive(_)) {
            // 工作区按每个请求判定（见 drive.rs 的 drive_allowed），这里只验证凭据本身。
            let roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机还没有加入任何网络"))?;
            return agent.cert.validate(&roster, self.system.now()).map_err(|reason| denied(&reason));
        }
        let roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机还没有加入任何网络"))?;
        // 发现接口以设备身份探测，不能把未经验证的 grants 当作可见性凭证。
        // 执行和会话请求仍交给目标设备完整判定并审计，包括无效凭据的拒绝。
        if matches!(req, CtlRequest::Status | CtlRequest::RemoteStatus { .. } | CtlRequest::Paths { .. } | CtlRequest::List | CtlRequest::SshHostKey { .. }) {
            agent.cert.validate(&roster, self.system.now()).map_err(|reason| denied(&reason))?;
        }
        if SecretKey::from_seed(agent.secret).public() != agent.cert.cert.agent {
            return Err(denied(&DenyReason::AssertionBadSignature));
        }
        Ok(())
    }

    /// 按名字找设备；agent 看不到的设备和不存在的设备得到同样的结果。
    fn resolve_for(&self, node: &str, agent: Option<&AgentCredential>) -> Result<Device, WireError> {
        let device = self.resolve(node)?;
        match agent {
            Some(a) if !visible(a, &device.name) => Err(err(ErrorCode::UnknownNode, format!("不认识节点 {node}"))),
            _ => Ok(device),
        }
    }

    /// agent 访问本机：和远端一样判定、审计、检测重放。
    fn authorize_local(&self, agent: Option<&AgentCredential>, body: &Request) -> Result<(), WireError> {
        let Some(agent) = agent else { return Ok(()) };
        let roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机还没有加入任何网络"))?;
        let me = roster.device(self.me()).cloned().ok_or_else(|| internal("本机不在名册里"))?;
        let principal = self.principal_for(Some(agent), &me, body)?;
        let action = required_action(body, &me.name)?.ok_or_else(|| internal("这个请求不需要授权"))?;
        self.authorize(&roster, self.me(), &principal, &action)
    }

    // ------------------------------------------------------------ agent 管理

    /// SSH 呈现层的主机密钥：由本机设备密钥对目标设备标识签名派生，因此稳定、不需要存储，
    /// 也不会和其他设备的密钥相同。SSH 只在本机两个进程之间运行，这把密钥只用来让 ssh 的主机校验稳定。
    fn ssh_host_key(&self, node: &str, agent: Option<&AgentCredential>) -> Result<[u8; 32], WireError> {
        let device = self.resolve_for(node, agent)?;
        let key = load_or_create_device_key(&self.system).map_err(internal)?;
        let msg = device.node_id.0;
        let sig = key.sign(nexus_core_proto::crypto::Purpose::SshHostKey, &msg).to_bytes();
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&sig[32..]);
        Ok(seed)
    }

    // ------------------------------------------------------------ 通行密钥

    /// 管理设备发起登记：记下一次性登记码，给出在浏览器里打开的链接。
    fn web_enroll_start(&self, label: String, grants: &[String], ttl_ms: u64, page: &str) -> Result<WebEnrollReport, WireError> {
        self.admin_key()?;
        let roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机没有名册"))?;
        if !valid_segment(&label) {
            return Err(err(ErrorCode::InvalidInput, format!("名字 {label:?} 不合法：只能用字母、数字、- 和 _")));
        }
        if roster.device_by_name(&label).is_some() || roster.web_key(&label).is_some() {
            return Err(err(ErrorCode::InvalidInput, format!("名字 {label} 已被使用（设备和通行密钥不能重名）")));
        }
        let grants: Vec<Grant> = if grants.is_empty() {
            DEFAULT_WEB_GRANTS.iter().map(|g| parse_grant(g)).collect::<Result<_, _>>()?
        } else {
            grants.iter().map(|g| parse_grant(g)).collect::<Result<_, _>>()?
        };
        let page = page.trim_end_matches('/');
        let rp_id = page_rp_id(page).ok_or_else(|| {
            err(ErrorCode::InvalidInput, format!("页面地址 {page} 不合法：应当是 https://域名，开发时可以用 http://localhost:端口"))
        })?;
        let mut token = [0u8; 16];
        self.system.random(&mut token);
        let expires_at = Timestamp(self.system.now().0.saturating_add(ttl_ms));
        self.web_enrolls.borrow_mut().insert(token, PendingWebEnroll { label: label.clone(), grants, rp_id, expires_at });
        let url = format!(
            "{page}/#enroll={}&node={}&hint={}",
            hex_encode(&token),
            self.me(),
            self.transport.local_hint()
        );
        Ok(WebEnrollReport { label, url, expires_at })
    }

    /// 浏览器交来新建的通行密钥：核对登记码和依赖方，写进名册并推送。
    async fn web_enroll_finish(&self, token: [u8; 16], credential: NewCredential) -> Result<String, WireError> {
        let _write = self.roster_write.lock().await;
        let invalid = || err(ErrorCode::Denied, "登记码无效、已用过或已过期");
        let (label, grants, key) = {
            let mut pending = self.web_enrolls.borrow_mut();
            let p = pending.get(&token).ok_or_else(invalid)?;
            if self.system.now() >= p.expires_at {
                pending.remove(&token);
                return Err(invalid());
            }
            // 依赖方不符时登记码保留：用户可以回到正确的页面重试。
            if credential.rp_id != p.rp_id {
                return Err(err(
                    ErrorCode::InvalidInput,
                    format!("通行密钥是为 {} 创建的，登记链接要求 {}", credential.rp_id, p.rp_id),
                ));
            }
            let key = P256Key::from_spki(&credential.spki).map_err(|_| err(ErrorCode::InvalidInput, "公钥不是 P-256（ES256）"))?;
            let p = pending.remove(&token).expect("刚刚找到");
            (p.label, p.grants, key)
        };
        let root = self.admin_key()?;
        let mut roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机没有名册"))?;
        roster.version += 1;
        roster.web_keys.push(WebKey {
            label: label.clone(),
            credential_id: credential.credential_id,
            public_key: key,
            rp_id: credential.rp_id,
            grants,
            added_at: self.system.now(),
        });
        roster.validate().map_err(|e| err(ErrorCode::InvalidInput, e.to_string()))?;
        self.install_roster(roster.sign(&root))?;
        drop(_write);
        self.broadcast_roster(None).await;
        Ok(label)
    }

    /// 浏览器用通行密钥登录：校验签名，签发网页会话凭证，给出它能看到的设备。
    fn web_login(&self, peer: NodeId, issued_at: Timestamp, assertion: &WebAuthnAssertion) -> Response {
        let result = (|| {
            let roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机还没有加入任何网络"))?;
            let key = roster
                .web_keys
                .iter()
                .find(|k| k.credential_id == assertion.credential_id)
                .ok_or_else(|| err(ErrorCode::Denied, "不认识这个通行密钥（可能已被撤销）"))?;
            let now = self.system.now();
            let me_name = roster.device(self.me()).map(|d| d.name.clone()).unwrap_or_default();
            let checked = if now.0.abs_diff(issued_at.0) > authz::MAX_CLOCK_SKEW_MS {
                Err(err(ErrorCode::Denied, "通行密钥的签名时间与设备时钟相差太远（检查两边的时钟）"))
            } else {
                let challenge = login_challenge(issued_at, peer);
                verify_assertion(&key.public_key, &key.rp_id, &challenge, assertion)
                    .map(|_| ())
                    .map_err(|e| err(ErrorCode::Denied, format!("通行密钥签名无效（{e:?}）")))
            };
            // 登录本身也记入审计。
            let outcome = match &checked {
                Ok(()) => Outcome::Allow,
                Err(_) => Outcome::Deny(DenyReason::NoMatchingGrant),
            };
            self.audit(
                &roster,
                &AuditEvent {
                    at: now,
                    peer,
                    principal: authz::PrincipalRef::Web { label: key.label.clone() },
                    capability: "web.login".into(),
                    resource: me_name,
                    outcome,
                },
            );
            checked?;
            let device_key = load_or_create_device_key(&self.system).map_err(internal)?;
            let session = WebSession {
                credential_id: key.credential_id.clone(),
                browser: peer,
                label: key.label.clone(),
                issuer: self.me(),
                issued_at: now,
                not_after: Timestamp(now.0 + authz::MAX_WEB_SESSION_MS),
            }
            .sign(&device_key);
            let hints: HashMap<NodeId, String> = self.addr_hints().into_iter().collect();
            let devices = roster
                .devices
                .iter()
                .filter(|d| ResourcePath::parse(&d.name).is_ok_and(|p| grants_see(&key.grants, &p)))
                .map(|d| WebDevice {
                    name: d.name.clone(),
                    node_id: d.node_id,
                    hint: if d.node_id == self.me() {
                        self.transport.local_hint()
                    } else {
                        hints.get(&d.node_id).cloned().unwrap_or_default()
                    },
                })
                .collect();
            Ok((session, devices))
        })();
        match result {
            Ok((session, devices)) => Response::WebLoggedIn { session, devices },
            Err(e) => Response::Error(e),
        }
    }

    fn web_list(&self) -> Result<Vec<WebKeyInfo>, WireError> {
        let roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机还没有加入任何网络"))?;
        Ok(roster
            .web_keys
            .iter()
            .map(|k| WebKeyInfo {
                label: k.label.clone(),
                rp_id: k.rp_id.clone(),
                grants: k.grants.iter().map(grant_text).collect(),
                added_at: k.added_at,
            })
            .collect())
    }

    /// 向网页节点发起一次硬件能力 RPC（M8.3）。
    async fn hardware_rpc(&self, node_id: NodeId, hint: &str, body: Request) -> Result<Response, WireError> {
        if !hint.is_empty() {
            self.transport.add_hint(node_id, hint);
        }
        let conn = self.transport.connect(node_id, Proto::Rpc).await.map_err(|e| err(ErrorCode::Unreachable, format!("连不上网页节点：{e}")))?;
        let mut stream = conn.open_bi().await.map_err(|e| err(ErrorCode::Unreachable, format!("连不上网页节点：{e}")))?;
        let header = RequestHeader::new(0, Principal::Device, &body);
        write_msg(&mut stream, &header).await.map_err(|e| err(ErrorCode::Unreachable, format!("发送请求失败：{e}")))?;
        let resp: Response = read_response(&mut stream).await.map_err(|e| err(ErrorCode::Unreachable, format!("读取回应失败：{e}")))?;
        let _ = stream.shutdown().await;
        Ok(resp)
    }

    async fn web_revoke(&self, label: &str) -> Result<MembershipReport, WireError> {
        let _write = self.roster_write.lock().await;
        let root = self.admin_key()?;
        let mut roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机没有名册"))?;
        let before = roster.web_keys.len();
        roster.web_keys.retain(|k| k.label != label);
        if roster.web_keys.len() == before {
            return Err(err(ErrorCode::NotFound, format!("没有名为 {label} 的通行密钥")));
        }
        roster.version += 1;
        self.install_roster(roster.sign(&root))?;
        drop(_write);
        let (pushed, unreachable) = self.broadcast_roster(None).await;
        Ok(MembershipReport { name: label.to_string(), node_id: self.me(), roster_version: self.roster_version(), pushed, unreachable })
    }

    /// 设置或清除名册里的锚点，推送到各设备。
    async fn anchor_set(&self, url: Option<String>) -> Result<AnchorReport, WireError> {
        let _write = self.roster_write.lock().await;
        let root = self.admin_key()?;
        let mut roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机没有名册"))?;
        let url = url.map(|u| u.trim_end_matches('/').to_string());
        roster.anchor = url;
        roster.version += 1;
        roster.validate().map_err(|e| err(ErrorCode::InvalidInput, e.to_string()))?;
        self.install_roster(roster.sign(&root))?;
        drop(_write);
        let (pushed, unreachable) = self.broadcast_roster(None).await;
        Ok(AnchorReport { anchor: self.roster().and_then(|r| r.anchor), roster_version: self.roster_version(), pushed, unreachable })
    }

    /// 本机签发的新名册：落盘并生效。
    fn install_roster(&self, signed: SignedRoster) -> Result<(), WireError> {
        save(&self.system, KEY_ROSTER, &signed).map_err(internal)?;
        self.trust.borrow_mut().roster = Some(signed);
        self.roster_epoch.send_modify(|v| *v += 1);
        Ok(())
    }

    fn load_agents(&self) -> Result<Vec<AgentInfo>, WireError> {
        load(&self.system, KEY_AGENTS).map(Option::unwrap_or_default).map_err(internal)
    }

    fn agent_issue(&self, label: String, grants: &[String], ttl_ms: u64) -> Result<AgentIssueReport, WireError> {
        let root = self.admin_key()?;
        if !valid_segment(&label) {
            return Err(err(ErrorCode::InvalidInput, format!("agent 名 {label:?} 不合法：只能用字母、数字、- 和 _")));
        }
        if ttl_ms == 0 {
            return Err(err(ErrorCode::InvalidInput, "有效期必须大于 0"));
        }
        if grants.is_empty() {
            return Err(err(ErrorCode::InvalidInput, "至少要有一条授权，例如 --grant term.exec@home"));
        }
        let grants = grants.iter().map(|g| parse_grant(g)).collect::<Result<Vec<_>, _>>()?;
        let mut agents = self.load_agents()?;
        if agents.iter().any(|a| a.label == label && !a.revoked) {
            return Err(err(ErrorCode::InvalidInput, format!("已经有一个叫 {label} 的 agent，请换个名字或先撤销它")));
        }
        let mut seed = [0u8; 32];
        self.system.random(&mut seed);
        let key = SecretKey::from_seed(seed);
        let now = self.system.now();
        let not_after = Timestamp(now.0.saturating_add(ttl_ms));
        let cert = AgentCert { agent: key.public(), label: label.clone(), not_before: now, not_after, grants: grants.clone() }.issue(&root);
        agents.push(AgentInfo {
            agent_id: key.public(),
            label: label.clone(),
            issued_at: now,
            not_after,
            grants: grants.iter().map(grant_text).collect(),
            revoked: false,
        });
        save(&self.system, KEY_AGENTS, &agents).map_err(internal)?;
        let credential = AgentCredential { cert, secret: seed }.encode();
        Ok(AgentIssueReport { agent_id: key.public(), label, not_after, credential })
    }

    async fn agent_revoke(&self, who: &str) -> Result<MembershipReport, WireError> {
        let _write = self.roster_write.lock().await;
        let root = self.admin_key()?;
        let mut agents = self.load_agents()?;
        let index = agents
            .iter()
            .position(|a| !a.revoked && (a.label == who || NodeId(a.agent_id.0).to_string() == who));
        // 不在本机列表里的 agent（恢复管理设备之后，M6.2）：用完整标识撤销。
        let (id, label) = match index {
            Some(i) => (agents[i].agent_id, agents[i].label.clone()),
            None => match who.parse::<NodeId>().ok().map(|n| PublicKey(n.0)) {
                Some(id) => (id, NodeId(id.0).short()),
                None => return Err(err(ErrorCode::NotFound, format!("没有名为 {who} 的有效 agent（不在本机列表里的 agent 用它的完整标识撤销）"))),
            },
        };
        let mut roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机没有名册"))?;
        if roster.is_agent_revoked(&id) {
            return Err(err(ErrorCode::InvalidState, format!("agent {label} 已经撤销过了")));
        }
        roster.version += 1;
        roster.revoked_agents.push(RevokedAgent { agent: id, revoked_at: self.system.now() });
        let signed = roster.sign(&root);
        save(&self.system, KEY_ROSTER, &signed).map_err(internal)?;
        self.trust.borrow_mut().roster = Some(signed);
        self.roster_epoch.send_modify(|v| *v += 1);
        if let Some(i) = index {
            agents[i].revoked = true;
            save(&self.system, KEY_AGENTS, &agents).map_err(internal)?;
        }
        drop(_write);
        let (pushed, unreachable) = self.broadcast_roster(None).await;
        Ok(MembershipReport {
            name: label,
            node_id: NodeId(id.0),
            roster_version: self.roster_version(),
            pushed,
            unreachable,
        })
    }

    fn agent_list(&self) -> Result<Vec<AgentInfo>, WireError> {
        self.admin_key()?;
        self.load_agents()
    }

    /// 命令行请求在某个节点上执行命令：打开执行，然后在命令行和执行之间转发。
    async fn exec(&self, node: &str, spec: ExecSpec, agent: Option<AgentCredential>, server: CtlServer) {
        let device = match self.resolve_for(node, agent.as_ref()) {
            Ok(d) => d,
            Err(e) => {
                let _ = server.tx.send(CtlResponse::Error(e)).await;
                return;
            }
        };
        if device.node_id == self.me() {
            if let Err(e) = self.authorize_local(agent.as_ref(), &Request::Exec(spec.clone())) {
                let _ = server.tx.send(CtlResponse::Error(e)).await;
                return;
            }
            // 在本机执行：用内存管道连接两端，走和远端完全相同的代码。
            let (near, far) = tokio::io::duplex(256 * 1024);
            let client = async {
                let mut near = near;
                match read_response(&mut near).await {
                    Ok(Response::Ok) => self.pump_exec(near, server).await,
                    Ok(Response::Error(e)) => {
                        let _ = server.tx.send(CtlResponse::Error(e)).await;
                    }
                    other => {
                        let _ = server.tx.send(CtlResponse::Error(internal(format!("{other:?}")))).await;
                    }
                }
            };
            futures_util::future::join(self.serve_exec(far, spec), client).await;
            return;
        }
        for attempt in 0..2 {
            let failure = match self.open_stream(&device, Request::Exec(spec.clone()), agent.as_ref()).await {
                Ok(stream) => return self.pump_exec(stream, server).await,
                Err(OpenError::StaleRoster) if attempt == 0 => {
                    let _ = self.push_roster(device.node_id).await;
                    continue;
                }
                Err(OpenError::StaleRoster) => internal("对方的名册始终比本机旧"),
                Err(OpenError::Remote(e)) => e,
                Err(OpenError::Io(e)) => err(ErrorCode::Unreachable, format!("连不上 {}：{e}", device.name)),
            };
            let _ = server.tx.send(CtlResponse::Error(failure)).await;
            return;
        }
    }

    /// 向远端发出一个流式请求（执行、终端会话），对方接受后返回这条流。
    async fn open_stream(
        &self,
        to: &Device,
        body: Request,
        agent: Option<&AgentCredential>,
    ) -> Result<<T::Conn as Connection>::Stream, OpenError> {
        let principal = self.principal_for(agent, to, &body).map_err(OpenError::Remote)?;
        let priority = body.class().priority();
        let open = async {
            let conn = self.transport.connect(to.node_id, Proto::Rpc).await?;
            let mut stream = conn.open_bi().await?;
            conn.set_priority(&mut stream, priority);
            let header = RequestHeader::new(self.roster_version(), principal, &body);
            write_msg(&mut stream, &header).await?;
            let resp: Response = read_response(&mut stream).await?;
            Ok::<_, io::Error>((stream, resp))
        };
        let (stream, resp) = tokio::time::timeout(REQUEST_TIMEOUT, open)
            .await
            .map_err(|_| OpenError::Io(io::Error::new(io::ErrorKind::TimedOut, "超时")))?
            .map_err(OpenError::Io)?;
        match resp {
            Response::Ok => Ok(stream),
            Response::Error(e) if e.code == ErrorCode::StaleRoster => Err(OpenError::StaleRoster),
            Response::Error(e) => Err(OpenError::Remote(e)),
            other => Err(OpenError::Remote(internal(format!("对方的回应无法理解：{other:?}")))),
        }
    }

    /// 调用方一侧：把命令行的输入送到执行端，把执行事件送回命令行，直到命令结束。
    async fn pump_exec<St: AsyncRead + AsyncWrite + Unpin>(&self, stream: St, mut server: CtlServer) {
        let (rd, mut wr) = tokio::io::split(stream);
        let mut reader = FrameReader::new(rd);
        let mut input_open = true;
        loop {
            // 先转发输出、再发送输入：两边都在等对方读的时候不会互相卡住。
            tokio::select! {
                biased;
                event = reader.next::<ExecEvent>() => match event {
                    Ok(Some(event)) => {
                        let done = matches!(event, ExecEvent::Exit(_));
                        if server.tx.send(CtlResponse::Exec(event)).await.is_err() && input_open {
                            input_open = false;
                            let _ = write_msg(&mut wr, &Input::Cancel).await;
                        }
                        if done {
                            break;
                        }
                    }
                    Ok(None) | Err(_) => {
                        let lost = err(ErrorCode::Unreachable, "执行过程中连接中断，命令可能仍在运行或已被终止");
                        let _ = server.tx.send(CtlResponse::Error(lost)).await;
                        break;
                    }
                },
                input = server.rx.recv(), if input_open => {
                    // 命令行已经离开时，请对方终止命令，然后继续等它结束。
                    let input = input.unwrap_or(Input::Cancel);
                    input_open = !matches!(input, Input::Cancel);
                    if write_msg(&mut wr, &input).await.is_err() {
                        input_open = false;
                    }
                }
            }
        }
        let _ = wr.shutdown().await;
        reader.drain().await;
    }

    /// 执行端：启动进程，转发输入和输出，执行超时与输出上限，最后报告结果。
    async fn serve_exec<St: AsyncRead + AsyncWrite + Unpin>(&self, stream: St, spec: ExecSpec) {
        let (rd, mut wr) = tokio::io::split(stream);
        let mut reader = FrameReader::new(rd);
        let cmd = Cmd { program: spec.program.clone(), args: spec.args.clone(), cwd: spec.cwd.clone(), env: spec.env.clone() };
        let mut proc = match self.system.spawn(cmd) {
            Ok(p) => p,
            Err(e) => {
                let failure = err(ErrorCode::SpawnFailed, format!("无法启动 {}：{e}", spec.program));
                let _ = write_response(&mut wr, &Response::Error(failure)).await;
                let _ = wr.shutdown().await;
                reader.drain().await;
                return;
            }
        };
        // 保留写入游标：select! 取消写入时，已经发出的半帧不会被重发。
        // 每次只缓存一帧，保留输出反压；计时和取消不依赖接收方继续读取。
        let mut pending = Some((nexus_wire::encode(&nexus_wire::ResponseEnvelope::new(&Response::Ok)).expect("固定回应可编码"), 0));
        let started = self.system.now();
        let limit = spec.timeout_ms.map(Duration::from_millis);
        let deadline = tokio::time::sleep(limit.unwrap_or(Duration::from_secs(100 * 365 * 24 * 3600)));
        tokio::pin!(deadline);
        let mut sent: u64 = 0;
        let (mut timed_out, mut truncated, mut input_open, mut caller_gone, mut exited) = (false, false, true, false, false);
        loop {
            if exited && pending.is_none() {
                break;
            }
            tokio::select! {
                biased;
                _ = &mut deadline, if limit.is_some() && !timed_out && !exited => {
                    timed_out = true;
                    proc.kill();
                }
                input = reader.next::<Input>(), if input_open && !exited => match input {
                    Ok(Some(Input::Stdin(bytes))) => proc.write_stdin(bytes),
                    Ok(Some(Input::CloseStdin)) => proc.close_stdin(),
                    Ok(None) => {
                        input_open = false;
                        proc.close_stdin();
                    }
                    Ok(Some(Input::Resize(_) | Input::Ack(_) | Input::Ping)) => {}
                    Ok(Some(Input::Cancel | Input::Detach)) | Err(_) => {
                        input_open = false;
                        caller_gone = true;
                        pending = None;
                        proc.kill();
                    }
                },
                result = async { write_pending(&mut wr, pending.as_mut().expect("有待发送帧")).await }, if pending.is_some() => {
                    pending = None;
                    if result.is_err() {
                        caller_gone = true;
                        proc.kill();
                    }
                }
                event = proc.next_event(), if pending.is_none() && !exited => match event {
                    ProcEvent::Exited(status) => {
                        exited = true;
                        let result = ExecResult {
                            exit_code: status.code,
                            signal: status.signal,
                            timed_out,
                            truncated,
                            duration_ms: self.system.now().millis_since(started),
                        };
                        if !caller_gone {
                            pending = Some((nexus_wire::encode(&ExecEvent::Exit(result)).expect("执行结果可编码"), 0));
                        }
                    }
                    ProcEvent::Stdout(bytes) | ProcEvent::Stderr(bytes) if truncated || caller_gone => drop(bytes),
                    event => {
                        let (is_err, mut bytes) = match event {
                            ProcEvent::Stdout(b) => (false, b),
                            ProcEvent::Stderr(b) => (true, b),
                            ProcEvent::Exited(_) => unreachable!(),
                        };
                        let room = spec.max_output.saturating_sub(sent);
                        if bytes.len() as u64 > room {
                            bytes.truncate(room as usize);
                            truncated = true;
                            proc.kill();
                        }
                        if !bytes.is_empty() {
                            sent += bytes.len() as u64;
                            let event = if is_err { ExecEvent::Stderr(bytes) } else { ExecEvent::Stdout(bytes) };
                            match nexus_wire::encode(&event) {
                                Ok(frame) => pending = Some((frame, 0)),
                                Err(_) => {
                                    caller_gone = true;
                                    proc.kill();
                                }
                            }
                        }
                    }
                },
            }
        }
        let _ = wr.shutdown().await;
        reader.drain().await;
    }

    // ------------------------------------------------------------ 终端会话

    /// 命令行请求连上某个节点上的会话：打开会话流，然后在命令行和会话之间转发。
    async fn attach(&self, node: &str, name: String, size: Size, create: bool, agent: Option<AgentCredential>, server: CtlServer) {
        let device = match self.resolve_for(node, agent.as_ref()) {
            Ok(d) => d,
            Err(e) => {
                let _ = server.tx.send(CtlResponse::Error(e)).await;
                return;
            }
        };
        if device.node_id == self.me() {
            let body = Request::SessionAttach { name: name.clone(), create, size };
            // agent 的会话要一直接受复查：授权通过后为它建立守卫。
            let check = required_action(&body, &device.name).and_then(|action| {
                self.authorize_local(agent.as_ref(), &body)?;
                let Some(a) = agent.as_ref() else { return Ok(None) };
                let action = action.ok_or_else(|| internal("会话请求应当需要授权"))?;
                let principal = self.principal_for(Some(a), &device, &body)?;
                Ok(Some(Box::new(SessionGuard::new(self.me(), principal, action))))
            });
            let guard = match check {
                Ok(guard) => guard,
                Err(e) => {
                    let _ = server.tx.send(CtlResponse::Error(e)).await;
                    return;
                }
            };
            let (near, far) = tokio::io::duplex(256 * 1024);
            let client = async {
                let mut near = near;
                match read_response(&mut near).await {
                    Ok(Response::Ok) => self.pump_session(near, server).await,
                    Ok(Response::Error(e)) => {
                        let _ = server.tx.send(CtlResponse::Error(e)).await;
                    }
                    other => {
                        let _ = server.tx.send(CtlResponse::Error(internal(format!("{other:?}")))).await;
                    }
                }
            };
            futures_util::future::join(self.serve_session(far, &name, create, size, guard), client).await;
            return;
        }
        let body = Request::SessionAttach { name, create, size };
        for attempt in 0..2 {
            let failure = match self.open_stream(&device, body.clone(), agent.as_ref()).await {
                Ok(stream) => return self.pump_session(stream, server).await,
                Err(OpenError::StaleRoster) if attempt == 0 => {
                    let _ = self.push_roster(device.node_id).await;
                    continue;
                }
                Err(OpenError::StaleRoster) => internal("对方的名册始终比本机旧"),
                Err(OpenError::Remote(e)) => e,
                Err(OpenError::Io(e)) => err(ErrorCode::Unreachable, format!("连不上 {}：{e}", device.name)),
            };
            let _ = server.tx.send(CtlResponse::Error(failure)).await;
            return;
        }
    }

    /// 客户端一侧：把命令行的输入、尺寸变化和确认送到会话，把会话的输出送回命令行。
    /// 连接中断时告诉命令行，由它决定是否重连；命令行离开时告诉会话。
    async fn pump_session<St: AsyncRead + AsyncWrite + Unpin>(&self, stream: St, mut server: CtlServer) {
        let (rd, mut wr) = tokio::io::split(stream);
        let mut reader = FrameReader::new(rd);
        let mut input_open = true;
        let mut last_heard = tokio::time::Instant::now();
        let mut next_ping = last_heard + HEARTBEAT;
        let lost = || err(ErrorCode::Unreachable, "与会话的连接中断");
        // 连接可能已经失效而传输层还没发现：写入卡住和长时间收不到心跳都当作中断。
        loop {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(last_heard + PEER_SILENT) => {
                    let _ = server.tx.send(CtlResponse::Error(lost())).await;
                    break;
                }
                _ = tokio::time::sleep_until(next_ping), if input_open => {
                    next_ping = tokio::time::Instant::now() + HEARTBEAT;
                    if !send_within(&mut wr, &Input::Ping).await {
                        let _ = server.tx.send(CtlResponse::Error(lost())).await;
                        break;
                    }
                }
                frame = reader.next::<SessionFrame>() => match frame {
                    Ok(Some(frame)) => {
                        last_heard = tokio::time::Instant::now();
                        let SessionFrame::Msg(msg) = frame else { continue };
                        let done = matches!(msg, ClientMsg::Exit(_));
                        if server.tx.send(CtlResponse::Session(msg)).await.is_err() && input_open {
                            input_open = false;
                            let _ = send_within(&mut wr, &Input::Detach).await;
                        }
                        if done {
                            break;
                        }
                    }
                    Ok(None) | Err(_) => {
                        let _ = server.tx.send(CtlResponse::Error(lost())).await;
                        break;
                    }
                },
                input = server.rx.recv(), if input_open => {
                    let input = input.unwrap_or(Input::Detach);
                    let leaving = matches!(input, Input::Detach | Input::Cancel);
                    if !send_within(&mut wr, &input).await {
                        if !leaving {
                            let _ = server.tx.send(CtlResponse::Error(lost())).await;
                        }
                        break;
                    }
                    if leaving {
                        break;
                    }
                }
            }
        }
        let _ = tokio::time::timeout(PEER_SILENT, wr.shutdown()).await;
        let _ = tokio::time::timeout(PEER_SILENT, reader.drain()).await;
    }

    /// 会话一侧：连上（必要时创建）会话，转发这个客户端的输入和会话给它的输出，直到它离开或会话结束。
    async fn serve_session<St: AsyncRead + AsyncWrite + Unpin>(
        &self,
        stream: St,
        name: &str,
        create: bool,
        size: Size,
        guard: Option<Box<SessionGuard>>,
    ) {
        let mut epoch = self.roster_epoch.subscribe();
        let expires = guard.as_ref().and_then(|g| g.not_after).map(|not_after| {
            let left = not_after.0.saturating_sub(self.system.now().0);
            tokio::time::Instant::now() + Duration::from_millis(left)
        });
        let (rd, mut wr) = tokio::io::split(stream);
        let mut reader = FrameReader::new(rd);
        let cmd = match self.open_session(name, create, size) {
            Ok(cmd) => cmd,
            Err(e) => {
                let _ = write_response(&mut wr, &Response::Error(e)).await;
                let _ = wr.shutdown().await;
                reader.drain().await;
                return;
            }
        };
        let client = self.next_client.get();
        self.next_client.set(client + 1);
        let (tx, mut out) = mpsc::unbounded_channel();
        if cmd.send(SessCmd::Attach { client, size, tx }).is_err() || write_response(&mut wr, &Response::Ok).await.is_err() {
            let _ = cmd.send(SessCmd::Event(TermEvent::Detach { client }));
            return;
        }
        // 写入卡住和长时间收不到客户端的任何消息，都说明客户端已经失联：让它离开会话。
        let mut last_heard = tokio::time::Instant::now();
        let mut next_beat = last_heard + HEARTBEAT;
        loop {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(last_heard + PEER_SILENT) => break,
                // 凭据到期：断开。
                _ = tokio::time::sleep_until(expires.unwrap_or(last_heard)), if expires.is_some() => break,
                // 名册变了：设备、agent 或通行密钥的权限可能已收回。
                Ok(()) = epoch.changed(), if guard.is_some() => {
                    let g = guard.as_ref().expect("有授权守卫");
                    let Some(roster) = self.roster() else { break };
                    if !g.allowed(&roster, self.me(), self.system.now()) {
                        break;
                    }
                }
                _ = tokio::time::sleep_until(next_beat) => {
                    next_beat = tokio::time::Instant::now() + HEARTBEAT;
                    if !send_within(&mut wr, &SessionFrame::Heartbeat).await {
                        break;
                    }
                }
                msg = out.recv() => match msg {
                    Some(msg) => {
                        let done = matches!(msg, ClientMsg::Exit(_));
                        if !send_within(&mut wr, &SessionFrame::Msg(msg)).await || done {
                            break;
                        }
                    }
                    None => break,
                },
                input = reader.next::<Input>() => {
                    last_heard = tokio::time::Instant::now();
                    let event = match input {
                        Ok(Some(Input::Stdin(bytes))) => TermEvent::Input { client, bytes },
                        Ok(Some(Input::Resize(size))) => TermEvent::Resize { client, size },
                        Ok(Some(Input::Ack(bytes))) => TermEvent::Ack { client, bytes },
                        Ok(Some(Input::CloseStdin | Input::Ping)) => continue,
                        Ok(Some(Input::Detach | Input::Cancel)) | Ok(None) | Err(_) => break,
                    };
                    if cmd.send(SessCmd::Event(event)).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = cmd.send(SessCmd::Event(TermEvent::Detach { client }));
        let _ = tokio::time::timeout(PEER_SILENT, wr.shutdown()).await;
        let _ = tokio::time::timeout(PEER_SILENT, reader.drain()).await;
    }

    /// 找到名为 `name` 的会话；不存在且 `create` 时启动一个新的。
    fn open_session(&self, name: &str, create: bool, size: Size) -> Result<mpsc::UnboundedSender<SessCmd>, WireError> {
        if let Some(h) = self.sessions.borrow().get(name) {
            return Ok(h.cmd.clone());
        }
        if !create {
            return Err(err(ErrorCode::NotFound, format!("没有会话 {name}")));
        }
        if !valid_segment(name) {
            return Err(err(ErrorCode::InvalidInput, format!("会话名 {name:?} 不合法：只能用字母、数字、- 和 _")));
        }
        let size = size.clamped();
        let created_at = self.system.now();
        let pty = self.system.spawn_pty(name, size, created_at).map_err(|e| err(ErrorCode::SpawnFailed, format!("无法启动终端：{e}")))?;
        self.start_session(name, pty, size, created_at)
    }

    /// 把一个终端交给会话任务，登记到会话列表里。
    fn start_session(&self, name: &str, pty: S::Pty, size: Size, created_at: Timestamp) -> Result<mpsc::UnboundedSender<SessCmd>, WireError> {
        let (cmd, cmds) = mpsc::unbounded_channel();
        let clients = Rc::new(Cell::new(0));
        let current_size = Rc::new(Cell::new(size));
        self.sessions.borrow_mut().insert(
            name.to_string(),
            SessionHandle { cmd: cmd.clone(), created_at, clients: clients.clone(), size: current_size.clone() },
        );
        let new = NewSession { name: name.to_string(), pty, size, cmds, clients, current_size };
        if self.spawn_tx.send(new).is_err() {
            self.sessions.borrow_mut().remove(name);
            return Err(internal("节点正在关闭"));
        }
        Ok(cmd)
    }

    /// 常驻程序启动时接管上一次运行留下的终端会话（M7.2）。
    async fn adopt_sessions(&self) {
        for a in self.system.adopt_ptys().await {
            if self.sessions.borrow().contains_key(&a.name) || !valid_segment(&a.name) {
                continue;
            }
            let _ = self.start_session(&a.name, a.pty, a.size.clamped(), a.created_at);
        }
    }

    /// 驱动一个会话：终端程序的输出交给状态机，状态机的动作发给客户端和终端。
    async fn run_session(&self, new: NewSession<S::Pty>) {
        let NewSession { name, mut pty, size, mut cmds, clients, current_size } = new;
        let mut live = LiveSession {
            term: TermSession::new(size, DEFAULT_WINDOW),
            clients: BTreeMap::new(),
            client_count: clients,
            current_size,
        };
        loop {
            // 先处理客户端的命令（输入、确认都很小），再处理终端输出：
            // 终端不停输出时，确认也能及时生效。
            tokio::select! {
                biased;
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => live.command(&mut pty, cmd),
                    None => pty.kill(),
                },
                event = pty.next_event() => match event {
                    PtyEvent::Output(bytes) => live.apply(&mut pty, TermEvent::PtyOutput(bytes)),
                    PtyEvent::Exited(status) => {
                        live.apply(&mut pty, TermEvent::PtyExited { code: status.code });
                        break;
                    }
                },
            }
        }
        // 程序已经结束：不再接受新的客户端，等落后的客户端拿到最终屏幕和退出码。
        self.sessions.borrow_mut().remove(&name);
        let linger = tokio::time::sleep(SESSION_LINGER);
        tokio::pin!(linger);
        while !live.clients.is_empty() {
            tokio::select! {
                biased;
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => live.command(&mut pty, cmd),
                    None => break,
                },
                _ = &mut linger => break,
            }
        }
    }

    /// 会话列表；给 agent 和网页登录时只包含它能看到的会话。
    fn session_list(&self, sees: impl Fn(&ResourcePath) -> bool) -> Vec<SessionInfo> {
        let me = self.name();
        self.sessions
            .borrow()
            .iter()
            .filter(|(name, _)| ResourcePath::parse(&format!("{me}/session/{name}")).is_ok_and(|p| sees(&p)))
            .map(|(name, h)| SessionInfo { name: name.clone(), clients: h.clients.get(), created_at: h.created_at, size: h.size.get() })
            .collect()
    }

    fn session_kill(&self, name: &str) -> Result<(), WireError> {
        match self.sessions.borrow().get(name) {
            Some(h) => {
                let _ = h.cmd.send(SessCmd::Kill);
                Ok(())
            }
            None => Err(err(ErrorCode::NotFound, format!("没有会话 {name}"))),
        }
    }

    /// 对一个节点发出一次普通请求，限时完成。
    async fn call_node(&self, device: &Device, body: Request, agent: Option<&AgentCredential>) -> Result<Response, WireError> {
        match tokio::time::timeout(REQUEST_TIMEOUT, self.request(device, body, agent)).await {
            Ok(Ok((Response::Error(e), _))) => Err(e),
            Ok(Ok((resp, _))) => Ok(resp),
            Ok(Err(e)) => Err(err(ErrorCode::Unreachable, format!("连不上 {}：{e}", device.name))),
            Err(_) => Err(err(ErrorCode::Unreachable, format!("连接 {} 超时", device.name))),
        }
    }

    async fn remote_session_list(&self, node: &str, agent: Option<&AgentCredential>) -> Result<Vec<SessionInfo>, WireError> {
        let device = self.resolve_for(node, agent)?;
        if device.node_id == self.me() {
            self.authorize_local(agent, &Request::SessionList)?;
            return Ok(self.session_list(|p| agent.is_none_or(|a| a.cert.can_see(p))));
        }
        match self.call_node(&device, Request::SessionList, agent).await? {
            Response::Sessions(list) => Ok(list),
            other => Err(internal(format!("对方的回应无法理解：{other:?}"))),
        }
    }

    // ------------------------------------------------------------ 发布服务（M3.1）

    fn publish(&self, name: String, addr: std::net::SocketAddr, proto: String, env: Vec<(String, String)>) -> Result<String, WireError> {
        let me = self.my_name()?;
        if !valid_segment(&name) {
            return Err(err(ErrorCode::InvalidInput, format!("服务名 {name:?} 不合法：只能用字母、数字、- 和 _")));
        }
        if !SERVICE_PROTOS.contains(&proto.as_str()) {
            return Err(err(ErrorCode::InvalidInput, format!("不认识的协议 {proto}：可用 {}", SERVICE_PROTOS.join("、"))));
        }
        for (k, _) in &env {
            let ok = !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') && !k.as_bytes()[0].is_ascii_digit();
            if !ok {
                return Err(err(ErrorCode::InvalidInput, format!("环境变量名 {k:?} 不合法")));
            }
        }
        let mut services = self.services.borrow_mut();
        if services.iter().any(|s| s.name == name) {
            return Err(err(ErrorCode::InvalidInput, format!("服务 {name} 已经发布了")));
        }
        services.push(PublishedService { name: name.clone(), addr, proto, env });
        save(&self.system, KEY_SERVICES, &*services).map_err(internal)?;
        Ok(format!("{me}/{name}"))
    }

    fn unpublish(&self, name: &str) -> Result<(), WireError> {
        let mut services = self.services.borrow_mut();
        let before = services.len();
        services.retain(|s| s.name != name);
        if services.len() == before {
            return Err(err(ErrorCode::NotFound, format!("没有发布服务 {name}")));
        }
        save(&self.system, KEY_SERVICES, &*services).map_err(internal)
    }

    fn my_name(&self) -> Result<String, WireError> {
        let roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机还没有加入任何网络"))?;
        roster.device(self.me()).map(|d| d.name.clone()).ok_or_else(|| internal("本机不在名册里"))
    }

    /// 本机发布的服务中，`may` 允许连接的那些。
    fn service_list(&self, me: &str, may: impl Fn(&Action) -> bool) -> Vec<ServiceInfo> {
        self.services
            .borrow()
            .iter()
            .filter(|s| {
                required_action(&Request::SvcConnect { name: s.name.clone() }, me).ok().flatten().is_some_and(|a| may(&a))
            })
            .map(PublishedService::info)
            .collect()
    }

    fn service_addr(&self, name: &str) -> Option<std::net::SocketAddr> {
        self.services.borrow().iter().find(|s| s.name == name).map(|s| s.addr)
    }

    async fn remote_services(&self, node: &str, agent: Option<&AgentCredential>) -> Result<(String, Vec<ServiceInfo>), WireError> {
        let device = self.resolve_for(node, agent)?;
        if device.node_id == self.me() {
            self.authorize_local(agent, &Request::SvcList)?;
            let list = self.service_list(&device.name, |a| agent.is_none_or(|ag| ag.cert.cert.grants.iter().any(|g| g.allows(a))));
            return Ok((device.name, list));
        }
        match self.call_node(&device, Request::SvcList, agent).await? {
            Response::Services(list) => Ok((device.name, list)),
            other => Err(internal(format!("对方的回应无法理解：{other:?}"))),
        }
    }

    /// 服务一侧：接到本机服务，然后双向转发，直到两个方向都结束。
    async fn serve_pipe<St: AsyncRead + AsyncWrite + Unpin>(&self, mut stream: St, addr: std::net::SocketAddr, name: &str) {
        let mut local = match self.system.tcp_connect(addr).await {
            Ok(c) => c,
            Err(e) => {
                let failure = err(ErrorCode::Unreachable, format!("服务 {name} 的本机程序没有在运行（{e}）"));
                let _ = write_response(&mut stream, &Response::Error(failure)).await;
                let _ = stream.shutdown().await;
                drain(&mut stream).await;
                return;
            }
        };
        if write_response(&mut stream, &Response::Ok).await.is_err() {
            return;
        }
        let _ = tokio::io::copy_bidirectional(&mut stream, &mut local).await;
    }

    /// 出口：替别的设备连到目标。不连本机地址：本机上的程序要给别人用，应当发布成服务。
    async fn serve_exit<St: AsyncRead + AsyncWrite + Unpin>(&self, mut stream: St, target: Target) {
        let result = async {
            let ips = match &target.host {
                Host::Ip(ip) => vec![*ip],
                Host::Name(name) => {
                    self.system.resolve(name).await.map_err(|e| err(ErrorCode::Unreachable, format!("出口解析不了 {name}：{e}")))?
                }
            };
            let ips: Vec<_> = ips.into_iter().filter(|ip| !is_own_address(ip)).collect();
            if ips.is_empty() {
                return Err(err(ErrorCode::Denied, format!("出口不连本机地址：{target}")));
            }
            let mut last = String::new();
            for ip in ips {
                match self.system.tcp_connect(std::net::SocketAddr::new(ip, target.port)).await {
                    Ok(c) => return Ok(c),
                    Err(e) => last = format!("出口连不上 {target}：{e}"),
                }
            }
            Err(err(ErrorCode::Unreachable, last))
        };
        let mut out = match result.await {
            Ok(c) => c,
            Err(failure) => {
                let _ = write_response(&mut stream, &Response::Error(failure)).await;
                let _ = stream.shutdown().await;
                drain(&mut stream).await;
                return;
            }
        };
        if write_response(&mut stream, &Response::Ok).await.is_err() {
            return;
        }
        let _ = tokio::io::copy_bidirectional(&mut stream, &mut out).await;
    }

    /// `nexus connect`：调用方一侧把命令行的输入送到服务，把服务的输出送回命令行。
    async fn connect(&self, node: &str, name: &str, agent: Option<AgentCredential>, server: CtlServer) {
        let body = Request::SvcConnect { name: name.to_string() };
        let device = match self.resolve_for(node, agent.as_ref()) {
            Ok(d) => d,
            Err(e) => {
                let _ = server.tx.send(CtlResponse::Error(e)).await;
                return;
            }
        };
        if device.node_id == self.me() {
            let check = required_action(&body, &device.name).and_then(|_| self.authorize_local(agent.as_ref(), &body));
            let addr = check.and_then(|()| self.service_addr(name).ok_or_else(|| err(ErrorCode::NotFound, format!("本机没有发布服务 {name}"))));
            let addr = match addr {
                Ok(a) => a,
                Err(e) => {
                    let _ = server.tx.send(CtlResponse::Error(e)).await;
                    return;
                }
            };
            // 在本机：用内存管道连接两端，走和远端完全相同的代码。
            let (near, far) = tokio::io::duplex(256 * 1024);
            let client = async {
                let mut near = near;
                match read_response(&mut near).await {
                    Ok(Response::Ok) => self.pump_pipe(near, server).await,
                    Ok(Response::Error(e)) => {
                        let _ = server.tx.send(CtlResponse::Error(e)).await;
                    }
                    other => {
                        let _ = server.tx.send(CtlResponse::Error(internal(format!("{other:?}")))).await;
                    }
                }
            };
            futures_util::future::join(self.serve_pipe(far, addr, name), client).await;
            return;
        }
        for attempt in 0..2 {
            let failure = match self.open_stream(&device, body.clone(), agent.as_ref()).await {
                Ok(stream) => return self.pump_pipe(stream, server).await,
                Err(OpenError::StaleRoster) if attempt == 0 => {
                    let _ = self.push_roster(device.node_id).await;
                    continue;
                }
                Err(OpenError::StaleRoster) => internal("对方的名册始终比本机旧"),
                Err(OpenError::Remote(e)) => e,
                Err(OpenError::Io(e)) => err(ErrorCode::Unreachable, format!("连不上 {}：{e}", device.name)),
            };
            let _ = server.tx.send(CtlResponse::Error(failure)).await;
            return;
        }
    }

    async fn pump_pipe<St: AsyncRead + AsyncWrite + Unpin>(&self, stream: St, mut server: CtlServer) {
        if server.tx.send(CtlResponse::Ok).await.is_err() {
            return;
        }
        let (mut rd, mut wr) = tokio::io::split(stream);
        let mut buf = vec![0u8; 64 * 1024];
        let (mut reading, mut writing) = (true, true);
        while reading || writing {
            tokio::select! {
                n = rd.read(&mut buf), if reading => match n {
                    Ok(0) | Err(_) => {
                        reading = false;
                        let _ = server.tx.send(CtlResponse::PipeEnd).await;
                    }
                    Ok(n) => {
                        if server.tx.send(CtlResponse::PipeData(buf[..n].to_vec())).await.is_err() {
                            break;
                        }
                    }
                },
                input = server.rx.recv(), if writing => match input {
                    Some(Input::Stdin(b)) => {
                        if wr.write_all(&b).await.is_err() {
                            writing = false;
                        }
                    }
                    Some(_) | None => {
                        writing = false;
                        let _ = wr.shutdown().await;
                    }
                },
            }
        }
    }

    // ------------------------------------------------------------ 统一入口与路由（M3.2）

    fn route_add(&self, rule: Rule) -> Result<u32, WireError> {
        match &rule.outbound {
            Outbound::Exit(device) => {
                if self.resolve(device)?.node_id == self.me() {
                    return Err(err(ErrorCode::InvalidInput, format!("{device} 就是本机：本机出去用 --direct")));
                }
            }
            Outbound::Socks5(upstream) => {
                if Target::parse(upstream).is_none() {
                    return Err(err(ErrorCode::InvalidInput, format!("上游代理应当写成 主机:端口：{upstream}")));
                }
            }
            Outbound::Direct | Outbound::Reject => {}
        }
        let mut routes = self.routes.borrow_mut();
        routes.rules.push(rule);
        save(&self.system, KEY_ROUTES, &*routes).map_err(internal)?;
        Ok(routes.rules.len() as u32)
    }

    fn route_rm(&self, index: u32) -> Result<(), WireError> {
        let mut routes = self.routes.borrow_mut();
        if index == 0 || index as usize > routes.rules.len() {
            return Err(err(ErrorCode::NotFound, format!("没有第 {index} 条规则")));
        }
        routes.rules.remove(index as usize - 1);
        save(&self.system, KEY_ROUTES, &*routes).map_err(internal)
    }

    fn route_explain(&self, target: &str) -> Result<RouteExplanation, WireError> {
        let t = Target::parse(target).ok_or_else(|| err(ErrorCode::InvalidInput, format!("目标应当写成 主机:端口：{target}")))?;
        let rules = self.routes.borrow().rules.clone();
        Ok(match route::decide(&t, &rules) {
            Decision::Service { device, service } => RouteExplanation::Service { device, service },
            Decision::BadInternalName => RouteExplanation::BadInternalName,
            Decision::Rule { index, .. } => RouteExplanation::Rule { index: index as u32 + 1, rule: rules[index].clone() },
            Decision::Default(outbound) => RouteExplanation::Default { outbound },
        })
    }

    /// 名字服务：服务的名字和规则里不直连的域名给本机临时地址，其余照常解析。
    async fn answer_dns(&self, raw: &[u8]) -> Vec<u8> {
        let Some(q) = dns::parse_query(raw) else { return Vec::new() };
        if self.tun.borrow().is_none() {
            return dns::error(&q, dns::SERVFAIL);
        }
        let target = Target { host: Host::Name(q.name.clone()), port: 0 };
        let capture = match route::decide(&target, &self.routes.borrow().rules) {
            Decision::Service { device, .. } => {
                if self.resolve(&device).is_err() {
                    return dns::error(&q, dns::NXDOMAIN);
                }
                true
            }
            Decision::BadInternalName => return dns::error(&q, dns::NXDOMAIN),
            Decision::Rule { outbound, .. } | Decision::Default(outbound) => outbound != Outbound::Direct,
        };
        if capture {
            let ip = match self.tun.borrow_mut().as_mut() {
                Some((_, pool)) => pool.assign(&q.name),
                None => return dns::error(&q, dns::SERVFAIL),
            };
            return dns::answer(&q, &[std::net::IpAddr::V4(ip)], DNS_TTL);
        }
        if q.qtype != dns::A && q.qtype != dns::AAAA {
            return dns::answer(&q, &[], DNS_TTL);
        }
        match self.system.resolve(&q.name).await {
            Ok(ips) => dns::answer(&q, &ips, DNS_TTL),
            Err(e) if e.kind() == io::ErrorKind::NotFound => dns::error(&q, dns::NXDOMAIN),
            Err(_) => dns::error(&q, dns::SERVFAIL),
        }
    }

    /// 虚拟网卡上的一条 TCP 连接：按地址找回名字，然后和统一入口一样按路由接到出口。
    async fn serve_tun(&self, mut client: ProxyStream, dst: std::net::SocketAddr) {
        let name = match (dst.ip(), self.tun.borrow().as_ref()) {
            (std::net::IpAddr::V4(ip), Some((_, pool))) => pool.lookup(ip).map(str::to_string),
            _ => None,
        };
        let Some(name) = name else {
            let _ = client.shutdown().await;
            return;
        };
        let target = Target { host: Host::Name(name), port: dst.port() };
        match self.open_outbound(&target).await {
            Ok(up) => {
                let _ = up.splice(client, Vec::new()).await;
            }
            Err(_) => {
                let _ = client.shutdown().await;
            }
        }
    }

    /// 入口上的一条连接：按第一个字节分辨 SOCKS5 和 HTTP 代理，读出目标，按路由接到出口，然后双向转发。
    async fn serve_proxy(&self, mut client: ProxyStream) {
        let mut buf = Vec::new();
        let entry = match tokio::time::timeout(PROXY_HANDSHAKE_TIMEOUT, fill(&mut client, &mut buf)).await {
            Ok(true) => sniff(buf[0]),
            _ => None,
        };
        let _ = match entry {
            Some(Entry::Socks5) => self.serve_socks5(client, buf).await,
            Some(Entry::Http) => self.serve_http_proxy(client, buf).await,
            None => {
                let _ = client.shutdown().await;
                Ok(())
            }
        };
    }

    async fn serve_socks5(&self, mut client: ProxyStream, mut buf: Vec<u8>) -> io::Result<()> {
        let handshake = async {
            let (used, no_auth) = loop {
                match socks5::parse_greeting(&buf) {
                    Ok(Some(g)) => break g,
                    Ok(None) if fill(&mut client, &mut buf).await => {}
                    _ => return Ok(None),
                }
            };
            buf.drain(..used);
            if !no_auth {
                client.write_all(&socks5::NO_ACCEPTABLE).await?;
                return Ok(None);
            }
            client.write_all(&socks5::NO_AUTH).await?;
            loop {
                match socks5::parse_request(&buf) {
                    Ok(Some((used, target))) => {
                        buf.drain(..used);
                        return Ok(Some(target));
                    }
                    Ok(None) if fill(&mut client, &mut buf).await => {}
                    Ok(None) => return Ok(None),
                    Err(reply) => {
                        client.write_all(&socks5::reply(reply)).await?;
                        return Ok(None);
                    }
                }
            }
        };
        let target = match tokio::time::timeout(PROXY_HANDSHAKE_TIMEOUT, handshake).await {
            Ok(Ok(Some(t))) => t,
            Ok(Err(e)) => return Err(e),
            _ => return client.shutdown().await,
        };
        match self.open_outbound(&target).await {
            Err(fail) => {
                let reply = match fail {
                    ProxyFail::Rejected | ProxyFail::Denied(_) => socks5::Reply::NotAllowed,
                    ProxyFail::Unreachable(_) => socks5::Reply::HostUnreachable,
                    ProxyFail::Refused(_) => socks5::Reply::ConnectionRefused,
                };
                client.write_all(&socks5::reply(reply)).await?;
                client.shutdown().await
            }
            Ok(up) => {
                client.write_all(&socks5::reply(socks5::Reply::Succeeded)).await?;
                up.splice(client, buf).await
            }
        }
    }

    async fn serve_http_proxy(&self, mut client: ProxyStream, mut buf: Vec<u8>) -> io::Result<()> {
        let read_head = async {
            loop {
                if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    return Some(end + 4);
                }
                if buf.len() > PROXY_MAX_HEAD || !fill(&mut client, &mut buf).await {
                    return None;
                }
            }
        };
        let Ok(Some(end)) = tokio::time::timeout(PROXY_HANDSHAKE_TIMEOUT, read_head).await else {
            return client.shutdown().await;
        };
        let (target, first, connect) = match http::parse(&buf) {
            Err(BadRequest(why)) => {
                client.write_all(&http::response(400, "Bad Request", why)).await?;
                return client.shutdown().await;
            }
            Ok(ProxyRequest::Connect(t)) => (t, buf[end..].to_vec(), true),
            Ok(ProxyRequest::Forward { target, head }) => (target, head, false),
        };
        match self.open_outbound(&target).await {
            Err(fail) => {
                let (status, reason, why) = match fail {
                    ProxyFail::Rejected => (403, "Forbidden", format!("路由规则不允许访问 {target}")),
                    ProxyFail::Denied(why) => (403, "Forbidden", why),
                    ProxyFail::Unreachable(why) | ProxyFail::Refused(why) => (502, "Bad Gateway", why),
                };
                client.write_all(&http::response(status, reason, &why)).await?;
                client.shutdown().await
            }
            Ok(up) => {
                if connect {
                    client.write_all(http::CONNECT_OK).await?;
                }
                up.splice(client, first).await
            }
        }
    }

    /// 按路由接到出口。
    async fn open_outbound(&self, target: &Target) -> Result<Upstream<S::Tcp, <T::Conn as Connection>::Stream>, ProxyFail> {
        let decision = route::decide(target, &self.routes.borrow().rules);
        let outbound = match decision {
            Decision::Service { device, service } => return self.open_service(&device, &service).await,
            Decision::BadInternalName => {
                return Err(ProxyFail::Unreachable(format!("{target} 不是服务的名字：应当是 <服务>.<设备>.{SERVICE_SUFFIX}")))
            }
            Decision::Rule { outbound, .. } | Decision::Default(outbound) => outbound,
        };
        match outbound {
            Outbound::Reject => Err(ProxyFail::Rejected),
            Outbound::Direct => {
                let ips = match &target.host {
                    Host::Ip(ip) => vec![*ip],
                    Host::Name(name) => self.system.resolve(name).await.map_err(|e| ProxyFail::Unreachable(format!("解析不了 {name}：{e}")))?,
                };
                let mut last = format!("{target} 没有地址");
                for ip in ips {
                    match self.system.tcp_connect(std::net::SocketAddr::new(ip, target.port)).await {
                        Ok(c) => return Ok(Upstream::Tcp(c)),
                        Err(e) => last = format!("连不上 {target}：{e}"),
                    }
                }
                Err(ProxyFail::Refused(last))
            }
            Outbound::Exit(device) => self.open_exit(&device, target).await,
            Outbound::Socks5(upstream) => self.open_upstream(&upstream, target).await,
        }
    }

    /// 经网络里的一台设备出去：对方判定授权、审计，再连到目标。
    async fn open_exit(&self, device: &str, target: &Target) -> Result<Upstream<S::Tcp, <T::Conn as Connection>::Stream>, ProxyFail> {
        let device = self.resolve(device).map_err(|e| ProxyFail::Unreachable(e.message))?;
        let body = Request::ExitConnect { target: target.to_string() };
        for attempt in 0..2 {
            match self.open_stream(&device, body.clone(), None).await {
                Ok(stream) => return Ok(Upstream::Remote(stream)),
                Err(OpenError::StaleRoster) if attempt == 0 => {
                    let _ = self.push_roster(device.node_id).await;
                }
                Err(OpenError::StaleRoster) => break,
                Err(OpenError::Remote(e)) if e.code == ErrorCode::Denied => return Err(ProxyFail::Denied(e.message)),
                Err(OpenError::Remote(e)) => return Err(ProxyFail::Unreachable(e.message)),
                Err(OpenError::Io(e)) => return Err(ProxyFail::Unreachable(format!("连不上出口 {}：{e}", device.name))),
            }
        }
        Err(ProxyFail::Unreachable("对方的名册始终比本机旧".into()))
    }

    /// 交给上游 SOCKS5 代理：目标原样交过去，不在本机解析。
    async fn open_upstream(&self, upstream: &str, target: &Target) -> Result<Upstream<S::Tcp, <T::Conn as Connection>::Stream>, ProxyFail> {
        let up = Target::parse(upstream).ok_or_else(|| ProxyFail::Unreachable(format!("上游代理的地址不对：{upstream}")))?;
        let ips = match &up.host {
            Host::Ip(ip) => vec![*ip],
            Host::Name(name) => self.system.resolve(name).await.map_err(|e| ProxyFail::Unreachable(format!("解析不了上游代理 {name}：{e}")))?,
        };
        let mut conn = None;
        for ip in ips {
            if let Ok(c) = self.system.tcp_connect(std::net::SocketAddr::new(ip, up.port)).await {
                conn = Some(c);
                break;
            }
        }
        let mut conn = conn.ok_or_else(|| ProxyFail::Unreachable(format!("上游代理 {upstream} 没有在运行")))?;
        let handshake = async {
            conn.write_all(&[5, 1, 0]).await?;
            let mut greet = [0u8; 2];
            conn.read_exact(&mut greet).await?;
            if greet != socks5::NO_AUTH {
                return Ok(Err(ProxyFail::Unreachable(format!("上游代理 {upstream} 要求认证（还不支持）"))));
            }
            conn.write_all(&socks5::request(target)).await?;
            let mut head = vec![0u8; 5];
            conn.read_exact(&mut head).await?;
            let total = match head[3] {
                1 => 10,
                4 => 22,
                3 => 7 + head[4] as usize,
                _ => 0,
            };
            if total < 5 {
                return Ok(Err(ProxyFail::Unreachable(format!("上游代理 {upstream} 的回应无法理解"))));
            }
            head.resize(total, 0);
            conn.read_exact(&mut head[5..]).await?;
            Ok::<_, io::Error>(match socks5::parse_reply(&head) {
                Ok(Some((_, 0))) => Ok(()),
                Ok(Some((_, 2))) => Err(ProxyFail::Denied(format!("上游代理 {upstream} 不允许访问 {target}"))),
                Ok(Some((_, code))) => Err(ProxyFail::Refused(format!("上游代理 {upstream} 连不上 {target}（回应码 {code}）"))),
                _ => Err(ProxyFail::Unreachable(format!("上游代理 {upstream} 的回应无法理解"))),
            })
        };
        match tokio::time::timeout(PROXY_HANDSHAKE_TIMEOUT, handshake).await {
            Ok(Ok(Ok(()))) => Ok(Upstream::Tcp(conn)),
            Ok(Ok(Err(fail))) => Err(fail),
            Ok(Err(e)) => Err(ProxyFail::Unreachable(format!("上游代理 {upstream} 断开了：{e}"))),
            Err(_) => Err(ProxyFail::Unreachable(format!("上游代理 {upstream} 没有回应"))),
        }
    }

    /// 网络里的服务：本机的直接连，别的设备的经 Nexus 连（对方判定授权并审计）。
    async fn open_service(&self, device: &str, name: &str) -> Result<Upstream<S::Tcp, <T::Conn as Connection>::Stream>, ProxyFail> {
        let device = self.resolve(device).map_err(|e| ProxyFail::Unreachable(e.message))?;
        if device.node_id == self.me() {
            let addr = self.service_addr(name).ok_or_else(|| ProxyFail::Unreachable(format!("本机没有发布服务 {name}")))?;
            return self.system.tcp_connect(addr).await.map(Upstream::Tcp).map_err(|e| {
                ProxyFail::Unreachable(format!("服务 {name} 的本机程序没有在运行（{e}）"))
            });
        }
        let body = Request::SvcConnect { name: name.to_string() };
        for attempt in 0..2 {
            match self.open_stream(&device, body.clone(), None).await {
                Ok(stream) => return Ok(Upstream::Remote(stream)),
                Err(OpenError::StaleRoster) if attempt == 0 => {
                    let _ = self.push_roster(device.node_id).await;
                }
                Err(OpenError::StaleRoster) => break,
                Err(OpenError::Remote(e)) => return Err(ProxyFail::Unreachable(e.message)),
                Err(OpenError::Io(e)) => return Err(ProxyFail::Unreachable(format!("连不上 {}：{e}", device.name))),
            }
        }
        Err(ProxyFail::Unreachable("对方的名册始终比本机旧".into()))
    }

    async fn remote_session_kill(&self, node: &str, name: &str, agent: Option<&AgentCredential>) -> Result<(), WireError> {
        let device = self.resolve_for(node, agent)?;
        let body = Request::SessionKill { name: name.to_string() };
        if device.node_id == self.me() {
            required_action(&body, &device.name)?;
            self.authorize_local(agent, &body)?;
            return self.session_kill(name);
        }
        match self.call_node(&device, body, agent).await? {
            Response::Ok => Ok(()),
            other => Err(internal(format!("对方的回应无法理解：{other:?}"))),
        }
    }

    /// 到其他设备的路径（M5.1）。
    fn paths(&self, device: Option<&str>) -> Result<Vec<nexus_wire::DevicePaths>, WireError> {
        let devices = match device {
            Some(d) => vec![self.resolve(d)?],
            None => {
                let roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机还没有加入任何网络"))?;
                roster.devices.into_iter().filter(|d| d.node_id != self.me()).collect()
            }
        };
        Ok(devices
            .into_iter()
            .map(|d| nexus_wire::DevicePaths { paths: self.transport.paths(d.node_id), device: d.name })
            .collect())
    }

    fn local_status(&self) -> StatusReport {
        let role = self.roster().and_then(|r| r.device(self.me()).map(|d| d.role));
        StatusReport {
            name: self.name(),
            node_id: self.me(),
            info: Some(self.info()),
            online: true,
            rtt_ms: None,
            path: None,
            role,
        }
    }

    fn init(&self, name: Option<String>) -> Result<InitReport, WireError> {
        if self.trust.borrow().root.is_some() {
            return Err(err(ErrorCode::InvalidState, "本机已经属于一个网络"));
        }
        let name = name.unwrap_or_else(|| self.configured_name.clone());
        if !valid_segment(&name) {
            return Err(err(ErrorCode::InvalidInput, format!("设备名 {name:?} 不合法：只能用字母、数字、- 和 _")));
        }
        let mut seed = [0u8; 32];
        self.system.random(&mut seed);
        let root = SecretKey::from_seed(seed);
        let roster = Roster {
            root: root.public(),
            version: 1,
            devices: vec![Device { node_id: self.me(), name: name.clone(), role: DeviceRole::Admin, added_at: self.system.now() }],
            revoked_agents: vec![],
            web_keys: vec![],
            anchor: None,
        }
        .sign(&root);
        let persist = || -> io::Result<()> {
            save(&self.system, KEY_ROOT_KEY, &seed)?;
            save(&self.system, KEY_ROOT_PUB, &root.public())?;
            save(&self.system, KEY_ROSTER, &roster)?;
            Ok(())
        };
        persist().map_err(internal)?;
        let mut trust = self.trust.borrow_mut();
        trust.root = Some(root.public());
        trust.root_key = Some(seed);
        trust.roster = Some(roster);
        self.roster_epoch.send_modify(|v| *v += 1);
        Ok(InitReport { name, node_id: self.me(), root: root.public(), recovery_code: hex_encode(&seed) })
    }

    async fn join_start(&self) -> Result<CtlResponse, WireError> {
        if self.trust.borrow().root.is_some() {
            return Err(err(ErrorCode::InvalidState, "本机已经属于一个网络"));
        }
        let mut nonce = [0u8; 16];
        self.system.random(&mut nonce);
        let expires_at = Timestamp(self.system.now().0 + JOIN_CODE_TTL_MS);
        let previous = self.pending_join.borrow_mut().replace(PendingJoin { nonce, expires_at, short_seed: None });
        let code = JoinCode {
            node_id: self.me(),
            nonce,
            name: self.configured_name.clone(),
            addr: self.transport.local_hint(),
        };
        // 旧的短码立即失效（尽力而为：碰头服务连不上时它也会在 10 分钟后过期）。
        if let Some(seed) = previous.and_then(|p| p.short_seed) {
            let _ = tokio::time::timeout(REQUEST_TIMEOUT, self.transport.rendezvous_put(seed, Vec::new())).await;
        }
        // 短加入码：加密后的长加入码放到碰头服务上，位置和密钥都从短码派生。
        let mut random = [0u8; 7];
        self.system.random(&mut random);
        let short = ShortCode::from_random(random);
        let keys = short.keys();
        let mut salt = [0u8; 16];
        self.system.random(&mut salt);
        let sealed = joincode::seal(&keys, salt, &code.to_bytes());
        let put = tokio::time::timeout(REQUEST_TIMEOUT, self.transport.rendezvous_put(keys.seed, sealed)).await;
        let (short_code, short_code_error) = match put {
            Ok(Ok(())) => {
                if let Some(p) = self.pending_join.borrow_mut().as_mut().filter(|p| p.nonce == nonce) {
                    p.short_seed = Some(keys.seed);
                }
                (Some(short.display()), None)
            }
            Ok(Err(e)) if e.kind() == io::ErrorKind::Unsupported => (None, Some(format!("没有碰头服务（{e}）"))),
            Ok(Err(e)) => (None, Some(format!("连不上碰头服务：{e}"))),
            Err(_) => (None, Some("连接碰头服务超时".to_string())),
        };
        Ok(CtlResponse::JoinCode { code: code.encode(), short_code, short_code_error, expires_at })
    }

    /// 管理设备拿到的加入码：长码直接解码；短码到碰头服务上取回、解密（M6.1）。
    /// 返回长加入码和短码的位置（用过之后让它失效）。
    async fn resolve_join_code(&self, text: &str) -> Result<(JoinCode, Option<[u8; 32]>), WireError> {
        if JoinCode::looks_like(text) {
            return JoinCode::decode(text).map(|c| (c, None)).ok_or_else(|| err(ErrorCode::InvalidInput, "加入码格式不对"));
        }
        let short = ShortCode::parse(text).ok_or_else(|| err(ErrorCode::InvalidInput, "加入码格式不对"))?;
        let keys = short.keys();
        let got = tokio::time::timeout(REQUEST_TIMEOUT, self.transport.rendezvous_get(keys.seed)).await;
        let data = match got {
            Ok(Ok(Some(data))) if !data.is_empty() => data,
            Ok(Ok(_)) => return Err(err(ErrorCode::NotFound, "找不到这个短加入码：已过期、已经用过，或者输错了")),
            Ok(Err(e)) if e.kind() == io::ErrorKind::Unsupported => {
                return Err(err(ErrorCode::InvalidState, format!("本机没有碰头服务，只能用长加入码（{e}）")))
            }
            Ok(Err(e)) => return Err(err(ErrorCode::Unreachable, format!("连不上碰头服务：{e}"))),
            Err(_) => return Err(err(ErrorCode::Unreachable, "连接碰头服务超时")),
        };
        let plain = joincode::open(&keys, &data).ok_or_else(|| err(ErrorCode::InvalidInput, "碰头服务上的短加入码记录无法解密"))?;
        let code = JoinCode::from_bytes(&plain).ok_or_else(|| err(ErrorCode::InvalidInput, "碰头服务上的短加入码记录格式不对"))?;
        Ok((code, Some(keys.seed)))
    }

    fn admin_key(&self) -> Result<SecretKey, WireError> {
        self.trust
            .borrow()
            .root_key
            .map(SecretKey::from_seed)
            .ok_or_else(|| err(ErrorCode::InvalidState, "只有管理设备（执行过 nexus init 的设备）能修改名册"))
    }

    async fn approve(&self, code: &str, name: Option<String>) -> Result<MembershipReport, WireError> {
        // 名册的修改串行进行。等新设备确认期间（最长两倍请求超时）一直持有：
        // 发给它的名册必须是最新的，同时发生的撤销等它确认之后再写入下一个版本。
        let _write = self.roster_write.lock().await;
        let root = self.admin_key()?;
        let (code, short_seed) = self.resolve_join_code(code).await?;
        let name = name.unwrap_or_else(|| code.name.clone());
        if !valid_segment(&name) {
            return Err(err(ErrorCode::InvalidInput, format!("设备名 {name:?} 不合法：只能用字母、数字、- 和 _")));
        }
        let mut roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机没有名册"))?;
        if roster.device(code.node_id).is_some() {
            return Err(err(ErrorCode::InvalidState, "这台设备已经在名册里"));
        }
        if roster.device_by_name(&name).is_some() {
            return Err(err(ErrorCode::InvalidInput, format!("设备名 {name} 已被占用，请用 --name 换一个")));
        }
        roster.version += 1;
        roster.devices.push(Device { node_id: code.node_id, name: name.clone(), role: DeviceRole::Member, added_at: self.system.now() });
        let signed = roster.sign(&root);

        self.transport.add_hint(code.node_id, &code.addr);
        let offer = Request::JoinOffer { nonce: code.nonce, roster: signed.clone(), addrs: self.addr_hints() };
        let reply = tokio::time::timeout(REQUEST_TIMEOUT * 2, self.request_once(code.node_id, Proto::Join, Principal::Device, offer)).await;
        match reply {
            Ok(Ok((Response::Ok, _))) => {}
            Ok(Ok((Response::Error(e), _))) => return Err(err(e.code, format!("对方拒绝加入：{}", e.message))),
            Ok(Ok((other, _))) => return Err(internal(format!("对方的回应无法理解：{other:?}"))),
            Ok(Err(e)) => return Err(err(ErrorCode::Unreachable, format!("连不上这台设备：{e}"))),
            Err(_) => return Err(err(ErrorCode::Unreachable, "连接这台设备超时")),
        }
        // 只有对方确认之后才更新本机名册，避免出现半加入的状态。
        self.adopt_roster(&signed)?;
        let _ = self.learn_hints(&vec![(code.node_id, code.addr.clone())]);
        drop(_write);
        // 用过的短码立即失效。
        if let Some(seed) = short_seed {
            let _ = tokio::time::timeout(REQUEST_TIMEOUT, self.transport.rendezvous_put(seed, Vec::new())).await;
        }
        let (pushed, unreachable) = self.broadcast_roster(Some(code.node_id)).await;
        Ok(MembershipReport { name, node_id: code.node_id, roster_version: self.roster_version(), pushed, unreachable })
    }

    async fn revoke(&self, name: &str) -> Result<MembershipReport, WireError> {
        let _write = self.roster_write.lock().await;
        let root = self.admin_key()?;
        let mut roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机没有名册"))?;
        let device = roster
            .device_by_name(name)
            .cloned()
            .ok_or_else(|| err(ErrorCode::UnknownNode, format!("名册里没有设备 {name}")))?;
        if device.node_id == self.me() {
            return Err(err(ErrorCode::InvalidInput, "不能把管理设备自己移出名册"));
        }
        roster.version += 1;
        roster.devices.retain(|d| d.node_id != device.node_id);
        let signed = roster.sign(&root);
        save(&self.system, KEY_ROSTER, &signed).map_err(internal)?;
        self.trust.borrow_mut().roster = Some(signed);
        self.roster_epoch.send_modify(|v| *v += 1);
        drop(_write);
        let (pushed, unreachable) = self.broadcast_roster(None).await;
        Ok(MembershipReport { name: name.to_string(), node_id: device.node_id, roster_version: self.roster_version(), pushed, unreachable })
    }

    /// 用恢复码（网络根密钥）让本机成为管理设备（M6.2）：名册里本机改为管理设备、原来的改为成员。
    async fn recover(&self, code: &str) -> Result<MembershipReport, WireError> {
        let _write = self.roster_write.lock().await;
        let (root_pub, has_key) = {
            let t = self.trust.borrow();
            (t.root, t.root_key.is_some())
        };
        let root_pub = root_pub.ok_or_else(|| err(ErrorCode::InvalidState, "本机还不属于任何网络：恢复码只能在网络里的设备上用"))?;
        if has_key {
            return Err(err(ErrorCode::InvalidState, "本机已经是管理设备"));
        }
        let cleaned: String = code.chars().filter(|c| !c.is_whitespace() && *c != '-').collect::<String>().to_ascii_lowercase();
        let seed: [u8; 32] = nexus_wire::hex_decode(&cleaned)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| err(ErrorCode::InvalidInput, "恢复码格式不对：应当是 64 个十六进制字符（nexus init 输出的）"))?;
        let root = SecretKey::from_seed(seed);
        if root.public() != root_pub {
            return Err(err(ErrorCode::InvalidInput, "恢复码不属于这个网络"));
        }
        let mut roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机没有名册"))?;
        let me = self.me();
        for d in &mut roster.devices {
            d.role = if d.node_id == me { DeviceRole::Admin } else { DeviceRole::Member };
        }
        roster.version += 1;
        let signed = roster.sign(&root);
        let persist = || -> io::Result<()> {
            save(&self.system, KEY_ROOT_KEY, &seed)?;
            save(&self.system, KEY_ROSTER, &signed)?;
            Ok(())
        };
        persist().map_err(internal)?;
        let name = signed.roster.device(me).map(|d| d.name.clone()).unwrap_or_default();
        {
            let mut t = self.trust.borrow_mut();
            t.root_key = Some(seed);
            t.roster = Some(signed);
        }
        self.roster_epoch.send_modify(|v| *v += 1);
        drop(_write);
        let (pushed, unreachable) = self.broadcast_roster(None).await;
        Ok(MembershipReport { name, node_id: me, roster_version: self.roster_version(), pushed, unreachable })
    }

    /// 在名册里按名字或标识找设备。
    fn resolve(&self, name_or_id: &str) -> Result<Device, WireError> {
        let roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机还没有加入任何网络"))?;
        let by_id = name_or_id.parse::<NodeId>().ok().and_then(|id| roster.device(id).cloned());
        roster
            .device_by_name(name_or_id)
            .cloned()
            .or(by_id)
            .ok_or_else(|| err(ErrorCode::UnknownNode, format!("不认识节点 {name_or_id}")))
    }

    async fn probe(&self, device: &Device) -> Result<StatusReport, WireError> {
        let started = self.system.now();
        // 探测总是以本机设备的身份进行；agent 能看到什么，由调用方按授权过滤。
        let reply = tokio::time::timeout(REQUEST_TIMEOUT, self.request(device, Request::NodeInfo, None)).await;
        let rtt_ms = self.system.now().millis_since(started);
        let offline = StatusReport {
            name: device.name.clone(),
            node_id: device.node_id,
            info: None,
            online: false,
            rtt_ms: None,
            path: None,
            role: Some(device.role),
        };
        match reply {
            Ok(Ok((Response::NodeInfo(info), path))) if info.node_id == device.node_id => Ok(StatusReport {
                info: Some(info),
                online: true,
                rtt_ms: Some(rtt_ms),
                path,
                ..offline
            }),
            Ok(Ok((Response::Error(e), _))) if e.code == ErrorCode::Denied => Err(err(
                ErrorCode::Denied,
                format!("{} 拒绝了访问（本机可能已被移出名册）：{}", device.name, e.message),
            )),
            // 超时、连接失败、或对端自称是别的节点：一律视为不可达。
            _ => Ok(offline),
        }
    }

    async fn remote_status(&self, name_or_id: &str, agent: Option<&AgentCredential>) -> Result<StatusReport, WireError> {
        let device = self.resolve_for(name_or_id, agent)?;
        if device.node_id == self.me() {
            return Ok(self.local_status());
        }
        self.probe(&device).await
    }

    async fn list(&self, agent: Option<&AgentCredential>) -> Vec<ListEntry> {
        let Some(roster) = self.roster() else {
            return vec![ListEntry { report: self.local_status(), is_self: true }];
        };
        let devices = roster.devices.iter().filter(|d| agent.is_none_or(|a| visible(a, &d.name)));
        let reports = join_all(devices.map(|d| async move {
            if d.node_id == self.me() {
                return ListEntry { report: self.local_status(), is_self: true };
            }
            let report = self.probe(d).await.unwrap_or(StatusReport {
                name: d.name.clone(),
                node_id: d.node_id,
                info: None,
                online: false,
                rtt_ms: None,
                path: None,
                role: Some(d.role),
            });
            ListEntry { report, is_self: false }
        }))
        .await;
        reports
    }
}

/// agent 的授权是否让它能看到这台设备（或设备之下的东西）。
fn visible(agent: &AgentCredential, device: &str) -> bool {
    ResourcePath::parse(device).is_ok_and(|p| agent.cert.can_see(&p))
}

/// 可取消的帧写入；部分写入的进度保存在调用方，不受 select! 取消影响。
async fn write_pending<W: AsyncWrite + Unpin>(wr: &mut W, frame: &mut (Vec<u8>, usize)) -> io::Result<()> {
    while frame.1 < frame.0.len() {
        let n = wr.write(&frame.0[frame.1..]).await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "无法发送执行输出"));
        }
        frame.1 += n;
    }
    wr.flush().await
}

/// 限时写一帧：写入卡住 [`PEER_SILENT`] 也算失败（连接可能已经失效而传输层还没发现）。
async fn send_within<W: AsyncWrite + Unpin, M: serde::Serialize>(wr: &mut W, msg: &M) -> bool {
    matches!(tokio::time::timeout(PEER_SILENT, write_msg(wr, msg)).await, Ok(Ok(())))
}

/// 页面地址对应的依赖方（通行密钥绑定的域名）：`https://域名[:端口]`，或开发用的 `http://localhost[:端口]`。
fn page_rp_id(page: &str) -> Option<String> {
    let (_, rest) = page.split_once("://")?;
    let host = rest.split(':').next()?;
    (!host.is_empty() && nexus_core_proto::webauthn::origin_allowed(page, host)).then(|| host.to_string())
}

/// 一组授权能否看到 `path`（或它之下的东西）。
fn grants_see(grants: &[Grant], path: &ResourcePath) -> bool {
    grants.iter().any(|g| g.resource.may_match_within(path))
}

/// 一个主体是否被授权做这个动作（只看授权范围；身份和凭据已在请求入口校验过）。
fn principal_may(roster: &Roster, principal: &Principal, action: &Action) -> bool {
    match principal {
        Principal::Device => true,
        Principal::Agent { cert, .. } => cert.cert.grants.iter().any(|g| g.allows(action)),
        Principal::Web { session } => roster
            .web_key(&session.session.label)
            .filter(|k| k.credential_id == session.session.credential_id)
            .is_some_and(|k| k.grants.iter().any(|g| g.allows(action))),
    }
}

/// 一个主体能否看到 `path`：设备本人看到一切，agent 和网页登录按各自的授权。
fn principal_sees(roster: &Roster, principal: &Principal, path: &ResourcePath) -> bool {
    match principal {
        Principal::Device => true,
        Principal::Agent { cert, .. } => cert.can_see(path),
        Principal::Web { session } => roster
            .web_key(&session.session.label)
            .filter(|k| k.credential_id == session.session.credential_id)
            .is_some_and(|k| grants_see(&k.grants, path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_sys::{ExitStatus, Size};
    use tokio::io::DuplexStream;
    use tokio::sync::Notify;

    #[derive(Default)]
    struct ExecState {
        output_ready: Notify,
        killed: Notify,
        is_killed: Cell<bool>,
    }

    struct TestSystem(Rc<ExecState>);
    struct TestProc {
        state: Rc<ExecState>,
        output_sent: bool,
    }
    struct TestPty;
    struct TestTransport;
    struct TestConnection;

    impl Transport for TestTransport {
        type Conn = TestConnection;
        fn node_id(&self) -> NodeId {
            NodeId([1; 32])
        }
        fn local_hint(&self) -> String {
            String::new()
        }
        fn add_hint(&self, _: NodeId, _: &str) {}
        async fn connect(&self, _: NodeId, _: Proto) -> io::Result<TestConnection> {
            unreachable!()
        }
        async fn accept(&self) -> io::Result<(TestConnection, Proto)> {
            unreachable!()
        }
    }
    impl Connection for TestConnection {
        type Stream = DuplexStream;
        fn remote(&self) -> NodeId {
            unreachable!()
        }
        fn path_kind(&self) -> Option<nexus_core_proto::PathKind> {
            None
        }
        async fn open_bi(&self) -> io::Result<DuplexStream> {
            unreachable!()
        }
        async fn accept_bi(&self) -> io::Result<Option<DuplexStream>> {
            unreachable!()
        }
    }
    impl System for TestSystem {
        type Proc = TestProc;
        type Pty = TestPty;
        type Tcp = DuplexStream;
        async fn tcp_connect(&self, _: std::net::SocketAddr) -> io::Result<DuplexStream> {
            Err(io::ErrorKind::ConnectionRefused.into())
        }
        async fn resolve(&self, _: &str) -> io::Result<Vec<std::net::IpAddr>> {
            Err(io::ErrorKind::NotFound.into())
        }
        fn now(&self) -> Timestamp {
            Timestamp(0)
        }
        fn random(&self, _: &mut [u8]) {
            unreachable!()
        }
        fn store_get(&self, _: &str) -> io::Result<Option<Vec<u8>>> {
            Ok(None)
        }
        fn cas_get(&self, _: &[u8; 32]) -> io::Result<Option<Vec<u8>>> {
            Ok(None)
        }
        fn cas_put(&self, _: &[u8; 32], _: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn cas_has(&self, _: &[u8; 32]) -> io::Result<bool> {
            Ok(false)
        }
        fn store_put(&self, _: &str, _: &[u8]) -> io::Result<()> {
            unreachable!()
        }
        fn append_log(&self, _: &str, _: &[u8], _: u64) -> io::Result<()> {
            unreachable!()
        }
        fn read_log(&self, _: &str) -> io::Result<Vec<u8>> {
            unreachable!()
        }
        fn spawn(&self, _: Cmd) -> io::Result<TestProc> {
            Ok(TestProc {
                state: self.0.clone(),
                output_sent: false,
            })
        }
        fn spawn_pty(&self, _: &str, _: Size, _: Timestamp) -> io::Result<TestPty> {
            unreachable!()
        }
    }
    impl Pty for TestPty {
        async fn next_event(&mut self) -> PtyEvent {
            unreachable!()
        }
        fn write(&mut self, _: Vec<u8>) {
            unreachable!()
        }
        fn resize(&mut self, _: Size) {
            unreachable!()
        }
        fn kill(&mut self) {
            unreachable!()
        }
    }
    impl Proc for TestProc {
        async fn next_event(&mut self) -> ProcEvent {
            if self.state.is_killed.get() {
                return ProcEvent::Exited(ExitStatus {
                    code: None,
                    signal: Some(9),
                });
            }
            if !self.output_sent {
                self.output_sent = true;
                self.state.output_ready.notify_one();
                return ProcEvent::Stdout(vec![b'x'; 65536]);
            }
            std::future::pending().await
        }
        fn write_stdin(&mut self, _: Vec<u8>) {}
        fn close_stdin(&mut self) {}
        fn kill(&mut self) {
            self.state.is_killed.set(true);
            self.state.killed.notify_one();
        }
    }

    /// FP-7.1-02：不认识的请求得到说明版本的回应，而不是连接被断开。
    #[tokio::test]
    async fn unknown_request_gets_a_version_answer() {
        let node = Node::new(TestTransport, TestSystem(Rc::default()), "home").unwrap();
        let (mut client, server) = tokio::io::duplex(4096);
        let header = RequestHeader::new(0, Principal::Device, &Request::NodeInfo);
        let mut bytes = postcard::to_allocvec(&header).unwrap();
        // 请求体是最后一个字段（长度 1，内容是请求的种类编号）：换成一个以后才有的编号。
        let last = bytes.len() - 1;
        bytes[last] = 120;
        let future: RequestHeader = postcard::from_bytes(&bytes).unwrap();
        let serve = node.serve_stream(&TestConnection, NodeId([2; 32]), Proto::Rpc, server);
        let ask = async {
            write_msg(&mut client, &future).await.unwrap();
            read_response(&mut client).await.unwrap()
        };
        let ((), answer) = tokio::time::timeout(Duration::from_secs(3), futures_util::future::join(serve, ask)).await.unwrap();
        match answer {
            Response::Error(e) => {
                assert_eq!(e.code, ErrorCode::UnsupportedVersion);
                assert!(e.message.contains("home") && e.message.contains(env!("CARGO_PKG_VERSION")) && e.message.contains("升级"), "{}", e.message);
            }
            other => panic!("应当回应版本错误：{other:?}"),
        }
    }

    async fn stalled_exec(cancel: bool) {
        let state = Rc::new(ExecState::default());
        let node = Node::new(TestTransport, TestSystem(state.clone()), "test").unwrap();
        // 比输出帧小得多，强制发生部分写入和反压。
        let (mut client, server) = tokio::io::duplex(16);
        let spec = ExecSpec {
            program: "test".into(),
            args: vec![],
            cwd: None,
            env: vec![],
            timeout_ms: (!cancel).then_some(200),
            max_output: 1 << 20,
        };
        let observer = async {
            assert!(matches!(
                read_response(&mut client).await.unwrap(),
                Response::Ok
            ));
            state.output_ready.notified().await;
            if cancel {
                write_msg(&mut client, &Input::Cancel).await.unwrap();
            }
            // 此时故意不读输出；超时和取消必须在窗口仍被堵住时杀掉进程。
            tokio::time::timeout(Duration::from_secs(1), state.killed.notified())
                .await
                .expect("反压不能阻止进程终止");
            if !cancel {
                // 超时打断部分写入之后，恢复读取，帧和结果仍完整。
                assert!(
                    matches!(read_msg::<_, ExecEvent>(&mut client).await.unwrap(), ExecEvent::Stdout(b) if b == vec![b'x'; 65536])
                );
                let ExecEvent::Exit(result) = read_msg(&mut client).await.unwrap() else {
                    panic!("应当收到退出结果")
                };
                assert!(result.timed_out);
            }
            drop(client);
        };
        tokio::time::timeout(
            Duration::from_secs(3),
            futures_util::future::join(node.serve_exec(server, spec), observer),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn timeout_kills_exec_while_output_is_blocked() {
        // covers: R10
        stalled_exec(false).await;
    }

    #[tokio::test]
    async fn cancel_kills_exec_while_output_is_blocked() {
        // covers: R10
        stalled_exec(true).await;
    }
}
