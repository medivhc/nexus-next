//! 运行时：把纯逻辑接到 `Transport` 和 `System` 两个接缝上。
//!
//! 本 crate 不直接引用任何具体的传输或系统实现，也不自行创建任务：
//! 所有并发都在 `Node::run` 这一个 future 里完成，因此同一份代码既能跑在
//! 真实的异步运行时上，也能跑在单线程的仿真里。
#![forbid(unsafe_code)]

use futures_util::stream::{FuturesUnordered, StreamExt};
use nexus_core_proto::{NodeId, NodeInfo, PeerBook};
use nexus_fabric::{read_msg, write_msg, Connection, Transport};
use nexus_sys::System;
use nexus_wire::{
    CtlRequest, CtlResponse, ErrorCode, Request, RequestHeader, Response, StatusReport, WireError,
    WIRE_VERSION,
};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// 一次本机控制调用：请求，以及回传结果的通道。
pub type CtlCall = (CtlRequest, oneshot::Sender<CtlResponse>);

/// 等待对端响应的上限。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

const KEY_NODE_ID: &str = "node_id";
const KEY_PEERS: &str = "peers";

/// 读取本节点的标识；第一次调用时生成并保存。
pub fn load_or_create_node_id<S: System>(sys: &S) -> io::Result<NodeId> {
    if let Some(bytes) = sys.store_get(KEY_NODE_ID)? {
        let raw: [u8; 32] = bytes
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "节点标识文件损坏"))?;
        return Ok(NodeId(raw));
    }
    let mut raw = [0u8; 32];
    sys.random(&mut raw);
    sys.store_put(KEY_NODE_ID, &raw)?;
    Ok(NodeId(raw))
}

pub fn load_peers<S: System>(sys: &S) -> io::Result<PeerBook> {
    match sys.store_get(KEY_PEERS)? {
        None => Ok(PeerBook::default()),
        Some(bytes) => postcard::from_bytes(&bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "已知节点表损坏")),
    }
}

pub fn save_peers<S: System>(sys: &S, peers: &PeerBook) -> io::Result<()> {
    let bytes = postcard::to_allocvec(peers).map_err(io::Error::other)?;
    sys.store_put(KEY_PEERS, &bytes)
}

pub struct Node<T, S> {
    transport: T,
    system: S,
    info: NodeInfo,
}

type Task<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

impl<T: Transport, S: System> Node<T, S> {
    pub fn new(transport: T, system: S, name: &str) -> Self {
        let info = NodeInfo {
            node_id: transport.node_id(),
            name: name.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: system.now(),
            now: system.now(),
        };
        Self { transport, system, info }
    }

    /// 本节点此刻的自述。
    pub fn info(&self) -> NodeInfo {
        NodeInfo { now: self.system.now(), ..self.info.clone() }
    }

    /// 驱动整个节点：接受连接、处理请求、处理本机控制调用。正常情况下不返回。
    pub async fn run(&self, ctl: &mut mpsc::Receiver<CtlCall>) -> io::Result<()> {
        let mut tasks: FuturesUnordered<Task<'_>> = FuturesUnordered::new();
        // accept 不保证可以安全取消，所以让同一个 future 跨循环存活。
        let mut accept = Box::pin(self.transport.accept());
        loop {
            tokio::select! {
                conn = &mut accept => {
                    let conn = conn?;
                    accept = Box::pin(self.transport.accept());
                    tasks.push(Box::pin(self.serve_conn(conn)));
                }
                Some((req, reply)) = ctl.recv() => {
                    tasks.push(Box::pin(async move {
                        let _ = reply.send(self.handle_ctl(req).await);
                    }));
                }
                Some(()) = tasks.next(), if !tasks.is_empty() => {}
            }
        }
    }

    async fn serve_conn(&self, conn: T::Conn) {
        while let Ok(Some(mut stream)) = conn.accept_bi().await {
            let response = match read_msg::<_, RequestHeader>(&mut stream).await {
                Ok(header) => self.handle_request(header),
                Err(_) => return,
            };
            let _ = write_msg(&mut stream, &response).await;
        }
    }

    fn handle_request(&self, header: RequestHeader) -> Response {
        if header.version != WIRE_VERSION {
            return Response::Error(WireError {
                code: ErrorCode::UnsupportedVersion,
                message: format!("不支持的协议版本 {}", header.version),
            });
        }
        match header.body {
            Request::NodeInfo => Response::NodeInfo(self.info()),
        }
    }

    /// 处理一次本机控制调用。
    pub async fn handle_ctl(&self, req: CtlRequest) -> CtlResponse {
        match req {
            CtlRequest::Status => CtlResponse::Status(StatusReport {
                name: self.info.name.clone(),
                node_id: self.info.node_id,
                info: Some(self.info()),
                online: true,
                rtt_ms: None,
                path: None,
            }),
            CtlRequest::RemoteStatus { node } => match self.remote_status(&node).await {
                Ok(report) => CtlResponse::Status(report),
                Err(e) => CtlResponse::Error(e),
            },
        }
    }

    async fn remote_status(&self, name_or_id: &str) -> Result<StatusReport, WireError> {
        let peers = load_peers(&self.system)
            .map_err(|e| WireError { code: ErrorCode::Internal, message: e.to_string() })?;
        let unknown = || WireError {
            code: ErrorCode::UnknownNode,
            message: format!("不认识节点 {name_or_id}"),
        };
        let id = peers.resolve(name_or_id).ok_or_else(unknown)?;
        let name = peers
            .peers
            .iter()
            .find(|(_, p)| *p == id)
            .map(|(n, _)| n.clone())
            .ok_or_else(unknown)?;

        let started = self.system.now();
        let reply = tokio::time::timeout(REQUEST_TIMEOUT, self.request(id, Request::NodeInfo)).await;
        let rtt_ms = self.system.now().millis_since(started);
        Ok(match reply {
            Ok(Ok(Response::NodeInfo(info))) if info.node_id == id => StatusReport {
                name,
                node_id: id,
                info: Some(info),
                online: true,
                rtt_ms: Some(rtt_ms),
                path: self.transport.path_kind(id),
            },
            // 超时、连接失败、对端报错、或对端自称是别的节点：一律视为不可达。
            _ => StatusReport { name, node_id: id, info: None, online: false, rtt_ms: None, path: None },
        })
    }

    /// 向另一个节点发起一次调用：一条流，一个请求，一个响应。
    async fn request(&self, to: NodeId, body: Request) -> io::Result<Response> {
        let conn = self.transport.connect(to).await?;
        let mut stream = conn.open_bi().await?;
        write_msg(&mut stream, &RequestHeader { version: WIRE_VERSION, body }).await?;
        read_msg(&mut stream).await
    }
}
