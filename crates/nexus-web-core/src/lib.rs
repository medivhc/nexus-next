//! 网页节点的客户端逻辑。
//!
//! 浏览器里编译成 WebAssembly 运行，传输是 iroh 的浏览器版；测试和仿真里用同一份代码，
//! 通行密钥由软件认证器扮演。这里只做协议，不碰浏览器 API：通行密钥的创建和签名由调用方完成。
#![forbid(unsafe_code)]

mod drive;
mod handle;
mod node;
mod service;
mod terminal;
pub use drive::{DriveEntry, DriveFile, DriveWorkspace};
pub use handle::{EntryHandle, MAX_USER_HANDLE};
pub use node::{challenge, WebNode, WebTerminal};
pub use service::{
    encode_http_request, parse_http_response, rewrite_location, rewrite_response_headers, rewrite_set_cookie,
    WebHttpRequest, WebHttpResponse, MAX_HTTP_RESPONSE_BYTES,
};
pub use terminal::{Notice, TerminalEnd, TerminalUi, UiEvent};

use nexus_core_proto::authz::Principal;
use nexus_core_proto::NodeId;
use nexus_fabric::{read_response, write_msg, Connection, Proto, Transport};
use nexus_wire::{hex_decode, ErrorCode, NewCredential, Request, RequestHeader, Response, WireError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 登记链接里的信息：`…#enroll=<登记码>&node=<管理设备>&hint=<地址提示>`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnrollLink {
    pub token: [u8; 16],
    pub node: NodeId,
    pub hint: String,
}

impl EnrollLink {
    /// 从完整链接或只从 `#` 之后的部分解析。
    pub fn parse(link: &str) -> Result<Self, String> {
        let fragment = link.rsplit_once('#').map_or(link, |(_, f)| f);
        let field = |key: &str| {
            fragment
                .split('&')
                .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
                .ok_or_else(|| format!("登记链接里缺少 {key}"))
        };
        let token: [u8; 16] = hex_decode(field("enroll")?)
            .and_then(|b| b.try_into().ok())
            .ok_or("登记码格式不对")?;
        let node: NodeId = field("node")?.parse().map_err(|_| "设备标识格式不对")?;
        Ok(Self { token, node, hint: field("hint")?.to_string() })
    }
}

pub(crate) fn unreachable(e: std::io::Error) -> WireError {
    WireError::new(ErrorCode::Unreachable, format!("连不上设备：{e}"))
}

/// 发一个请求、收一个回应，然后像常驻程序之间那样有序关闭这条流。
pub(crate) async fn call<T: Transport>(
    transport: &T,
    to: NodeId,
    proto: Proto,
    principal: Principal,
    body: Request,
) -> Result<Response, WireError> {
    let conn = transport.connect(to, proto).await.map_err(unreachable)?;
    let mut stream = conn.open_bi().await.map_err(unreachable)?;
    let header = RequestHeader::new(0, principal, &body);
    write_msg(&mut stream, &header).await.map_err(unreachable)?;
    let resp: Response = read_response(&mut stream).await.map_err(unreachable)?;
    let _ = stream.shutdown().await;
    // 读到对方关闭为止，再丢弃这条流。
    let mut sink = [0u8; 256];
    while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {}
    Ok(resp)
}

/// 把新建的通行密钥交给管理设备登记，返回它在网络里的名字。
pub async fn enroll<T: Transport>(transport: &T, link: &EnrollLink, credential: NewCredential) -> Result<String, WireError> {
    transport.add_hint(link.node, &link.hint);
    let body = Request::WebEnroll { token: link.token, credential };
    match call(transport, link.node, Proto::Web, Principal::Device, body).await? {
        Response::WebEnrolled { label } => Ok(label),
        Response::Error(e) => Err(e),
        other => Err(WireError::new(ErrorCode::Internal, format!("管理设备的回应无法理解：{other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enroll_links_parse() {
        let node = NodeId([7; 32]);
        let link = format!("https://bewith.link/#enroll={}&node={node}&hint=abcd", "0f".repeat(16));
        let parsed = EnrollLink::parse(&link).unwrap();
        assert_eq!(parsed, EnrollLink { token: [0x0f; 16], node, hint: "abcd".into() });
        assert_eq!(EnrollLink::parse(&link[link.find('#').unwrap() + 1..]).unwrap(), parsed);
        assert!(EnrollLink::parse("https://bewith.link/").is_err());
        assert!(EnrollLink::parse(&link.replace("enroll=0f", "enroll=0")).is_err());
        assert!(EnrollLink::parse(&link.replace("node=07", "node=zz")).is_err());
    }
}
