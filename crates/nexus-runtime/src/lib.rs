//! 运行时：把纯逻辑接到 `Transport` 和 `System` 两个接缝上。
//!
//! 本 crate 不直接引用任何具体的传输或系统实现，也不自行创建任务：
//! 所有并发都在 `Node::run` 这一个 future 里完成，因此同一份代码既能跑在
//! 真实的异步运行时上，也能跑在单线程的仿真里。
#![forbid(unsafe_code)]

use futures_util::future::join_all;
use futures_util::stream::{FuturesUnordered, StreamExt};
use nexus_core_proto::authz::{
    self, Action, AgentCert, Assertion, AuditEvent, Context, DenyReason, Grant, Outcome, Principal, ReplayGuard,
};
use nexus_core_proto::path::{CapabilityPattern, ResourcePattern};
use nexus_core_proto::roster::RevokedAgent;
use nexus_core_proto::path::{valid_segment, Capability, ResourcePath};
use nexus_core_proto::roster::{accept_update, Device, DeviceRole, Roster, SignedRoster};
use nexus_core_proto::{NodeId, NodeInfo, PublicKey, SecretKey, Timestamp};
use nexus_fabric::{read_msg, write_msg, Connection, FrameReader, Proto, Transport};
use nexus_sys::{Cmd, Proc, ProcEvent, Pty, PtyEvent, System};
use nexus_term_proto::{Action as TermAction, ClientId, Event as TermEvent, Session as TermSession, DEFAULT_WINDOW};
use nexus_wire::{
    hex_encode, AddrHints, AgentCredential, AgentInfo, AgentIssueReport, AuditRecord, ClientMsg, CtlRequest, CtlResponse, ErrorCode, ExecEvent, ExecResult, ExecSpec, InitReport,
    Input, JoinCode, ListEntry, MembershipReport, Request, RequestHeader, Response, SessionInfo, Size, StatusReport,
    WireError, WIRE_VERSION,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
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
    Session { name: String, create: bool, size: Size },
}

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

/// 会话里的程序结束后，等落后的客户端追上最终屏幕的最长时间。
const SESSION_LINGER: Duration = Duration::from_secs(10);

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
const LOG_AUDIT: &str = "audit";

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
        Request::RosterPush { .. } | Request::JoinOffer { .. } => return Ok(None),
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
}

type Task<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

pub struct Node<T, S: System> {
    transport: T,
    system: S,
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
}

impl<T: Transport, S: System> Node<T, S> {
    pub fn new(transport: T, system: S, name: &str) -> io::Result<Self> {
        let trust = Trust {
            root: load(&system, KEY_ROOT_PUB)?,
            roster: load(&system, KEY_ROSTER)?,
            root_key: load(&system, KEY_ROOT_KEY)?,
            addrs: load::<_, Vec<(NodeId, String)>>(&system, KEY_ADDRS)?.unwrap_or_default().into_iter().collect(),
        };
        for (node, hint) in &trust.addrs {
            transport.add_hint(*node, hint);
        }
        let (push_tx, push_rx) = mpsc::unbounded_channel();
        let (spawn_tx, spawn_rx) = mpsc::unbounded_channel();
        Ok(Self {
            sessions: RefCell::default(),
            next_client: Cell::new(1),
            spawn_tx,
            spawn_rx: RefCell::new(Some(spawn_rx)),
            replay: RefCell::default(),
            started_at: system.now(),
            transport,
            system,
            configured_name: name.to_string(),
            trust: RefCell::new(trust),
            pending_join: RefCell::new(None),
            push_tx,
            push_rx: RefCell::new(Some(push_rx)),
        })
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
        Ok(())
    }

    /// 驱动整个节点：接受连接、处理请求、处理本机控制调用。正常情况下不返回。
    pub async fn run(&self, ctl: &mut mpsc::Receiver<CtlCall>) -> io::Result<()> {
        let mut pushes = self.push_rx.borrow_mut().take().expect("run 只能调用一次");
        let mut spawns = self.spawn_rx.borrow_mut().take().expect("run 只能调用一次");
        let mut tasks: FuturesUnordered<Task<'_>> = FuturesUnordered::new();
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
                        streams.push(Box::pin(self.serve_stream(peer, proto, stream)));
                        accept = Box::pin(conn.accept_bi());
                    }
                    _ => break,
                },
            }
        }
        while streams.next().await.is_some() {}
    }

    async fn serve_stream(&self, peer: NodeId, proto: Proto, mut stream: <T::Conn as Connection>::Stream) {
        let dispatch = match read_msg::<_, RequestHeader>(&mut stream).await {
            Ok(header) => match proto {
                Proto::Rpc => self.handle_rpc(peer, header),
                Proto::Join => Dispatch::Reply(self.handle_join(header)),
            },
            Err(_) => return,
        };
        let response = match dispatch {
            Dispatch::Reply(r) => r,
            Dispatch::Exec(spec) => return self.serve_exec(stream, spec).await,
            Dispatch::Session { name, create, size } => return self.serve_session(stream, &name, create, size).await,
        };
        if write_msg(&mut stream, &response).await.is_err() {
            return;
        }
        let _ = stream.shutdown().await;
        drain(&mut stream).await;
    }

    fn handle_rpc(&self, peer: NodeId, header: RequestHeader) -> Dispatch {
        if header.version != WIRE_VERSION {
            return Dispatch::Reply(Response::Error(err(ErrorCode::UnsupportedVersion, format!("不支持的协议版本 {}", header.version))));
        }
        // 名册推送只靠签名证明自己，不要求发送方已经在本机的旧名册里。
        if let Request::RosterPush { roster, addrs } = &header.body {
            return Dispatch::Reply(match self.adopt_roster(roster) {
                Ok(()) => {
                    let _ = self.learn_hints(addrs);
                    Response::Ok
                }
                Err(e) if matches!(e.code, ErrorCode::InvalidInput) => Response::Ok, // 不比本机新：无需更新
                Err(e) => Response::Error(e),
            });
        }
        let mine = self.roster_version();
        if header.roster_version > mine {
            return Dispatch::Reply(Response::Error(err(ErrorCode::StaleRoster, format!("本机名册版本 {mine} 较旧"))));
        }
        let Some(roster) = self.roster() else {
            return Dispatch::Reply(Response::Error(err(ErrorCode::Denied, "本机还没有加入任何网络")));
        };
        let me = roster.device(self.me()).map(|d| d.name.clone()).unwrap_or_default();
        let action = match required_action(&header.body, &me) {
            Ok(Some(action)) => action,
            Ok(None) => return Dispatch::Reply(Response::Error(err(ErrorCode::InvalidInput, "这个请求不能在此协议上使用"))),
            Err(e) => return Dispatch::Reply(Response::Error(e)),
        };
        if let Err(e) = self.authorize(&roster, peer, &header.principal, &action) {
            return Dispatch::Reply(Response::Error(e));
        }
        if mine > header.roster_version {
            // 对方的名册旧了，稍后在后台推过去。
            let _ = self.push_tx.send(peer);
        }
        Dispatch::Reply(match header.body {
            Request::NodeInfo => Response::NodeInfo(self.info()),
            Request::Exec(spec) => return Dispatch::Exec(spec),
            Request::SessionAttach { name, create, size } => return Dispatch::Session { name, create, size },
            Request::SessionList => Response::Sessions(self.session_list(agent_cert(&header.principal))),
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
    fn handle_join(&self, header: RequestHeader) -> Response {
        let Request::JoinOffer { nonce, roster, addrs } = header.body else {
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
            let _ = self.system.append_log(LOG_AUDIT, &frame);
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
        let header = RequestHeader { version: WIRE_VERSION, roster_version: self.roster_version(), principal, body };
        write_msg(&mut stream, &header).await?;
        stream.shutdown().await?;
        let resp = read_msg(&mut stream).await?;
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
            CtlRequest::Attach { node, session, size } => return self.attach(&node, session, size, agent, server).await,
            CtlRequest::Status => Ok(CtlResponse::Status(self.local_status())),
            CtlRequest::RemoteStatus { node } => self.remote_status(&node, agent_ref).await.map(CtlResponse::Status),
            CtlRequest::Init { name } => self.init(name).map(CtlResponse::Init),
            CtlRequest::JoinStart => self.join_start(),
            CtlRequest::Approve { code, name } => self.approve(&code, name).await.map(CtlResponse::Membership),
            CtlRequest::Revoke { name } => self.revoke(&name).await.map(CtlResponse::Membership),
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
        };
        let _ = server.tx.send(result.unwrap_or_else(CtlResponse::Error)).await;
    }

    /// 带凭据的控制调用：凭据必须属于本网络，而且 agent 不能做管理操作。
    fn check_credential(&self, agent: &AgentCredential, req: &CtlRequest) -> Result<(), WireError> {
        let admin = matches!(
            req,
            CtlRequest::Init { .. }
                | CtlRequest::JoinStart
                | CtlRequest::Approve { .. }
                | CtlRequest::Revoke { .. }
                | CtlRequest::AgentIssue { .. }
                | CtlRequest::AgentRevoke { .. }
                | CtlRequest::AgentList
                | CtlRequest::Audit { .. }
        );
        if admin {
            return Err(err(ErrorCode::Denied, "agent 不能执行管理操作"));
        }
        if self.roster().is_none() {
            return Err(err(ErrorCode::InvalidState, "本机还没有加入任何网络"));
        }
        // 证书本身是否有效、授权是否覆盖，由被访问的设备判定；这里只拦下私钥与证书明显不配的凭据。
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
        let root = self.admin_key()?;
        let mut agents = self.load_agents()?;
        let index = agents
            .iter()
            .position(|a| !a.revoked && (a.label == who || NodeId(a.agent_id.0).to_string() == who))
            .ok_or_else(|| err(ErrorCode::NotFound, format!("没有名为 {who} 的有效 agent")))?;
        let mut roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机没有名册"))?;
        let id = agents[index].agent_id;
        roster.version += 1;
        roster.revoked_agents.push(RevokedAgent { agent: id, revoked_at: self.system.now() });
        let signed = roster.sign(&root);
        save(&self.system, KEY_ROSTER, &signed).map_err(internal)?;
        self.trust.borrow_mut().roster = Some(signed);
        agents[index].revoked = true;
        save(&self.system, KEY_AGENTS, &agents).map_err(internal)?;
        let (pushed, unreachable) = self.broadcast_roster(None).await;
        Ok(MembershipReport {
            name: agents[index].label.clone(),
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
                match read_msg::<_, Response>(&mut near).await {
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
        let open = async {
            let conn = self.transport.connect(to.node_id, Proto::Rpc).await?;
            let mut stream = conn.open_bi().await?;
            let header = RequestHeader { version: WIRE_VERSION, roster_version: self.roster_version(), principal, body };
            write_msg(&mut stream, &header).await?;
            let resp: Response = read_msg(&mut stream).await?;
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
                let _ = write_msg(&mut wr, &Response::Error(failure)).await;
                let _ = wr.shutdown().await;
                reader.drain().await;
                return;
            }
        };
        if write_msg(&mut wr, &Response::Ok).await.is_err() {
            proc.kill();
        }
        let started = self.system.now();
        let limit = spec.timeout_ms.map(Duration::from_millis);
        let deadline = tokio::time::sleep(limit.unwrap_or(Duration::from_secs(100 * 365 * 24 * 3600)));
        tokio::pin!(deadline);
        let mut sent: u64 = 0;
        let (mut timed_out, mut truncated, mut input_open, mut caller_gone) = (false, false, true, false);
        loop {
            tokio::select! {
                biased;
                // 计时器放在最前：命令不停地输出时，超时也必须能生效。
                _ = &mut deadline, if limit.is_some() && !timed_out => {
                    timed_out = true;
                    proc.kill();
                }
                event = proc.next_event() => match event {
                    ProcEvent::Exited(status) => {
                        let result = ExecResult {
                            exit_code: status.code,
                            signal: status.signal,
                            timed_out,
                            truncated,
                            duration_ms: self.system.now().millis_since(started),
                        };
                        if !caller_gone {
                            let _ = write_msg(&mut wr, &ExecEvent::Exit(result)).await;
                        }
                        break;
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
                            if write_msg(&mut wr, &event).await.is_err() {
                                caller_gone = true;
                                proc.kill();
                            }
                        }
                    }
                },
                input = reader.next::<Input>(), if input_open => match input {
                    Ok(Some(Input::Stdin(bytes))) => proc.write_stdin(bytes),
                    Ok(Some(Input::CloseStdin)) => proc.close_stdin(),
                    Ok(None) => {
                        input_open = false;
                        proc.close_stdin();
                    }
                    // 终端会话的消息对执行没有意义。
                    Ok(Some(Input::Resize(_) | Input::Ack(_))) => {}
                    Ok(Some(Input::Cancel | Input::Detach)) | Err(_) => {
                        input_open = false;
                        caller_gone = true;
                        proc.kill();
                    }
                },
            }
        }
        let _ = wr.shutdown().await;
        reader.drain().await;
    }

    // ------------------------------------------------------------ 终端会话

    /// 命令行请求连上某个节点上的会话：打开会话流，然后在命令行和会话之间转发。
    async fn attach(&self, node: &str, name: String, size: Size, agent: Option<AgentCredential>, server: CtlServer) {
        let device = match self.resolve_for(node, agent.as_ref()) {
            Ok(d) => d,
            Err(e) => {
                let _ = server.tx.send(CtlResponse::Error(e)).await;
                return;
            }
        };
        if device.node_id == self.me() {
            let body = Request::SessionAttach { name: name.clone(), create: true, size };
            let check = required_action(&body, &device.name).and_then(|_| self.authorize_local(agent.as_ref(), &body));
            if let Err(e) = check {
                let _ = server.tx.send(CtlResponse::Error(e)).await;
                return;
            }
            let (near, far) = tokio::io::duplex(256 * 1024);
            let client = async {
                let mut near = near;
                match read_msg::<_, Response>(&mut near).await {
                    Ok(Response::Ok) => self.pump_session(near, server).await,
                    Ok(Response::Error(e)) => {
                        let _ = server.tx.send(CtlResponse::Error(e)).await;
                    }
                    other => {
                        let _ = server.tx.send(CtlResponse::Error(internal(format!("{other:?}")))).await;
                    }
                }
            };
            futures_util::future::join(self.serve_session(far, &name, true, size), client).await;
            return;
        }
        let body = Request::SessionAttach { name, create: true, size };
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
        loop {
            tokio::select! {
                biased;
                msg = reader.next::<ClientMsg>() => match msg {
                    Ok(Some(msg)) => {
                        let done = matches!(msg, ClientMsg::Exit(_));
                        if server.tx.send(CtlResponse::Session(msg)).await.is_err() && input_open {
                            input_open = false;
                            let _ = write_msg(&mut wr, &Input::Detach).await;
                        }
                        if done {
                            break;
                        }
                    }
                    Ok(None) | Err(_) => {
                        let lost = err(ErrorCode::Unreachable, "与会话的连接中断");
                        let _ = server.tx.send(CtlResponse::Error(lost)).await;
                        break;
                    }
                },
                input = server.rx.recv(), if input_open => {
                    let input = input.unwrap_or(Input::Detach);
                    let leaving = matches!(input, Input::Detach | Input::Cancel);
                    if write_msg(&mut wr, &input).await.is_err() || leaving {
                        input_open = false;
                        if leaving {
                            break;
                        }
                    }
                }
            }
        }
        let _ = wr.shutdown().await;
        reader.drain().await;
    }

    /// 会话一侧：连上（必要时创建）会话，转发这个客户端的输入和会话给它的输出，直到它离开或会话结束。
    async fn serve_session<St: AsyncRead + AsyncWrite + Unpin>(&self, stream: St, name: &str, create: bool, size: Size) {
        let (rd, mut wr) = tokio::io::split(stream);
        let mut reader = FrameReader::new(rd);
        let cmd = match self.open_session(name, create, size) {
            Ok(cmd) => cmd,
            Err(e) => {
                let _ = write_msg(&mut wr, &Response::Error(e)).await;
                let _ = wr.shutdown().await;
                reader.drain().await;
                return;
            }
        };
        let client = self.next_client.get();
        self.next_client.set(client + 1);
        let (tx, mut out) = mpsc::unbounded_channel();
        if cmd.send(SessCmd::Attach { client, size, tx }).is_err() || write_msg(&mut wr, &Response::Ok).await.is_err() {
            let _ = cmd.send(SessCmd::Event(TermEvent::Detach { client }));
            return;
        }
        loop {
            tokio::select! {
                biased;
                msg = out.recv() => match msg {
                    Some(msg) => {
                        let done = matches!(msg, ClientMsg::Exit(_));
                        if write_msg(&mut wr, &msg).await.is_err() || done {
                            break;
                        }
                    }
                    None => break,
                },
                input = reader.next::<Input>() => {
                    let event = match input {
                        Ok(Some(Input::Stdin(bytes))) => TermEvent::Input { client, bytes },
                        Ok(Some(Input::Resize(size))) => TermEvent::Resize { client, size },
                        Ok(Some(Input::Ack(bytes))) => TermEvent::Ack { client, bytes },
                        Ok(Some(Input::CloseStdin)) => continue,
                        Ok(Some(Input::Detach | Input::Cancel)) | Ok(None) | Err(_) => break,
                    };
                    if cmd.send(SessCmd::Event(event)).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = cmd.send(SessCmd::Event(TermEvent::Detach { client }));
        let _ = wr.shutdown().await;
        reader.drain().await;
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
        let pty = self.system.spawn_pty(size).map_err(|e| err(ErrorCode::SpawnFailed, format!("无法启动终端：{e}")))?;
        let (cmd, cmds) = mpsc::unbounded_channel();
        let clients = Rc::new(Cell::new(0));
        let current_size = Rc::new(Cell::new(size));
        self.sessions.borrow_mut().insert(
            name.to_string(),
            SessionHandle { cmd: cmd.clone(), created_at: self.system.now(), clients: clients.clone(), size: current_size.clone() },
        );
        let new = NewSession { name: name.to_string(), pty, size, cmds, clients, current_size };
        if self.spawn_tx.send(new).is_err() {
            self.sessions.borrow_mut().remove(name);
            return Err(internal("节点正在关闭"));
        }
        Ok(cmd)
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

    /// 会话列表；给 agent 时只包含它能看到的会话。
    fn session_list(&self, agent: Option<&authz::SignedAgentCert>) -> Vec<SessionInfo> {
        let me = self.name();
        self.sessions
            .borrow()
            .iter()
            .filter(|(name, _)| {
                agent.is_none_or(|c| ResourcePath::parse(&format!("{me}/session/{name}")).is_ok_and(|p| c.can_see(&p)))
            })
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
            return Ok(self.session_list(agent.map(|a| &a.cert)));
        }
        match self.call_node(&device, Request::SessionList, agent).await? {
            Response::Sessions(list) => Ok(list),
            other => Err(internal(format!("对方的回应无法理解：{other:?}"))),
        }
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
        Ok(InitReport { name, node_id: self.me(), root: root.public(), recovery_code: hex_encode(&seed) })
    }

    fn join_start(&self) -> Result<CtlResponse, WireError> {
        if self.trust.borrow().root.is_some() {
            return Err(err(ErrorCode::InvalidState, "本机已经属于一个网络"));
        }
        let mut nonce = [0u8; 16];
        self.system.random(&mut nonce);
        let expires_at = Timestamp(self.system.now().0 + JOIN_CODE_TTL_MS);
        *self.pending_join.borrow_mut() = Some(PendingJoin { nonce, expires_at });
        let code = JoinCode {
            node_id: self.me(),
            nonce,
            name: self.configured_name.clone(),
            addr: self.transport.local_hint(),
        };
        Ok(CtlResponse::JoinCode { code: code.encode(), expires_at })
    }

    fn admin_key(&self) -> Result<SecretKey, WireError> {
        self.trust
            .borrow()
            .root_key
            .map(SecretKey::from_seed)
            .ok_or_else(|| err(ErrorCode::InvalidState, "只有管理设备（执行过 nexus init 的设备）能修改名册"))
    }

    async fn approve(&self, code: &str, name: Option<String>) -> Result<MembershipReport, WireError> {
        let root = self.admin_key()?;
        let code = JoinCode::decode(code).ok_or_else(|| err(ErrorCode::InvalidInput, "加入码格式不对"))?;
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
        save(&self.system, KEY_ROSTER, &signed).map_err(internal)?;
        self.trust.borrow_mut().roster = Some(signed);
        let _ = self.learn_hints(&vec![(code.node_id, code.addr.clone())]);
        let (pushed, unreachable) = self.broadcast_roster(Some(code.node_id)).await;
        Ok(MembershipReport { name, node_id: code.node_id, roster_version: self.roster_version(), pushed, unreachable })
    }

    async fn revoke(&self, name: &str) -> Result<MembershipReport, WireError> {
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
        let (pushed, unreachable) = self.broadcast_roster(None).await;
        Ok(MembershipReport { name: name.to_string(), node_id: device.node_id, roster_version: self.roster_version(), pushed, unreachable })
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

fn agent_cert(p: &Principal) -> Option<&authz::SignedAgentCert> {
    match p {
        Principal::Device => None,
        Principal::Agent { cert, .. } => Some(cert),
    }
}
