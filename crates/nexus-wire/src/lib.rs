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

/// 节点间协议的主版本：变了才互不兼容（连接的协议名也跟着变，见 `nexus_fabric::Proto`）。
/// 规则见 docs/protocol.md。
pub const WIRE_VERSION: u16 = 19;
/// 次版本：同一主版本内只增加请求、回应和错误码，已有的编码不变（M7.1）。
pub const WIRE_MINOR: u16 = 5;
/// 单帧上限。超过即视为协议错误。
pub const MAX_FRAME: usize = 1 << 20;

/// 已知的节点地址提示：节点标识与传输层自己解释的一段文本。
pub type AddrHints = Vec<(NodeId, String)>;

/// 节点间请求。每条双向流承载一次调用。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestHeader {
    pub version: u16,
    /// 发送方的协议次版本（M7.1）。
    pub minor: u16,
    /// 发送方持有的名册版本，没有名册时为 0。双方据此发现谁的名册更旧。
    pub roster_version: u64,
    /// 以谁的身份请求：对端设备自己，或经由它的一个 agent。
    pub principal: Principal,
    /// 请求体单独编码：旧版本不认识新请求时仍能读出请求头，回应「版本较旧」而不是断开。
    body: Vec<u8>,
}

impl RequestHeader {
    pub fn new(roster_version: u64, principal: Principal, body: &Request) -> Self {
        Self {
            version: WIRE_VERSION,
            minor: WIRE_MINOR,
            roster_version,
            principal,
            body: postcard::to_allocvec(body).expect("内存中的值总能编码"),
        }
    }

    /// 解出请求体。本机不认识时返回说明版本的错误；`me` 是本机的名字和软件版本，写进说明里。
    pub fn request(&self, me: &str) -> Result<Request, WireError> {
        postcard::from_bytes(&self.body).map_err(|_| {
            WireError::new(
                ErrorCode::UnsupportedVersion,
                format!(
                    "{me} 的版本较旧（协议 {WIRE_VERSION}.{WIRE_MINOR}），不认识这个请求（请求方协议 {}.{}）：请升级 {me}",
                    self.version, self.minor
                ),
            )
        })
    }
}

/// 回应的信封（M7.1）：回应体单独编码，带回应方的次版本。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub minor: u16,
    body: Vec<u8>,
}

impl ResponseEnvelope {
    pub fn new(response: &Response) -> Self {
        Self { minor: WIRE_MINOR, body: postcard::to_allocvec(response).expect("内存中的值总能编码") }
    }

    /// 解出回应；本机不认识时是说明对方版本较新的错误。
    pub fn open(self) -> Response {
        postcard::from_bytes(&self.body).unwrap_or_else(|_| {
            Response::Error(WireError::new(
                ErrorCode::UnsupportedVersion,
                format!(
                    "对方的版本较新（协议 {WIRE_VERSION}.{}），本机（协议 {WIRE_VERSION}.{WIRE_MINOR}）不认识它的回应：请升级本机",
                    self.minor
                ),
            ))
        })
    }
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
    /// 经对方出去：对方连到 `target`（主机:端口），之后同 `SvcConnect`。对方要先开放出口。
    ExitConnect { target: String },
    /// Drive（M4.2）：对方是否是存储设备、它有哪些工作区和它们的头。
    DriveInfo,
    /// 请存储设备保存这个工作区。
    DriveRegister { ws: String },
    /// 对象：回应 `Ok`，然后按顺序每个哈希一帧 `Option<Vec<u8>>`（没有时为空）。
    DriveGet { ws: String, hashes: Vec<[u8; 32]> },
    /// 对方缺哪些对象：回应 `DriveMissing`。
    DriveHave { ws: String, hashes: Vec<[u8; 32]> },
    /// 推一个新头：回应 `Ok` 之后送 `count` 帧对象（`Vec<u8>`），最后对方回应 `DrivePushed`。
    DrivePush { ws: String, head: [u8; 32], count: u32 },
    /// 保存预读清单（同名替换）：回应 `Ok`。
    DriveProfilePut { ws: String, name: String, files: Vec<String> },
    /// 取预读清单：回应 `DriveProfile`。
    DriveProfileGet { ws: String, name: String },
    /// 网页节点硬件能力（M8.3）：列出浏览器当前已显式同意开放的硬件能力。
    HardwareList,
    /// 网页节点硬件能力（M8.3）：采集一项硬件能力（摄像头/麦克风/位置）。
    HardwareCapture { kind: HardwareKind },
    /// 临时分享（M9.1）：访客打开分享链接，校验令牌并扣减一次可用次数。
    ShareOpen { token: [u8; 16] },
    /// 临时分享（M9.1）：访客列出被分享目录下的相对子路径。
    ShareDriveList { token: [u8; 16], path: String },
    /// 临时分享（M9.1）：访客读取被分享文件（或被分享目录下的相对子路径）最多 `max` 字节。
    ShareDriveRead { token: [u8; 16], path: String, max: u64 },
    /// 临时分享（M9.1）：访客连接被分享的 HTTP 服务。
    ShareServiceConnect { token: [u8; 16] },
    /// 临时分享（M9.1）：访客只读观看被分享的终端会话。
    ShareSessionWatch { token: [u8; 16], size: Size },
    /// Vault（M9.2）：管理设备把一条加密后的秘密推送给被授权的设备（`entry: None` 表示擦除）。
    VaultPush { name: String, entry: Option<VaultCipherEntry> },
    /// Vault（M9.2）：成员设备向管理设备拉取自己被授权的所有密文条目。
    VaultPull,
    /// 根密钥轮换（M10.1）：带着旧根签署的轮换证书和新根签署的新名册推送给各设备。
    RootRotate {
        rotation: nexus_core_proto::roster::SignedRootRotation,
        roster: SignedRoster,
        addrs: AddrHints,
    },
    /// 连到对方发布的一个 UDP 服务（`--proto udp`，M11.5）。对方先回 `Response::Ok` 或 `Response::Error`；
    /// 之后两个方向都是一串数据报帧：2 字节大端长度 + 数据报。任一方向结束发送即结束这条转发。
    SvcDatagram { name: String },
    /// 经对方出口转发公网 UDP 数据报（N1）：对方先检验出口开放、授权与非本机地址，
    /// 回应 `Response::Ok` 后使用与 `SvcDatagram` 相同的 2 字节大端长度前缀数据报帧。
    ExitDatagram { target: String },
}

/// 临时分享的目标（M9.1）：全部为只读访问。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShareTarget {
    Drive { ws: String, path: String },
    Service { name: String },
    Terminal { session: String },
}

impl ShareTarget {
    pub fn kind_str(&self) -> &'static str {
        match self {
            ShareTarget::Drive { .. } => "drive",
            ShareTarget::Service { .. } => "service",
            ShareTarget::Terminal { .. } => "terminal",
        }
    }

    pub fn describe(&self, device: &str) -> String {
        match self {
            ShareTarget::Drive { ws, path } => format!("{device}/drive/{ws}:{path}"),
            ShareTarget::Service { name } => format!("{device}/svc/{name}"),
            ShareTarget::Terminal { session } => format!("{device}/session/{session}"),
        }
    }
}

/// 网页节点可发布的硬件能力类别（M8.3）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum HardwareKind {
    Camera,
    Microphone,
    Location,
}

impl HardwareKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HardwareKind::Camera => "camera",
            HardwareKind::Microphone => "microphone",
            HardwareKind::Location => "location",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "camera" => Some(HardwareKind::Camera),
            "microphone" => Some(HardwareKind::Microphone),
            "location" => Some(HardwareKind::Location),
            _ => None,
        }
    }
}

/// 访客打开临时分享后看到的元信息（M9.1）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInfo {
    pub id: String,
    pub device: String,
    pub target: ShareTarget,
    pub expires_at: Timestamp,
    pub remaining_uses: Option<u32>,
}

/// 临时分享目录里的一项（M9.1）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareDriveEntry {
    pub name: String,
    pub kind: String,
    pub size: u64,
    pub target: Option<String>,
}

/// 本机创建的一条临时分享记录（M9.1）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareRecord {
    pub id: String,
    pub target: ShareTarget,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub max_uses: Option<u32>,
    pub uses: u32,
    pub revoked: bool,
}

/// 一条临时分享在指定时刻的状态（R93）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareStatus {
    Active,
    Expired,
    Exhausted,
    Revoked,
}

impl ShareStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ShareStatus::Active => "active",
            ShareStatus::Expired => "expired",
            ShareStatus::Exhausted => "exhausted",
            ShareStatus::Revoked => "revoked",
        }
    }
}

impl ShareRecord {
    pub fn is_expired(&self, now: Timestamp) -> bool {
        now >= self.expires_at
    }

    pub fn status_at(&self, now: Timestamp) -> ShareStatus {
        if self.revoked {
            ShareStatus::Revoked
        } else if self.is_expired(now) {
            ShareStatus::Expired
        } else if self.max_uses.is_some_and(|max| self.uses >= max) {
            ShareStatus::Exhausted
        } else {
            ShareStatus::Active
        }
    }
}

/// 创建临时分享的结果（M9.1）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareCreatedReport {
    pub id: String,
    pub url: String,
    pub target: ShareTarget,
    pub expires_at: Timestamp,
    pub max_uses: Option<u32>,
}

/// 网页节点返回的一次硬件采集结果（M8.3）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HardwareCapture {
    Camera { mime: String, data: Vec<u8> },
    Microphone { mime: String, duration_ms: u32, data: Vec<u8> },
    /// 经纬度采用 1e-7 度整数（精度约 1.1 cm），精度采用厘米整数，保持确定性与 Eq。
    Location { lat_e7: i64, lon_e7: i64, accuracy_cm: u32, timestamp_ms: u64 },
}


/// Vault 密文条目（M9.2）：落盘与跨网传输均只含认证密文 `sealed`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultCipherEntry {
    pub name: String,
    pub version: u64,
    pub updated_at: Timestamp,
    pub devices: Vec<String>,
    pub sealed: Vec<u8>,
}

/// Vault 条目的摘要信息（M9.2，不含秘密明文与密文）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultEntryInfo {
    pub name: String,
    pub version: u64,
    pub updated_at: Timestamp,
    pub devices: Vec<String>,
}

/// 吊销设备后需要轮换的 Vault 条目清单项（M9.2）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultRotateItem {
    pub revoked_device: String,
    pub entries: Vec<String>,
}

/// 根密钥轮换完成后的报告（M10.1）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootRotateReport {
    pub old_root: nexus_core_proto::PublicKey,
    pub new_root: nexus_core_proto::PublicKey,
    pub recovery_code: String,
    pub roster_version: u64,
    pub pushed: u32,
    pub unreachable: Vec<String>,
}

/// 一条流承载的流量的类别。发送时交互优先：同一条连接上，批量流再满，终端和控制也先走。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TrafficClass {
    /// 终端、控制、查询。
    Interactive,
    /// 远端命令：输出可能很大，但通常短。
    Normal,
    /// 服务和出口的连接。
    Bulk,
}

impl TrafficClass {
    /// 发送优先级，越大越先发。
    pub fn priority(self) -> i32 {
        match self {
            TrafficClass::Interactive => 8,
            TrafficClass::Normal => 0,
            TrafficClass::Bulk => -8,
        }
    }
}

impl Request {
    pub fn class(&self) -> TrafficClass {
        match self {
            Request::Exec(_) => TrafficClass::Normal,
            Request::SvcConnect { .. }
            | Request::ExitConnect { .. }
            | Request::DriveGet { .. }
            | Request::DrivePush { .. }
            | Request::ShareServiceConnect { .. } => {
                TrafficClass::Bulk
            }
            Request::DriveInfo
            | Request::DriveRegister { .. }
            | Request::DriveHave { .. }
            | Request::DriveProfilePut { .. }
            | Request::DriveProfileGet { .. }
            | Request::HardwareList
            | Request::HardwareCapture { .. }
            | Request::ShareOpen { .. }
            | Request::ShareDriveList { .. }
            | Request::ShareDriveRead { .. }
            | Request::ShareSessionWatch { .. }
            | Request::VaultPush { .. }
            | Request::VaultPull
            | Request::RootRotate { .. } => TrafficClass::Interactive,
            | Request::SvcDatagram { .. } | Request::ExitDatagram { .. } => TrafficClass::Interactive,
            Request::NodeInfo
            | Request::RosterSync
            | Request::RosterPush { .. }
            | Request::JoinOffer { .. }
            | Request::WebEnroll { .. }
            | Request::WebLogin { .. }
            | Request::SessionAttach { .. }
            | Request::SessionList
            | Request::SessionKill { .. }
            | Request::SvcList => TrafficClass::Interactive,
        }
    }
}

/// Drive 的本机控制请求。路径写作 `<工作区>:/<路径>`；提交号是十六进制，可以是前缀。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriveRequest {
    /// `replicas` 为空时用网络里开了 serve 的设备。
    Create { ws: String, replicas: Vec<String> },
    /// 本机是否作为存储设备；`on` 为空时只查询。
    Serve { on: Option<bool> },
    /// 和存储设备同步：取头和目录结构，快进、推送或报告分叉。
    Sync { ws: String },
    /// 把存储设备上的头合并进来（分叉时）。
    Merge { ws: String },
    /// 数据在哪里。
    Where { path: String },
    /// 本机知道的工作区。
    List,
    /// 列目录（对文件列出它自己）；`at` 为空时看工作树。
    Ls { path: String, at: Option<String> },
    /// 读文件：回应 `Ok`，然后 `PipeData`，最后 `PipeEnd`。
    Cat { path: String, at: Option<String>, offset: u64, length: Option<u64> },
    /// 写文件：内容随后以 `Input::Stdin` 送来，`Input::CloseStdin` 结束。
    Put { path: String, exec: bool },
    Symlink { path: String, target: String },
    /// 新建空目录。
    Mkdir { path: String },
    /// 改名或移动（同一个工作区里），目标是文件时被替换。
    Rename { from: String, to: String },
    /// 设或清文件的可执行位。
    Chmod { path: String, exec: bool },
    /// 工作树的根（变了说明内容变了）。挂载用它发现同步进来的修改。
    Stamp { ws: String },
    /// 开始（`name` 有值）或结束（`None`）记录读了哪些文件。结束时得到预读清单并交给存储设备。
    Record { ws: String, name: Option<String> },
    /// 按预读清单（`all` 时整个工作区）把文件内容提前取到本机。
    Prefetch { ws: String, profile: Option<String>, all: bool },
    Rm { path: String, recursive: bool },
    /// 未提交的改动。
    Status { ws: String },
    Commit { ws: String, message: String },
    Log { ws: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriveResponse {
    Created { ws: String },
    Workspaces(Vec<WorkspaceInfo>),
    Entries { path: String, entries: Vec<DriveEntryInfo> },
    Written { path: String, size: u64 },
    Removed { path: String },
    /// Mkdir、Rename、Chmod 完成。
    Done { path: String },
    Stamp { tree: [u8; 32] },
    Recording { name: String },
    /// 记录结束：清单（按第一次读到的顺序）和保存了它的存储设备。
    Profile { name: String, files: Vec<String>, saved_to: Vec<String> },
    Prefetched(DrivePrefetch),
    Changes(Vec<DriveChange>),
    /// 提交，以及随后推给存储设备的结果。
    Committed { commit: String, sync: DriveSyncReport },
    Log(Vec<DriveCommitInfo>),
    Serving { on: bool },
    Synced(DriveSyncReport),
    Merged { commit: String, conflicts: Vec<String>, sync: DriveSyncReport },
    Where(DriveWhere),
}

/// 一次同步的结果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveSyncReport {
    /// local（没有副本）、same、pulled（快进到存储设备的头）、pushed、diverged、offline（副本都连不上）。
    pub state: String,
    pub head: Option<String>,
    /// 存储设备上的头（最近一次知道的）。
    pub remote: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveWhere {
    pub path: String,
    pub replicas: Vec<String>,
    /// 本机的头是否已经在存储设备上。
    pub synced: bool,
    /// 未提交的改动数。
    pub uncommitted: u32,
    /// 这个路径下的内容一共多少字节，本机有多少。
    pub total_bytes: u64,
    pub local_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub name: String,
    pub head: Option<String>,
    /// 未提交的改动数。
    pub changes: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrivePrefetch {
    /// 清单里（或工作区里）现在存在的文件数。
    pub files: u64,
    /// 这次从别的设备取到的字节数。
    pub fetched_bytes: u64,
    /// 这些文件的内容总共多大、现在本机有多少。
    pub total_bytes: u64,
    pub local_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveEntryInfo {
    pub name: String,
    /// file、dir 或 symlink。
    pub kind: String,
    pub size: u64,
    pub exec: bool,
    pub target: Option<String>,
    /// 文件内容的标识（清单的哈希）：内容相同就相同。
    pub content: Option<[u8; 32]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveChange {
    /// added、modified 或 deleted。
    pub kind: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveCommitInfo {
    pub commit: String,
    pub message: String,
    pub device: String,
    pub time_ms: u64,
}

/// 完整节点的虚拟网卡。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunReport {
    pub name: String,
    /// 网段，例如 198.18.0.0/16。
    pub net: String,
    /// 名字服务的地址。
    pub dns: String,
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
    /// `tcp`、`http`，远程桌面是 `vnc` 或 `rdp`（M11.1）。
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
    DriveInfo { serving: bool, workspaces: Vec<(String, Option<[u8; 32]>)> },
    DriveMissing(Vec<[u8; 32]>),
    /// 推送的结果：对方现在的头；`accepted` 为假时是分叉（或对方已经更新），要先同步。
    DrivePushed { accepted: bool, head: Option<[u8; 32]> },
    DriveProfile(Option<Vec<String>>),
    Error(WireError),
    HardwareList(Vec<HardwareKind>),
    HardwareCapture(HardwareCapture),
    ShareOpened(ShareInfo),
    ShareDriveEntries(Vec<ShareDriveEntry>),
    ShareDriveFile { size: u64, bytes: Vec<u8>, truncated: bool },
    VaultEntries(Vec<VaultCipherEntry>),
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
    /// 对方给的错误码本机不认识（对方版本较新，M7.1）。永远是最后一个：新的错误码加在它前面，它从不发出去。
    #[serde(other)]
    Unknown,
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

/// 到一台设备的连接上的一条路径（M5.1）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathInfo {
    /// 本机这一端：IP 地址，或者中继时为 `relay`。
    pub local: String,
    /// 对方：地址和端口，或者中继的地址。
    pub remote: String,
    pub kind: PathKind,
    /// 往返时间（微秒）。
    pub rtt_us: u64,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub lost_packets: u64,
    /// 这条路径经过的本机网络接口按流量计费（M5.2）。
    pub metered: bool,
}

/// 本机的一个网络接口（M5.2）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IfaceInfo {
    pub name: String,
    pub addrs: Vec<String>,
    /// 现在是否按计费网络对待。
    pub metered: bool,
    /// 自动识别的结果。
    pub auto_metered: bool,
    /// 用户的设置；`None` 时按自动识别。
    pub setting: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicePaths {
    pub device: String,
    /// 没有连接时为空。
    pub paths: Vec<PathInfo>,
}

/// 本机控制通道：命令行发给常驻程序的请求。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CtlRequest {
    /// 本机节点的状态。
    Status,
    /// 另一个节点的状态，按名字或标识指定。
    RemoteStatus { node: String },
    /// 到其他设备的路径（M5.1）；`device` 为空时列出所有设备。
    Paths { device: Option<String> },
    /// 换成新版本并重新启动（M7.3）。由常驻程序的外壳处理，不进节点。
    Upgrade { file: String, confirm_ms: u64, force: bool },
    UpgradeStatus,
    UpgradeRollback,
    /// 本机的网络接口和是否计费（M5.2）；`set` 时先改某个接口的设置（`None` 为自动识别）。
    Interfaces { set: Option<(String, Option<bool>)> },
    /// 在第一台设备上建立网络。
    Init { name: Option<String> },
    /// 用恢复码让本机成为管理设备（M6.2）。
    Recover { code: String },
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
    /// 列出网页节点当前开放的硬件能力（M8.3）。
    HardwareList { node_id: NodeId, hint: String },
    /// 从网页节点采集一项硬件能力（M8.3）。
    HardwareCapture { node_id: NodeId, hint: String, kind: HardwareKind },
    ShareCreate { target: ShareTarget, ttl_ms: u64, max_uses: Option<u32>, page: Option<String> },
    ShareList,
    ShareRevoke { id: String },
    VaultPut { name: String, value: Vec<u8>, devices: Vec<String> },
    VaultGet { name: String },
    VaultList,
    VaultRm { name: String },
    VaultSync,
    VaultRotateNeeded { device: Option<String> },
    RootRotate,
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
    /// 批量导入路由规则（N1）：导入前自动保存当前规则为 LKG 快照，并返回静态审计结果。
    RouteImport { rules: Vec<nexus_net_core::route::Rule>, replace: bool },
    /// 回滚路由规则到上一份 LKG 快照（N1）。
    RouteRollback,
    /// 对当前路由规则执行静态审计（N1）。
    RouteAudit,
    /// 新增或更新一条 DNS 改写/过滤/白名单规则（N3）。
    DnsRuleAdd { rule: nexus_net_core::dns_policy::DnsRule },
    /// 删除指定 ID 的 DNS 规则（N3）。
    DnsRuleRm { id: String },
    /// 列出全部 DNS 规则（N3）。
    DnsRuleList,
    /// 解释指定域名的 DNS 策略命中结果（N3）。
    DnsExplain { domain: String },
    /// 查询本地脱敏流事件日志（N3）。
    FlowRecent { limit: u32 },
    /// 开放或关闭本机的出口。
    ExitSet { enabled: bool },
    ExitShow,
    /// 完整节点的虚拟网卡（不是完整节点时为空）。
    TunShow,
    /// 文件（Drive）。
    Drive(DriveRequest),
    /// 连上某个节点上的终端会话。之后命令行发送 [`Input`]，常驻程序发送 `CtlResponse::Session`。
    /// `create`：会话不存在时新建。重新连上时为 false（M7.2：会话已经结束时不悄悄换成一个新的）。
    Attach { node: String, session: String, size: Size, create: bool },
    SessionList { node: String },
    SessionKill { node: String, session: String },
}

/// 最近一次升级（M7.3）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeReport {
    /// pending、confirmed、rolled_back
    pub state: String,
    pub from: String,
    pub to: String,
    pub exe: String,
    pub message: String,
    /// 这次升级的编号（换上时的毫秒时间）；没有升级过为 0。用来区分前后两次升级（M7.3b）。
    pub id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CtlResponse {
    Status(StatusReport),
    Paths(Vec<DevicePaths>),
    Upgrade(UpgradeReport),
    Interfaces(Vec<IfaceInfo>),
    Init(InitReport),
    /// 长加入码；短加入码（M6.1）在没有碰头服务时为 None，原因在 `short_code_error`。
    JoinCode { code: String, short_code: Option<String>, short_code_error: Option<String>, expires_at: Timestamp },
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
    HardwareList(Vec<HardwareKind>),
    HardwareCapture(HardwareCapture),
    ShareCreated(ShareCreatedReport),
    Shares(Vec<ShareRecord>),
    ShareRevoked(ShareRecord),
    VaultPut(VaultEntryInfo),
    VaultValue { name: String, value: Vec<u8> },
    VaultList(Vec<VaultEntryInfo>),
    VaultRotateNeeded(Vec<VaultRotateItem>),
    RootRotated(RootRotateReport),
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
    RouteImported { total: u32, audit: nexus_net_core::route::RuleAudit },
    RouteAudit(nexus_net_core::route::RuleAudit),
    DnsRules(Vec<nexus_net_core::dns_policy::DnsRule>),
    DnsExplained(nexus_net_core::dns_policy::DnsDecision),
    Flows(Vec<nexus_net_core::diag::FlowEvent>),
    Exit { enabled: bool },
    Tun(Option<TunReport>),
    Drive(DriveResponse),
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
    /// 根密钥本身：管理设备丢了时用 `nexus recover` 在另一台设备上恢复（M6.2）。只显示这一次。
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

const JOIN_CODE_PREFIX: &str = "nxj2-";

/// 长加入码的二进制形式（R63）：地址提示是十六进制时按字节存，不再编两次。
#[derive(Serialize, Deserialize)]
struct JoinCodeBytes {
    node_id: NodeId,
    nonce: [u8; 16],
    name: String,
    addr: HintBytes,
}

#[derive(Serialize, Deserialize)]
enum HintBytes {
    Hex(Vec<u8>),
    Text(String),
}

/// 末尾的校验：抄错一个字符时直接报格式不对，而不是去连一台不存在的设备。
const JOIN_CODE_CHECK: usize = 3;

fn join_check(bytes: &[u8]) -> [u8; JOIN_CODE_CHECK] {
    let h = nexus_core_proto::joincode::checksum(bytes);
    [h[0], h[1], h[2]]
}

impl JoinCode {
    /// 二进制形式，也是短加入码在碰头服务上存放的内容。
    pub fn to_bytes(&self) -> Vec<u8> {
        let addr = match hex_decode(&self.addr) {
            Some(b) if !self.addr.is_empty() && hex_encode(&b) == self.addr => HintBytes::Hex(b),
            _ => HintBytes::Text(self.addr.clone()),
        };
        let body = JoinCodeBytes { node_id: self.node_id, nonce: self.nonce, name: self.name.clone(), addr };
        let mut bytes = postcard::to_allocvec(&body).expect("内存中的值总能编码");
        let check = join_check(&bytes);
        bytes.extend_from_slice(&check);
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let (body, check) = bytes.split_at(bytes.len().checked_sub(JOIN_CODE_CHECK)?);
        if join_check(body) != check {
            return None;
        }
        let (b, rest): (JoinCodeBytes, _) = postcard::take_from_bytes(body).ok()?;
        if !rest.is_empty() {
            return None;
        }
        let addr = match b.addr {
            HintBytes::Hex(bytes) => hex_encode(&bytes),
            HintBytes::Text(t) => t,
        };
        Some(JoinCode { node_id: b.node_id, nonce: b.nonce, name: b.name, addr })
    }

    pub fn encode(&self) -> String {
        format!("{JOIN_CODE_PREFIX}{}", nexus_core_proto::joincode::base32_encode(&self.to_bytes()))
    }

    pub fn decode(s: &str) -> Option<Self> {
        let s = s.trim();
        let body = s.get(..JOIN_CODE_PREFIX.len()).filter(|p| p.eq_ignore_ascii_case(JOIN_CODE_PREFIX)).map(|_| &s[JOIN_CODE_PREFIX.len()..])?;
        Self::from_bytes(&nexus_core_proto::joincode::base32_decode(body)?)
    }

    /// 文本看起来是长加入码（而不是短加入码）。
    pub fn looks_like(s: &str) -> bool {
        s.trim().get(..4).is_some_and(|p| p.eq_ignore_ascii_case("nxj2") || p.eq_ignore_ascii_case("nxj1"))
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
        bytes.extend(encode(&RequestHeader::new(0, nexus_core_proto::authz::Principal::Device, &Request::NodeInfo)).unwrap());
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
        assert!(text.starts_with("nxj2-"));
        assert_eq!(JoinCode::decode(&text), Some(code.clone()));
        assert_eq!(JoinCode::decode(&format!("  {text}\n")), Some(code.clone()));
        assert_eq!(JoinCode::decode(&text.to_uppercase()), Some(code));
        for bad in ["", "nxj2-", "nxj2-zz", "nxj2-abc", "xxxx-00", "nxj1-00", &text[..text.len() - 2]] {
            assert_eq!(JoinCode::decode(bad), None, "{bad:?}");
        }
    }

    /// FP-6.1-06：只有小写字母和数字；不超过旧格式的 60%；改一个字符要么解不开、要么内容不同。
    #[test]
    fn join_code_is_compact() {
        // 地址提示的样子和 iroh 驱动给的一样：一段十六进制（端点地址：身份、中继、两个直连地址）。
        let hint = hex_encode(&(0..110u8).collect::<Vec<_>>());
        let code = JoinCode { node_id: NodeId([0xab; 32]), nonce: [7; 16], name: "office-pc".into(), addr: hint.clone() };
        let text = code.encode();
        assert!(text[5..].bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()), "{text}");
        let old = format!("nxj1-{}", hex_encode(&postcard::to_allocvec(&code).unwrap()));
        assert!(text.len() * 10 <= old.len() * 6, "新 {} 个字符，旧 {} 个", text.len(), old.len());
        assert_eq!(JoinCode::decode(&text).unwrap().addr, hint);
        // 不是十六进制的提示（仿真里的主机名）原样保留。
        let sim = JoinCode { addr: "home".into(), ..code.clone() };
        assert_eq!(JoinCode::decode(&sim.encode()), Some(sim));
        for i in 5..text.len() {
            let mut b = text.clone().into_bytes();
            b[i] = if b[i] == b'a' { b'b' } else { b'a' };
            let changed = String::from_utf8(b).unwrap();
            assert_ne!(JoinCode::decode(&changed), Some(code.clone()), "改第 {i} 个字符");
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

    // covers: R93
    #[test]
    fn share_record_status_and_precedence() {
        let make = |expires_at: u64, max_uses: Option<u32>, uses: u32, revoked: bool| ShareRecord {
            id: "abcd1234".into(),
            target: ShareTarget::Service { name: "web".into() },
            created_at: Timestamp(1000),
            expires_at: Timestamp(expires_at),
            max_uses,
            uses,
            revoked,
        };

        let active = make(2000, Some(3), 1, false);
        assert!(!active.is_expired(Timestamp(1999)));
        assert_eq!(active.status_at(Timestamp(1999)), ShareStatus::Active);
        assert_eq!(ShareStatus::Active.as_str(), "active");

        assert!(active.is_expired(Timestamp(2000)));
        assert_eq!(active.status_at(Timestamp(2000)), ShareStatus::Expired);
        assert_eq!(ShareStatus::Expired.as_str(), "expired");

        let exhausted = make(2000, Some(2), 2, false);
        assert!(!exhausted.is_expired(Timestamp(1500)));
        assert_eq!(exhausted.status_at(Timestamp(1500)), ShareStatus::Exhausted);
        assert_eq!(ShareStatus::Exhausted.as_str(), "exhausted");

        let unlimited = make(2000, None, 999, false);
        assert_eq!(unlimited.status_at(Timestamp(1500)), ShareStatus::Active);

        let revoked_only = make(2000, Some(3), 0, true);
        assert!(!revoked_only.is_expired(Timestamp(1500)));
        assert_eq!(revoked_only.status_at(Timestamp(1500)), ShareStatus::Revoked);
        assert_eq!(ShareStatus::Revoked.as_str(), "revoked");

        let expired_and_exhausted = make(2000, Some(1), 1, false);
        assert!(expired_and_exhausted.is_expired(Timestamp(2000)));
        assert_eq!(expired_and_exhausted.status_at(Timestamp(2000)), ShareStatus::Expired);

        let revoked_and_expired = make(2000, Some(1), 0, true);
        assert!(revoked_and_expired.is_expired(Timestamp(2500)));
        assert_eq!(revoked_and_expired.status_at(Timestamp(2500)), ShareStatus::Revoked);

        let revoked_expired_exhausted = make(2000, Some(1), 1, true);
        assert!(revoked_expired_exhausted.is_expired(Timestamp(2500)));
        assert_eq!(revoked_expired_exhausted.status_at(Timestamp(2500)), ShareStatus::Revoked);
    }
}
