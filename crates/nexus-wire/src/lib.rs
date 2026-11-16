//! 纯逻辑层：线上消息与帧格式。
//!
//! 帧 = 4 字节小端长度 + postcard 编码的消息。本 crate 只做编解码，不做 IO。
#![forbid(unsafe_code)]

use nexus_core_proto::authz::{AuditEvent, Principal, SignedAgentCert};
use nexus_core_proto::roster::{DeviceRole, SignedRoster};
use nexus_core_proto::{NodeId, NodeInfo, PathKind, Timestamp};
pub use nexus_term_proto::{ClientMsg, Size};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// 节点间协议版本。
pub const WIRE_VERSION: u16 = 10;
/// 单帧上限。超过即视为协议错误。
pub const MAX_FRAME: usize = 1 << 20;

/// 已知的节点地址提示：节点标识与传输层自己解释的一段文本。
pub type AddrHints = Vec<(NodeId, String)>;

/// 节点间请求。每条双向流承载一次调用。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestHeader {
    pub version: u16,
    /// 发送方持有的名册版本，没有名册时为 0。双方据此发现谁的名册更旧。
    pub roster_version: u64,
    /// 以谁的身份请求：对端设备自己，或经由它的一个 agent。
    pub principal: Principal,
    pub body: Request,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    NodeInfo,
    /// 请求对方在名册比我新时把名册发过来。
    RosterSync,
    /// 把一份名册推给对方。
    RosterPush { roster: SignedRoster, addrs: AddrHints },
    /// 管理设备向正在等待加入的设备发出的邀请（只在加入协议上使用）。
    JoinOffer { nonce: [u8; 16], roster: SignedRoster, addrs: AddrHints },
    /// 执行一条命令。对方先回 `Response::Ok` 或 `Response::Error`；之后调用方
    /// 发送 [`Input`]，对方发送 [`ExecEvent`]，直到 `ExecEvent::Exit`。
    Exec(ExecSpec),
    /// 网页节点（协议 nexus/web）：用一次性登记码登记一个新建的通行密钥。
    WebEnroll { token: [u8; 16], credential: NewCredential },
    /// 网页节点：用通行密钥登录。挑战是 `login_challenge(issued_at, 浏览器的临时节点标识)`。
    WebLogin { issued_at: Timestamp, assertion: nexus_core_proto::webauthn::WebAuthnAssertion },
    /// 连上一个终端会话，不存在且 `create` 时创建。对方先回 `Response::Ok` 或 `Response::Error`；
    /// 之后调用方发送 [`Input`]（`Stdin`、`Resize`、`Ack`、`Detach`），对方发送 [`ClientMsg`]。
    SessionAttach { name: String, create: bool, size: Size },
    /// 列出对方的终端会话。
    SessionList,
    /// 结束对方的一个终端会话。
    SessionKill { name: String },
    /// 连到对方发布的一个服务。对方先回 `Response::Ok` 或 `Response::Error`；
    /// 之后这条流就是到那个服务的字节流，任一方向结束发送即关闭那个方向。
    SvcConnect { name: String },
    /// 列出对方发布的、调用者能用的服务。
    SvcList,
}

/// `nexus route explain` 的结果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouteExplanation {
    /// 网络里的服务。
    Service { device: String, service: String },
    /// 以 .internal 结尾但不是「服务.设备.internal」。
    BadInternalName,
    /// 命中第 `index` 条规则（从 1 开始）。
    Rule { index: u32, rule: nexus_net_core::route::Rule },
    /// 没有规则匹配。
    Default { outbound: nexus_net_core::route::Outbound },
}

/// 一个发布的服务，给使用方看的部分（不含发布方的本机地址）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceInfo {
    pub name: String,
    /// `tcp` 或 `http`。
    pub proto: String,
    /// 使用方需要的环境变量，值里的 `{addr}` 由使用方换成它访问服务用的地址。
    pub env: Vec<(String, String)>,
}

/// 浏览器新建的通行密钥（`navigator.credentials.create` 的结果）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewCredential {
    pub credential_id: Vec<u8>,
    /// `getPublicKey()` 给出的 SubjectPublicKeyInfo（DER）。
    pub spki: Vec<u8>,
    /// 创建时页面所在的依赖方（域名）。
    pub rp_id: String,
}

/// 网页登录能看到的一台设备，带着连接它所需的地址提示。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebDevice {
    pub name: String,
    pub node_id: NodeId,
    pub hint: String,
}

/// 一个终端会话的概况。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub name: String,
    pub clients: u32,
    pub created_at: Timestamp,
    pub size: Size,
}

/// 要执行的命令。不经过 shell：`program` 直接作为可执行文件启动。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecSpec {
    pub program: String,
    pub args: Vec<String>,
    /// 工作目录；`None` 时使用常驻程序所在用户的主目录。
    pub cwd: Option<String>,
    /// 追加的环境变量。
    pub env: Vec<(String, String)>,
    /// 超过这个时长就终止命令；`None` 表示不限。
    pub timeout_ms: Option<u64>,
    /// 标准输出与标准错误合计的上限，超过就终止命令。
    pub max_output: u64,
}

/// 调用方在一次执行中发出的消息。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Input {
    Stdin(Vec<u8>),
    /// 标准输入结束。
    CloseStdin,
    /// 调用方不再需要结果，终止命令。
    Cancel,
    /// 终端会话：客户端的终端尺寸变了。
    Resize(Size),
    /// 终端会话：客户端已经处理完这么多字节的输出。
    Ack(u64),
    /// 终端会话：客户端离开，会话继续运行。
    Detach,
    /// 终端会话：客户端还在。长时间收不到任何消息，会话一侧就认为这个客户端已经失联。
    Ping,
}

/// 终端会话流上，会话一侧发给客户端的帧。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionFrame {
    Msg(ClientMsg),
    /// 会话还在。客户端长时间收不到任何帧，就认为连接已经失效，重新连接。
    Heartbeat,
}

/// 执行过程中产生的事件。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(ExecResult),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecResult {
    /// 正常退出时的退出码；被信号终止时为 `None`。
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    /// 因超时被终止。
    pub timed_out: bool,
    /// 因输出超过上限被终止；已经送出的输出恰好是上限那么多。
    pub truncated: bool,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    NodeInfo(NodeInfo),
    Roster { roster: Option<SignedRoster>, addrs: AddrHints },
    Ok,
    Sessions(Vec<SessionInfo>),
    Services(Vec<ServiceInfo>),
    /// 通行密钥登记成功，带上它的名字。
    WebEnrolled { label: String },
    /// 登录成功：网页会话凭证，以及这个通行密钥能看到的设备。
    WebLoggedIn { session: nexus_core_proto::authz::SignedWebSession, devices: Vec<WebDevice> },
    Error(WireError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    UnsupportedVersion,
    UnknownNode,
    Unreachable,
    Internal,
    /// 对方拒绝了这次访问。
    Denied,
    /// 对方的名册比请求方旧；请求方应先把名册推过去再重试。
    StaleRoster,
    /// 当前状态不允许这个操作，例如尚未初始化、已经加入、不是管理设备。
    InvalidState,
    InvalidInput,
    /// 命令无法启动，例如可执行文件不存在。
    SpawnFailed,
    /// 要操作的对象不存在，例如没有这个名字的会话。
    NotFound,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub code: ErrorCode,
    pub message: String,
}

impl WireError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

/// 本机控制通道：命令行发给常驻程序的请求。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CtlRequest {
    /// 本机节点的状态。
    Status,
    /// 另一个节点的状态，按名字或标识指定。
    RemoteStatus { node: String },
    /// 在第一台设备上建立网络。
    Init { name: Option<String> },
    /// 准备加入一个网络，返回加入码。
    JoinStart,
    /// 批准一台设备加入。
    Approve { code: String, name: Option<String> },
    /// 把一台设备移出名册。
    Revoke { name: String },
    /// 列出名册里的所有设备。
    List,
    /// 在某个节点上执行命令。之后命令行发送 [`Input`]，常驻程序发送 `CtlResponse::Exec`。
    Exec { node: String, spec: ExecSpec },
    /// 签发一份 agent 凭据（只能在管理设备上）。授权条目写作 `能力@资源`。
    AgentIssue { label: String, grants: Vec<String>, ttl_ms: u64 },
    /// 撤销一个 agent，按名字或标识指定。
    AgentRevoke { agent: String },
    AgentList,
    /// 本机的审计记录，最新的在后。
    Audit { limit: Option<u32> },
    /// 本机 SSH 呈现层为某个设备使用的主机密钥（种子）。对同一台目标设备总是相同。
    SshHostKey { node: String },
    /// 发起通行密钥登记（只能在管理设备上）：`page` 是页面地址，它的主机名就是通行密钥的依赖方。
    /// `page` 为空时用名册里的锚点，没有锚点时用默认页面。
    WebEnroll { label: String, grants: Vec<String>, ttl_ms: u64, page: Option<String> },
    WebList,
    WebRevoke { label: String },
    /// 设置（`None` 为清除）名册里的锚点。只能在管理设备上。
    AnchorSet { url: Option<String> },
    AnchorShow,
    /// 在本机发布一个服务：本机地址只留在本机。
    Publish { name: String, addr: core::net::SocketAddr, proto: String, env: Vec<(String, String)> },
    Unpublish { name: String },
    /// 列出一台设备上调用者能用的服务。
    Services { node: String },
    /// 连到一台设备上的服务。回应 `Ok` 之后，调用方发送 `Input::Stdin` / `Input::CloseStdin`，
    /// 常驻程序发送 `PipeData`，服务结束发送时发送 `PipeEnd`。
    Connect { node: String, name: String },
    /// 路由规则：加在最后。
    RouteAdd { rule: nexus_net_core::route::Rule },
    /// 删除第 `index` 条（从 1 开始）。
    RouteRm { index: u32 },
    RouteList,
    /// 说明 `target`（主机:端口）会走哪里。
    RouteExplain { target: String },
    /// 连上某个节点上的终端会话。之后命令行发送 [`Input`]，常驻程序发送 `CtlResponse::Session`。
    Attach { node: String, session: String, size: Size },
    SessionList { node: String },
    SessionKill { node: String, session: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CtlResponse {
    Status(StatusReport),
    Init(InitReport),
    JoinCode { code: String, expires_at: Timestamp },
    Membership(MembershipReport),
    List(Vec<ListEntry>),
    Exec(ExecEvent),
    Session(ClientMsg),
    Sessions(Vec<SessionInfo>),
    AgentIssued(AgentIssueReport),
    AgentRevoked(MembershipReport),
    Agents(Vec<AgentInfo>),
    Audit(Vec<AuditRecord>),
    SshHostKey([u8; 32]),
    WebEnroll(WebEnrollReport),
    WebKeys(Vec<WebKeyInfo>),
    WebRevoked(MembershipReport),
    Anchor(AnchorReport),
    /// 发布成功，带上服务的路径（例如 `home/web`）。
    Published { path: String },
    Services { node: String, services: Vec<ServiceInfo> },
    PipeData(Vec<u8>),
    PipeEnd,
    RouteAdded { index: u32 },
    Routes(Vec<nexus_net_core::route::Rule>),
    Explained(RouteExplanation),
    Ok,
    Error(WireError),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    /// 在线时是对端的自述；离线时只有本机已知的名字和标识。
    pub name: String,
    pub node_id: NodeId,
    pub info: Option<NodeInfo>,
    pub online: bool,
    pub rtt_ms: Option<u64>,
    pub path: Option<PathKind>,
    /// 这台设备在本机名册中的角色；不在名册中时为 `None`。
    pub role: Option<DeviceRole>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListEntry {
    pub report: StatusReport,
    pub is_self: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitReport {
    pub name: String,
    pub node_id: NodeId,
    pub root: nexus_core_proto::PublicKey,
    /// 根密钥本身，用于在所有设备都丢失后恢复。只显示这一次。
    pub recovery_code: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipReport {
    pub name: String,
    pub node_id: NodeId,
    pub roster_version: u64,
    /// 新名册已经送达的其他设备数。
    pub pushed: u32,
    /// 暂时没能送达的设备名；它们会在下次与任一已更新的设备通信时得到新名册。
    pub unreachable: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorReport {
    pub anchor: Option<String>,
    pub roster_version: u64,
    /// 设置时：新名册已经送达的其他设备数和暂时没能送达的设备名。查看时为 0 和空。
    pub pushed: u32,
    pub unreachable: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebEnrollReport {
    pub label: String,
    /// 在要用的设备上打开这个链接完成登记。
    pub url: String,
    pub expires_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebKeyInfo {
    pub label: String,
    pub rp_id: String,
    pub grants: Vec<String>,
    pub added_at: Timestamp,
}

/// 命令行打开控制会话时发送的第一条消息。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CtlHello {
    /// 以这个 agent 的身份发出请求；`None` 表示以本机设备（用户本人）的身份。
    pub agent: Option<AgentCredential>,
    pub request: CtlRequest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentIssueReport {
    pub agent_id: nexus_core_proto::PublicKey,
    pub label: String,
    pub not_after: Timestamp,
    /// 凭据文本（`nxa1-` 开头），含 agent 的私钥。
    pub credential: String,
}

/// 管理设备记录的一个已签发的 agent。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub agent_id: nexus_core_proto::PublicKey,
    pub label: String,
    pub issued_at: Timestamp,
    pub not_after: Timestamp,
    pub grants: Vec<String>,
    pub revoked: bool,
}

/// 一条审计记录，附上经由的设备在记录时的名字。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub event: AuditEvent,
    pub peer_name: String,
}

/// agent 凭据：管理设备签发的证书，加上 agent 自己的私钥。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCredential {
    pub cert: SignedAgentCert,
    pub secret: [u8; 32],
}

const CREDENTIAL_PREFIX: &str = "nxa1-";

impl AgentCredential {
    pub fn encode(&self) -> String {
        let bytes = postcard::to_allocvec(self).expect("内存中的值总能编码");
        format!("{CREDENTIAL_PREFIX}{}", hex_encode(&bytes))
    }

    pub fn decode(s: &str) -> Option<Self> {
        let body = s.trim().strip_prefix(CREDENTIAL_PREFIX)?;
        postcard::from_bytes(&hex_decode(body)?).ok()
    }

    /// 文本看起来是一份凭据（而不是凭据文件的路径）。
    pub fn looks_like(s: &str) -> bool {
        s.trim_start().starts_with(CREDENTIAL_PREFIX)
    }
}

/// 加入码：新设备显示，管理设备输入。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinCode {
    pub node_id: NodeId,
    /// 一次性随机数。只有看到加入码的人才能让这台设备接受邀请。
    pub nonce: [u8; 16],
    /// 新设备建议的名字。
    pub name: String,
    /// 新设备的地址提示。
    pub addr: String,
}

const JOIN_CODE_PREFIX: &str = "nxj1-";

impl JoinCode {
    pub fn encode(&self) -> String {
        let bytes = postcard::to_allocvec(self).expect("内存中的值总能编码");
        format!("{JOIN_CODE_PREFIX}{}", hex_encode(&bytes))
    }

    pub fn decode(s: &str) -> Option<Self> {
        let body = s.trim().strip_prefix(JOIN_CODE_PREFIX)?;
        postcard::from_bytes(&hex_decode(body)?).ok()
    }
}

pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    TooLarge(usize),
    Malformed,
}

impl core::fmt::Display for FrameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FrameError::TooLarge(n) => write!(f, "帧长度 {n} 超过上限 {MAX_FRAME}"),
            FrameError::Malformed => f.write_str("帧内容无法解码"),
        }
    }
}

impl std::error::Error for FrameError {}

/// 把一条消息编码成一帧。
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let body = postcard::to_allocvec(msg).map_err(|_| FrameError::Malformed)?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// 解析帧头，返回帧体长度。
pub fn body_len(header: [u8; 4]) -> Result<usize, FrameError> {
    let n = u32::from_le_bytes(header) as usize;
    if n > MAX_FRAME {
        return Err(FrameError::TooLarge(n));
    }
    Ok(n)
}

/// 解码帧体。
pub fn decode_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, FrameError> {
    postcard::from_bytes(body).map_err(|_| FrameError::Malformed)
}

/// 增量解码器：可以分多次喂入字节。
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// 缓冲里有完整的一帧时取出并解码，否则返回 `None`。
    pub fn next_frame<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FrameError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let n = body_len([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])?;
        if self.buf.len() < 4 + n {
            return Ok(None);
        }
        let msg = decode_body(&self.buf[4..4 + n])?;
        self.buf.drain(..4 + n);
        Ok(Some(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core_proto::{NodeId, Timestamp};

    fn sample() -> Response {
        Response::NodeInfo(NodeInfo {
            node_id: NodeId([7; 32]),
            name: "home".into(),
            version: "0.0.1".into(),
            started_at: Timestamp(1_700_000_000_000),
            now: Timestamp(1_700_000_000_500),
            roster_version: Some(3),
        })
    }

    #[test]
    fn frame_round_trip() {
        let frame = encode(&sample()).unwrap();
        let mut dec = FrameDecoder::default();
        dec.push(&frame);
        assert_eq!(dec.next_frame::<Response>().unwrap(), Some(sample()));
        assert_eq!(dec.next_frame::<Response>().unwrap(), None);
    }

    #[test]
    fn decoder_handles_byte_at_a_time_and_back_to_back_frames() {
        let mut bytes = encode(&sample()).unwrap();
        bytes.extend(encode(&RequestHeader { version: WIRE_VERSION, roster_version: 0, principal: nexus_core_proto::authz::Principal::Device, body: Request::NodeInfo }).unwrap());
        let mut dec = FrameDecoder::default();
        let mut got_first = false;
        let mut got_second = false;
        for b in bytes {
            dec.push(&[b]);
            if !got_first {
                got_first = dec.next_frame::<Response>().unwrap().is_some();
            } else if !got_second {
                got_second = dec.next_frame::<RequestHeader>().unwrap().is_some();
            }
        }
        assert!(got_first && got_second);
    }

    #[test]
    fn join_code_round_trip_and_rejects_garbage() {
        let code = JoinCode { node_id: NodeId([5; 32]), nonce: [6; 16], name: "home".into(), addr: "x".into() };
        let text = code.encode();
        assert!(text.starts_with("nxj1-"));
        assert_eq!(JoinCode::decode(&text), Some(code.clone()));
        assert_eq!(JoinCode::decode(&format!("  {text}\n")), Some(code));
        for bad in ["", "nxj1-", "nxj1-zz", "nxj1-abc", "xxxx-00", &text[..text.len() - 2]] {
            assert_eq!(JoinCode::decode(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn hex_round_trip() {
        let data: Vec<u8> = (0..=255).collect();
        assert_eq!(hex_decode(&hex_encode(&data)), Some(data));
        assert_eq!(hex_decode("0"), None);
        assert_eq!(hex_decode("g0"), None);
    }

    #[test]
    fn oversized_and_garbage_frames_are_errors_not_panics() {
        let mut dec = FrameDecoder::default();
        dec.push(&u32::MAX.to_le_bytes());
        assert!(matches!(dec.next_frame::<Response>(), Err(FrameError::TooLarge(_))));

        let mut dec = FrameDecoder::default();
        dec.push(&3u32.to_le_bytes());
        dec.push(&[0xff, 0xff, 0xff]);
        assert_eq!(dec.next_frame::<Response>(), Err(FrameError::Malformed));
    }
}
