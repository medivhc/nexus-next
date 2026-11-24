//! 网页里打开已发布的网页服务（M8.2，R71）：经 Nexus 连到服务，发一个 HTTP/1.1 请求，读完回应。
//! 功能点见 docs/feature-points/M8.2.md。拼装和解析是纯函数，单独测试。

use crate::node::WebNode;
use crate::unreachable;
use nexus_fabric::{read_response, write_msg, Connection, Proto, Transport};
use nexus_wire::{ErrorCode, Request, RequestHeader, Response, ServiceInfo, WireError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 回应最大多少。
pub const MAX_RESPONSE: usize = 64 << 20;

/// 一个 HTTP 请求（临时分享的访客用，M9.1）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebHttpRequest {
    pub method: String,
    /// 以 `/` 开头，含 `?` 后面的部分。
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// 一个 HTTP 回应。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub reason: String,
    /// 去掉了逐跳的头（`Transfer-Encoding`、`Connection` 等）。
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// 逐跳的头：只对一段连接有意义，不转发。
fn hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection" | "keep-alive" | "proxy-connection" | "transfer-encoding" | "te" | "trailer" | "upgrade" | "host" | "content-length"
    )
}

fn invalid(msg: impl Into<String>) -> WireError {
    WireError::new(ErrorCode::InvalidInput, msg)
}

/// 拼请求：`Host: localhost`、`Connection: close`，有正文时带 `Content-Length`。
pub fn encode_request(method: &str, path: &str, headers: &[(String, String)], body: &[u8]) -> Result<Vec<u8>, WireError> {
    let token = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
    if !token(method) {
        return Err(invalid(format!("HTTP 方法不合法：{method:?}")));
    }
    if !path.starts_with('/') || path.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err(invalid(format!("HTTP 路径不合法：{path:?}")));
    }
    let mut out = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    for (k, v) in headers {
        if !token(k) || v.bytes().any(|b| b == b'\r' || b == b'\n') {
            return Err(invalid(format!("请求头不合法：{k:?}")));
        }
        if !hop_by_hop(k) {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    if !body.is_empty() || matches!(method, "POST" | "PUT" | "PATCH") {
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(body);
    Ok(out)
}

/// 回应的头：状态码、说明、头，和正文从哪里开始。
type Head = (u16, String, Vec<(String, String)>, usize);

/// 解析回应的头；还没到齐时为 `None`。
fn parse_head(buf: &[u8]) -> Result<Option<Head>, String> {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if buf.len() > 64 * 1024 { Err("回应头太长".into()) } else { Ok(None) };
    };
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| "回应头不是 UTF-8".to_string())?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(format!("不是 HTTP/1.x 的回应：{status_line:?}"));
    }
    let status: u16 = parts.next().and_then(|s| s.parse().ok()).filter(|s| (100..600).contains(s)).ok_or_else(|| format!("状态行不合法：{status_line:?}"))?;
    let reason = parts.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    for l in lines {
        let (k, v) = l.split_once(':').ok_or_else(|| format!("回应头不合法：{l:?}"))?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    Ok(Some((status, reason, headers, end + 4)))
}

fn get<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// 分块传输的正文：完整时返回正文。
fn dechunk(mut buf: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let mut out = Vec::new();
    loop {
        let Some(eol) = buf.windows(2).position(|w| w == b"\r\n") else { return Ok(None) };
        let size_line = std::str::from_utf8(&buf[..eol]).map_err(|_| "分块大小不合法".to_string())?;
        let size_hex = size_line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| format!("分块大小不合法：{size_line:?}"))?;
        buf = &buf[eol + 2..];
        if size == 0 {
            // 结尾的附加头（通常没有）到空行为止。
            return Ok(buf.windows(2).position(|w| w == b"\r\n").map(|_| out));
        }
        if out.len() + size > MAX_RESPONSE {
            return Err(too_big());
        }
        if buf.len() < size + 2 {
            return Ok(None);
        }
        out.extend_from_slice(&buf[..size]);
        buf = &buf[size + 2..];
    }
}

fn too_big() -> String {
    format!("回应超过 {} MiB", MAX_RESPONSE >> 20)
}

/// 解析读到的回应。`eof`：连接已经结束。回应完整时返回它；还要再读时为 `None`。
/// 中间的 `1xx` 回应跳过。`head_only`：请求是 HEAD，回应没有正文。
pub fn parse_response(mut buf: &[u8], eof: bool, head_only: bool) -> Result<Option<HttpResponse>, String> {
    loop {
        let Some((status, reason, headers, start)) = parse_head(buf)? else {
            return if eof { Err("回应还没读完连接就结束了".into()) } else { Ok(None) };
        };
        if (100..200).contains(&status) && status != 101 {
            buf = &buf[start..];
            continue;
        }
        let rest = &buf[start..];
        let finish = |body: Vec<u8>| {
            let headers = headers.iter().filter(|(k, _)| !hop_by_hop(k)).cloned().collect();
            Some(HttpResponse { status, reason: reason.clone(), headers, body })
        };
        if head_only || status == 204 || status == 304 || status == 101 {
            return Ok(finish(Vec::new()));
        }
        if get(&headers, "transfer-encoding").is_some_and(|t| t.to_ascii_lowercase().contains("chunked")) {
            return match dechunk(rest)? {
                Some(body) => Ok(finish(body)),
                None if eof => Err("分块传输还没结束连接就结束了".into()),
                None => Ok(None),
            };
        }
        if let Some(len) = get(&headers, "content-length") {
            let len: usize = len.parse().map_err(|_| format!("Content-Length 不合法：{len:?}"))?;
            if len > MAX_RESPONSE {
                return Err(too_big());
            }
            return match rest.len() >= len {
                true => Ok(finish(rest[..len].to_vec())),
                false if eof => Err("正文还没读完连接就结束了".into()),
                false => Ok(None),
            };
        }
        // 没有长度：读到连接结束。
        if rest.len() > MAX_RESPONSE {
            return Err(too_big());
        }
        return Ok(if eof { finish(rest.to_vec()) } else { None });
    }
}

/// 在已经接到服务的流上发出请求、读完回应（网页节点和临时分享的访客共用）。
pub(crate) async fn exchange<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    req: &WebHttpRequest,
    what: &str,
) -> Result<HttpResponse, WireError> {
    let request = encode_request(&req.method, &req.path, &req.headers, &req.body)?;
    stream.write_all(&request).await.map_err(unreachable)?;
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let head_only = req.method.eq_ignore_ascii_case("HEAD");
    loop {
        let n = stream.read(&mut chunk).await.map_err(unreachable)?;
        buf.extend_from_slice(&chunk[..n]);
        match parse_response(&buf, n == 0, head_only) {
            Ok(Some(r)) => {
                let _ = stream.shutdown().await;
                return Ok(r);
            }
            Ok(None) if buf.len() > MAX_RESPONSE + 64 * 1024 => return Err(invalid(too_big())),
            Ok(None) => {}
            Err(m) if m == too_big() => return Err(invalid(m)),
            Err(m) => return Err(WireError::new(ErrorCode::Unreachable, format!("{what} 的回应：{m}"))),
        }
    }
}

impl<T: Transport> WebNode<T> {
    /// 设备上它能用的服务。
    pub async fn services(&self, device: &str) -> Result<Vec<ServiceInfo>, WireError> {
        match self.rpc(device, Request::SvcList).await? {
            Response::Services(list) => Ok(list),
            other => Err(WireError::new(ErrorCode::Internal, format!("回应无法理解：{other:?}"))),
        }
    }

    /// 经 Nexus 向设备上的网页服务发一个 HTTP 请求，读完回应。不跟随跳转。
    pub async fn http(&self, device: &str, service: &str, method: &str, path: &str, headers: &[(String, String)], body: &[u8]) -> Result<HttpResponse, WireError> {
        // 先检查请求本身：不合法的不用连接。
        encode_request(method, path, headers, body)?;
        // 只对网页服务：说明里的协议不是 http 的不发（没有授权时列表里没有它，下面连接时由设备判定）。
        // 协议记下来，同一个服务的后续请求不用再问。
        let key = (device.to_string(), service.to_string());
        let known = self.svc_protos().borrow().get(&key).cloned();
        let proto = match known {
            Some(p) => Some(p),
            None => match self.services(device).await {
                Ok(list) => {
                    let mut protos = self.svc_protos().borrow_mut();
                    for s in &list {
                        protos.insert((device.to_string(), s.name.clone()), s.proto.clone());
                    }
                    protos.get(&key).cloned()
                }
                Err(_) => None,
            },
        };
        if let Some(p) = proto.filter(|p| p != "http") {
            return Err(invalid(format!("{device}/{service} 不是网页服务（协议 {p}），用 nexus connect 连接它")));
        }
        let to = self.device(device)?;
        let conn = self.transport().connect(to, Proto::Rpc).await.map_err(unreachable)?;
        let mut stream = conn.open_bi().await.map_err(unreachable)?;
        let body_req = Request::SvcConnect { name: service.to_string() };
        write_msg(&mut stream, &RequestHeader::new(0, self.principal(), &body_req)).await.map_err(unreachable)?;
        match read_response(&mut stream).await.map_err(unreachable)? {
            Response::Ok => {}
            Response::Error(e) => return Err(e),
            other => return Err(WireError::new(ErrorCode::Internal, format!("回应无法理解：{other:?}"))),
        }
        let req = WebHttpRequest { method: method.to_string(), path: path.to_string(), headers: headers.to_vec(), body: body.to_vec() };
        exchange(&mut stream, &req, &format!("{device}/{service}")).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(raw: &[u8]) -> HttpResponse {
        parse_response(raw, true, false).unwrap().expect("应当完整")
    }

    #[test]
    fn request_has_host_close_and_length() {
        let h = vec![("Cookie".to_string(), "a=b".to_string()), ("Connection".to_string(), "keep-alive".to_string())];
        let r = encode_request("POST", "/f?x=1", &h, b"a=1").unwrap();
        assert_eq!(
            String::from_utf8(r).unwrap(),
            "POST /f?x=1 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nCookie: a=b\r\nContent-Length: 3\r\n\r\na=1"
        );
        assert!(encode_request("GET", "no-slash", &[], b"").is_err());
        assert!(encode_request("G T", "/", &[], b"").is_err());
        assert!(encode_request("GET", "/", &[("X".into(), "a\r\nEvil: 1".into())], b"").is_err());
    }

    #[test]
    fn content_length_chunked_and_close_delimited() {
        let r = done(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-A: 1\r\n\r\nhelloEXTRA");
        assert_eq!((r.status, r.body.as_slice(), r.headers.clone()), (200, &b"hello"[..], vec![("X-A".to_string(), "1".to_string())]));
        let r = done(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;ext=1\r\nde\r\n0\r\n\r\n");
        assert_eq!(r.body, b"abcde");
        assert!(r.headers.is_empty());
        assert_eq!(done(b"HTTP/1.0 200 OK\r\n\r\nuntil close").body, b"until close");
        // 还没读完
        assert_eq!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhel", false, false).unwrap(), None);
        assert_eq!(parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nab", false, false).unwrap(), None);
        assert_eq!(parse_response(b"HTTP/1.1 200 OK\r\n\r\npart", false, false).unwrap(), None);
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhel", true, false).is_err());
    }

    #[test]
    fn interim_no_body_and_bad_responses() {
        let r = done(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\nok");
        assert_eq!((r.status, r.body.as_slice()), (201, &b"ok"[..]));
        assert_eq!(done(b"HTTP/1.1 304 Not Modified\r\nETag: x\r\n\r\n").body, b"");
        assert_eq!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n", false, true).unwrap().unwrap().body, b"");
        assert!(parse_response(b"SSH-2.0-OpenSSH\r\n\r\n", true, false).is_err());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 99999999999\r\n\r\n", false, false).unwrap_err().contains("MiB"));
    }
}
