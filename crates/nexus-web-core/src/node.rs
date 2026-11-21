//! 登录之后的网页节点：带着网页会话凭证直接访问各设备。

use crate::{call, unreachable};
use nexus_core_proto::authz::{Principal, SignedWebSession};
use nexus_core_proto::webauthn::WebAuthnAssertion;
use nexus_core_proto::{NodeId, NodeInfo, Timestamp};
use nexus_fabric::{read_msg, read_response, write_msg, write_response, Connection, FrameReader, Proto, Transport};
use nexus_wire::{
    ErrorCode, Input, Request, RequestHeader, Response, SessionFrame, SessionInfo, Size, WebDevice, WireError, WIRE_VERSION,
};
use tokio::io::{ReadHalf, WriteHalf};

/// 浏览器这次要签名的挑战。
pub fn challenge<T: Transport>(transport: &T, issued_at: Timestamp) -> [u8; 32] {
    nexus_core_proto::webauthn::login_challenge(issued_at, transport.node_id())
}

pub struct WebNode<T> {
    transport: T,
    session: SignedWebSession,
    devices: Vec<WebDevice>,
}

impl<T: Transport> WebNode<T> {
    /// 向入口设备登录。调用前应当已经给 `transport` 加上入口设备的地址提示。
    pub async fn login(transport: T, entry: NodeId, issued_at: Timestamp, assertion: WebAuthnAssertion) -> Result<Self, WireError> {
        let body = Request::WebLogin { issued_at, assertion };
        match call(&transport, entry, Proto::Web, Principal::Device, body).await? {
            Response::WebLoggedIn { session, devices } => {
                for d in &devices {
                    transport.add_hint(d.node_id, &d.hint);
                }
                Ok(Self { transport, session, devices })
            }
            Response::Error(e) => Err(e),
            other => Err(WireError::new(ErrorCode::Internal, format!("入口设备的回应无法理解：{other:?}"))),
        }
    }

    fn principal(&self) -> Principal {
        Principal::Web { session: self.session.clone() }
    }

    fn device(&self, name: &str) -> Result<NodeId, WireError> {
        self.devices
            .iter()
            .find(|d| d.name == name)
            .map(|d| d.node_id)
            .ok_or_else(|| WireError::new(ErrorCode::UnknownNode, format!("不认识设备 {name}")))
    }

    async fn rpc(&self, device: &str, body: Request) -> Result<Response, WireError> {
        let to = self.device(device)?;
        match call(&self.transport, to, Proto::Rpc, self.principal(), body).await? {
            Response::Error(e) => Err(e),
            r => Ok(r),
        }
    }

    /// 通行密钥在网络里的名字。
    pub fn label(&self) -> &str {
        &self.session.session.label
    }

    /// 这次登录的有效期截止时间。
    pub fn expires_at(&self) -> Timestamp {
        self.session.session.not_after
    }

    /// 能看到的设备。
    pub fn devices(&self) -> &[WebDevice] {
        &self.devices
    }

    pub async fn status(&self, device: &str) -> Result<NodeInfo, WireError> {
        match self.rpc(device, Request::NodeInfo).await? {
            Response::NodeInfo(info) => Ok(info),
            other => Err(WireError::new(ErrorCode::Internal, format!("回应无法理解：{other:?}"))),
        }
    }

    pub async fn session_list(&self, device: &str) -> Result<Vec<SessionInfo>, WireError> {
        match self.rpc(device, Request::SessionList).await? {
            Response::Sessions(list) => Ok(list),
            other => Err(WireError::new(ErrorCode::Internal, format!("回应无法理解：{other:?}"))),
        }
    }

    /// 连上（必要时创建）一台设备上的终端会话。
    pub async fn attach(&self, device: &str, session: &str, size: Size) -> Result<WebTerminal<<T::Conn as Connection>::Stream>, WireError> {
        let to = self.device(device)?;
        let conn = self.transport.connect(to, Proto::Rpc).await.map_err(unreachable)?;
        let mut stream = conn.open_bi().await.map_err(unreachable)?;
        let header = RequestHeader::new(0, self.principal(), &Request::SessionAttach { name: session.to_string(), create: true, size });
        write_msg(&mut stream, &header).await.map_err(unreachable)?;
        match read_response(&mut stream).await.map_err(unreachable)? {
            Response::Ok => {
                let (rd, writer) = tokio::io::split(stream);
                Ok(WebTerminal { reader: FrameReader::new(rd), writer })
            }
            Response::Error(e) => Err(e),
            other => Err(WireError::new(ErrorCode::Internal, format!("回应无法理解：{other:?}"))),
        }
    }
}

/// 一个连着的终端会话。调用方负责：处理完输出后发送确认，每 5 秒发送一次心跳（`Input::Ping`）。
pub struct WebTerminal<S> {
    reader: FrameReader<ReadHalf<S>>,
    writer: WriteHalf<S>,
}

impl<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin> WebTerminal<S> {
    /// 会话发来的下一帧；连接断开时为 `None`。可以安全地放进 `select!`。
    pub async fn next(&mut self) -> Option<SessionFrame> {
        self.reader.next::<SessionFrame>().await.ok().flatten()
    }

    pub async fn send(&mut self, input: Input) -> Result<(), WireError> {
        write_msg(&mut self.writer, &input).await.map_err(unreachable)
    }

    /// 有序关闭发送方向：已经发出的消息都会送到。
    pub async fn close(&mut self) {
        use tokio::io::AsyncWriteExt;
        let _ = self.writer.shutdown().await;
    }
}
