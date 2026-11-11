//! 运行时：把纯逻辑接到 `Transport` 和 `System` 两个接缝上。
//!
//! 本 crate 不直接引用任何具体的传输或系统实现，也不自行创建任务：
//! 所有并发都在 `Node::run` 这一个 future 里完成，因此同一份代码既能跑在
//! 真实的异步运行时上，也能跑在单线程的仿真里。
#![forbid(unsafe_code)]

use futures_util::future::join_all;
use futures_util::stream::{FuturesUnordered, StreamExt};
use nexus_core_proto::authz::{self, Action, Context, Outcome, Principal};
use nexus_core_proto::path::{valid_segment, Capability, ResourcePath};
use nexus_core_proto::roster::{accept_update, Device, DeviceRole, Roster, SignedRoster};
use nexus_core_proto::{NodeId, NodeInfo, PublicKey, SecretKey, Timestamp};
use nexus_fabric::{read_msg, write_msg, Connection, Proto, Transport};
use nexus_sys::System;
use nexus_wire::{
    hex_encode, AddrHints, CtlRequest, CtlResponse, ErrorCode, InitReport, JoinCode, ListEntry,
    MembershipReport, Request, RequestHeader, Response, StatusReport, WireError, WIRE_VERSION,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

/// 一次本机控制调用：请求，以及回传结果的通道。
pub type CtlCall = (CtlRequest, oneshot::Sender<CtlResponse>);

/// 等待对端响应的上限。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// 加入码的有效期。
pub const JOIN_CODE_TTL_MS: u64 = 10 * 60 * 1000;

const KEY_DEVICE: &str = "device_key";
const KEY_ROOT_PUB: &str = "root_pub";
const KEY_ROOT_KEY: &str = "root_key";
const KEY_ROSTER: &str = "roster";
const KEY_ADDRS: &str = "addrs";

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

pub struct Node<T, S> {
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
        Ok(Self {
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
        let mut tasks: FuturesUnordered<Task<'_>> = FuturesUnordered::new();
        // accept 不保证可以安全取消，所以让同一个 future 跨循环存活。
        let mut accept = Box::pin(self.transport.accept());
        loop {
            tokio::select! {
                conn = &mut accept => {
                    let (conn, proto) = conn?;
                    accept = Box::pin(self.transport.accept());
                    tasks.push(Box::pin(self.serve_conn(conn, proto)));
                }
                Some((req, reply)) = ctl.recv() => {
                    tasks.push(Box::pin(async move {
                        let _ = reply.send(self.handle_ctl(req).await);
                    }));
                }
                Some(peer) = pushes.recv() => {
                    tasks.push(Box::pin(async move {
                        let _ = tokio::time::timeout(REQUEST_TIMEOUT, self.push_roster(peer)).await;
                    }));
                }
                Some(()) = tasks.next(), if !tasks.is_empty() => {}
            }
        }
    }

    async fn serve_conn(&self, conn: T::Conn, proto: Proto) {
        let peer = conn.remote();
        while let Ok(Some(mut stream)) = conn.accept_bi().await {
            let response = match read_msg::<_, RequestHeader>(&mut stream).await {
                Ok(header) => match proto {
                    Proto::Rpc => self.handle_rpc(peer, header),
                    Proto::Join => self.handle_join(header),
                },
                Err(_) => return,
            };
            if write_msg(&mut stream, &response).await.is_err() {
                return;
            }
            let _ = stream.shutdown().await;
            drain(&mut stream).await;
        }
    }

    fn handle_rpc(&self, peer: NodeId, header: RequestHeader) -> Response {
        if header.version != WIRE_VERSION {
            return Response::Error(err(ErrorCode::UnsupportedVersion, format!("不支持的协议版本 {}", header.version)));
        }
        // 名册推送只靠签名证明自己，不要求发送方已经在本机的旧名册里。
        if let Request::RosterPush { roster, addrs } = &header.body {
            return match self.adopt_roster(roster) {
                Ok(()) => {
                    let _ = self.learn_hints(addrs);
                    Response::Ok
                }
                Err(e) if matches!(e.code, ErrorCode::InvalidInput) => Response::Ok, // 不比本机新：无需更新
                Err(e) => Response::Error(e),
            };
        }
        let mine = self.roster_version();
        if header.roster_version > mine {
            return Response::Error(err(ErrorCode::StaleRoster, format!("本机名册版本 {mine} 较旧")));
        }
        let Some(roster) = self.roster() else {
            return Response::Error(err(ErrorCode::Denied, "本机还没有加入任何网络"));
        };
        let me = roster.device(self.me()).map(|d| d.name.clone()).unwrap_or_default();
        let capability = match header.body {
            Request::NodeInfo => "node.info",
            Request::RosterSync => "roster.sync",
            Request::RosterPush { .. } | Request::JoinOffer { .. } => {
                return Response::Error(err(ErrorCode::InvalidInput, "这个请求不能在此协议上使用"))
            }
        };
        let action = Action {
            capability: Capability::parse(capability).expect("内置能力名合法"),
            resource: match ResourcePath::parse(&me) {
                Ok(r) => r,
                Err(_) => return Response::Error(err(ErrorCode::Internal, "本机不在名册里")),
            },
        };
        let ctx = Context { roster: &roster, me: self.me(), now: self.system.now() };
        let verdict = authz::decide(&ctx, peer, &Principal::Device, &action);
        if let Outcome::Deny(reason) = verdict.outcome {
            return Response::Error(err(ErrorCode::Denied, format!("访问被拒绝：{reason:?}")));
        }
        if mine > header.roster_version {
            // 对方的名册旧了，稍后在后台推过去。
            let _ = self.push_tx.send(peer);
        }
        match header.body {
            Request::NodeInfo => Response::NodeInfo(self.info()),
            _ => Response::Roster {
                roster: (mine > header.roster_version).then(|| self.trust.borrow().roster.clone()).flatten(),
                addrs: self.addr_hints(),
            },
        }
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
    async fn request(&self, to: NodeId, body: Request) -> io::Result<(Response, Option<nexus_core_proto::PathKind>)> {
        let (resp, path) = self.request_once(to, Proto::Rpc, body.clone()).await?;
        if let Response::Error(WireError { code: ErrorCode::StaleRoster, .. }) = resp {
            self.push_roster(to).await?;
            return self.request_once(to, Proto::Rpc, body).await;
        }
        Ok((resp, path))
    }

    async fn request_once(
        &self,
        to: NodeId,
        proto: Proto,
        body: Request,
    ) -> io::Result<(Response, Option<nexus_core_proto::PathKind>)> {
        let conn = self.transport.connect(to, proto).await?;
        let mut stream = conn.open_bi().await?;
        let header = RequestHeader { version: WIRE_VERSION, roster_version: self.roster_version(), body };
        write_msg(&mut stream, &header).await?;
        stream.shutdown().await?;
        let resp = read_msg(&mut stream).await?;
        drain(&mut stream).await;
        Ok((resp, conn.path_kind()))
    }

    async fn push_roster(&self, to: NodeId) -> io::Result<()> {
        let Some(roster) = self.trust.borrow().roster.clone() else { return Ok(()) };
        let body = Request::RosterPush { roster, addrs: self.addr_hints() };
        match self.request_once(to, Proto::Rpc, body).await?.0 {
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

    /// 处理一次本机控制调用。
    pub async fn handle_ctl(&self, req: CtlRequest) -> CtlResponse {
        let result = match req {
            CtlRequest::Status => Ok(CtlResponse::Status(self.local_status())),
            CtlRequest::RemoteStatus { node } => self.remote_status(&node).await.map(CtlResponse::Status),
            CtlRequest::Init { name } => self.init(name).map(CtlResponse::Init),
            CtlRequest::JoinStart => self.join_start(),
            CtlRequest::Approve { code, name } => self.approve(&code, name).await.map(CtlResponse::Membership),
            CtlRequest::Revoke { name } => self.revoke(&name).await.map(CtlResponse::Membership),
            CtlRequest::List => Ok(CtlResponse::List(self.list().await)),
        };
        result.unwrap_or_else(CtlResponse::Error)
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
        let reply = tokio::time::timeout(REQUEST_TIMEOUT * 2, self.request_once(code.node_id, Proto::Join, offer)).await;
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
        let reply = tokio::time::timeout(REQUEST_TIMEOUT, self.request(device.node_id, Request::NodeInfo)).await;
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

    async fn remote_status(&self, name_or_id: &str) -> Result<StatusReport, WireError> {
        let device = self.resolve(name_or_id)?;
        if device.node_id == self.me() {
            return Ok(self.local_status());
        }
        self.probe(&device).await
    }

    async fn list(&self) -> Vec<ListEntry> {
        let Some(roster) = self.roster() else {
            return vec![ListEntry { report: self.local_status(), is_self: true }];
        };
        let reports = join_all(roster.devices.iter().map(|d| async move {
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
