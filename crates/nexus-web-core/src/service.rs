//! 网页节点打开已发布的 HTTP 服务（M8.2，R69）：复用现有 `SvcList` 与 `SvcConnect` 协议，
//! 在双向字节流上完成 HTTP/1.1 请求编码与响应解析。

use crate::node::WebNode;
use crate::unreachable;
use nexus_core_proto::path::valid_segment;
use nexus_fabric::{read_response, write_msg, Connection, Proto, Transport};
use nexus_wire::{ErrorCode, Request, RequestHeader, Response, ServiceInfo, WireError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 单次 HTTP 响应体的内存上限（32 MiB），防止单个大响应拖垮浏览器标签页。
pub const MAX_HTTP_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
/// 响应头允许的最大字节数（64 KiB）。
const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;

/// 浏览器经 Nexus 转发的一次 HTTP/1.1 请求。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebHttpRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// 目标本机 HTTP 服务返回的 HTTP/1.1 响应。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebHttpResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

fn bad_input(msg: impl Into<String>) -> WireError {
    WireError::new(ErrorCode::InvalidInput, msg)
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "proxy-connection"
            | "keep-alive"
            | "transfer-encoding"
            | "upgrade"
            | "te"
            | "trailers"
    )
}

/// 将请求编码为 HTTP/1.1 报文；拒绝含换行符的字段以防 CRLF 注入。
pub fn encode_http_request(req: &WebHttpRequest, host: &str) -> Result<Vec<u8>, WireError> {
    if req.method.is_empty()
        || !req
            .method
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(bad_input(format!("非法的 HTTP 方法：{:?}", req.method)));
    }
    if !req.path.starts_with('/') || req.path.bytes().any(|b| b == b'\r' || b == b'\n' || b == b' ' || b == 0) {
        return Err(bad_input(format!("非法的请求路径：{:?}", req.path)));
    }
    let mut out = Vec::with_capacity(256 + req.body.len());
    out.extend_from_slice(format!("{} {} HTTP/1.1\r\n", req.method, req.path).as_bytes());
    let mut has_host = false;
    for (k, v) in &req.headers {
        if k.is_empty()
            || k.bytes().any(|b| b <= 0x20 || b == b':' || b == 0x7f)
            || v.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
        {
            return Err(bad_input(format!("非法的请求头：{k:?}")));
        }
        if is_hop_by_hop(k) || k.eq_ignore_ascii_case("content-length") {
            continue;
        }
        if k.eq_ignore_ascii_case("host") {
            has_host = true;
        }
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if !has_host {
        out.extend_from_slice(format!("Host: {host}\r\n").as_bytes());
    }
    out.extend_from_slice(b"Connection: close\r\n");
    if !req.body.is_empty() || matches!(req.method.as_str(), "POST" | "PUT" | "PATCH") {
        out.extend_from_slice(format!("Content-Length: {}\r\n", req.body.len()).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&req.body);
    Ok(out)
}

/// 解析 HTTP/1.1 响应报文（支持 `Content-Length`、`Transfer-Encoding: chunked` 与读至 EOF）。
pub fn parse_http_response(raw: &[u8], max_body: usize) -> Result<WebHttpResponse, WireError> {
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Err(WireError::new(ErrorCode::Internal, "HTTP 响应缺少头部结束标记"));
    };
    let head = std::str::from_utf8(&raw[..split])
        .map_err(|_| WireError::new(ErrorCode::Internal, "HTTP 响应头不是合法 UTF-8"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| WireError::new(ErrorCode::Internal, "HTTP 响应为空"))?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(WireError::new(ErrorCode::Internal, format!("不支持的 HTTP 版本：{version}")));
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| WireError::new(ErrorCode::Internal, format!("非法的 HTTP 状态码：{status_line}")))?;
    let status_text = parts.next().unwrap_or("").to_string();

    let mut headers = Vec::new();
    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let key = k.trim();
        let val = v.trim();
        if key.eq_ignore_ascii_case("transfer-encoding") && val.to_ascii_lowercase().contains("chunked") {
            chunked = true;
            continue;
        }
        if key.eq_ignore_ascii_case("content-length") {
            if let Ok(n) = val.parse::<usize>() {
                content_length = Some(n);
            }
        }
        if is_hop_by_hop(key) {
            continue;
        }
        headers.push((key.to_string(), val.to_string()));
    }

    let raw_body = &raw[split + 4..];
    let body = if chunked {
        // 去掉不再准确的原始 Content-Length（若有）
        headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-length"));
        decode_chunked(raw_body, max_body)?
    } else if let Some(len) = content_length {
        if len > max_body {
            return Err(WireError::new(ErrorCode::Internal, format!("响应体超过上限（{len} > {max_body} 字节）")));
        }
        let end = len.min(raw_body.len());
        raw_body[..end].to_vec()
    } else {
        if raw_body.len() > max_body {
            return Err(WireError::new(ErrorCode::Internal, format!("响应体超过上限（{} > {max_body} 字节）", raw_body.len())));
        }
        raw_body.to_vec()
    };

    Ok(WebHttpResponse { status, status_text, headers, body })
}

fn decode_chunked(mut input: &[u8], max_body: usize) -> Result<Vec<u8>, WireError> {
    let mut out = Vec::new();
    loop {
        let Some(crlf) = input.windows(2).position(|w| w == b"\r\n") else {
            return Err(WireError::new(ErrorCode::Internal, "chunked 响应缺少块长度行结束符"));
        };
        let size_line = std::str::from_utf8(&input[..crlf])
            .map_err(|_| WireError::new(ErrorCode::Internal, "chunked 块长度不是 UTF-8"))?;
        let hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(hex, 16)
            .map_err(|_| WireError::new(ErrorCode::Internal, format!("非法的 chunked 长度：{hex:?}")))?;
        input = &input[crlf + 2..];
        if size == 0 {
            break;
        }
        if out.len().saturating_add(size) > max_body {
            return Err(WireError::new(ErrorCode::Internal, format!("chunked 响应超过上限（{max_body} 字节）")));
        }
        if input.len() < size + 2 || &input[size..size + 2] != b"\r\n" {
            return Err(WireError::new(ErrorCode::Internal, "chunked 响应块数据不完整"));
        }
        out.extend_from_slice(&input[..size]);
        input = &input[size + 2..];
    }
    Ok(out)
}

/// 将相对根路径的 `Location`（如 `/login`）改写到 `/_svc/<device>/<service>/login`。
pub fn rewrite_location(location: &str, device: &str, service: &str) -> String {
    let prefix = format!("/_svc/{device}/{service}");
    if location.starts_with('/') && !location.starts_with("//") && !location.starts_with(&format!("{prefix}/")) && location != prefix {
        format!("{prefix}{location}")
    } else {
        location.to_string()
    }
}

/// 将 `Set-Cookie` 的 `Path` 收窄到 `/_svc/<device>/<service>/`，防止写入主页面根路径。
pub fn rewrite_set_cookie(cookie: &str, device: &str, service: &str) -> String {
    let scoped = format!("Path=/_svc/{device}/{service}/");
    let mut parts: Vec<String> = Vec::new();
    let mut replaced = false;
    for (i, part) in cookie.split(';').enumerate() {
        let trimmed = part.trim();
        if i > 0 && trimmed.to_ascii_lowercase().starts_with("path=") {
            parts.push(scoped.clone());
            replaced = true;
        } else if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }
    if !replaced {
        parts.push(scoped);
    }
    parts.join("; ")
}

/// 对经浏览器代理返回的响应头做路径与 Cookie 隔离改写。
pub fn rewrite_response_headers(resp: &mut WebHttpResponse, device: &str, service: &str) {
    for (k, v) in &mut resp.headers {
        if k.eq_ignore_ascii_case("location") {
            *v = rewrite_location(v, device, service);
        } else if k.eq_ignore_ascii_case("set-cookie") {
            *v = rewrite_set_cookie(v, device, service);
        }
    }
}

impl<T: Transport> WebNode<T> {
    /// 列出目标设备上当前通行密钥有权连接的已发布服务。
    pub async fn services(&self, device: &str) -> Result<Vec<ServiceInfo>, WireError> {
        match self.rpc(device, Request::SvcList).await? {
            Response::Services(list) => Ok(list),
            other => Err(WireError::new(ErrorCode::Internal, format!("回应无法理解：{other:?}"))),
        }
    }

    /// 经 `SvcConnect` 向目标设备上已发布的 HTTP 服务转发一次 HTTP/1.1 请求。
    pub async fn http_fetch(&self, device: &str, service: &str, req: &WebHttpRequest) -> Result<WebHttpResponse, WireError> {
        if !valid_segment(service) {
            return Err(bad_input(format!("服务名不合法：{service:?}")));
        }
        let host = format!("{service}.{device}.internal");
        let encoded = encode_http_request(req, &host)?;
        let to = self.device(device)?;
        let conn = self.transport().connect(to, Proto::Rpc).await.map_err(unreachable)?;
        let mut stream = conn.open_bi().await.map_err(unreachable)?;
        let header = RequestHeader::new(0, self.principal(), &Request::SvcConnect { name: service.to_string() });
        write_msg(&mut stream, &header).await.map_err(unreachable)?;
        match read_response(&mut stream).await.map_err(unreachable)? {
            Response::Ok => {}
            Response::Error(e) => return Err(e),
            other => return Err(WireError::new(ErrorCode::Internal, format!("回应无法理解：{other:?}"))),
        }
        stream.write_all(&encoded).await.map_err(unreachable)?;
        stream.shutdown().await.map_err(unreachable)?;

        let hard_limit = MAX_HTTP_RESPONSE_BYTES + MAX_HTTP_HEADER_BYTES;
        let mut raw = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = stream.read(&mut buf).await.map_err(unreachable)?;
            if n == 0 {
                break;
            }
            if raw.len().saturating_add(n) > hard_limit {
                return Err(WireError::new(
                    ErrorCode::Internal,
                    format!("响应超过 {} MiB 上限", MAX_HTTP_RESPONSE_BYTES / (1024 * 1024)),
                ));
            }
            raw.extend_from_slice(&buf[..n]);
        }
        parse_http_response(&raw, MAX_HTTP_RESPONSE_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_request_blocks_crlf_and_strips_hop_by_hop() {
        let req = WebHttpRequest {
            method: "POST".into(),
            path: "/api/items?a=1".into(),
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("Connection".into(), "keep-alive".into()),
            ],
            body: b"{}".to_vec(),
        };
        let text = String::from_utf8(encode_http_request(&req, "web.home.internal").unwrap()).unwrap();
        assert!(text.starts_with("POST /api/items?a=1 HTTP/1.1\r\n"));
        assert!(text.contains("Host: web.home.internal\r\n"));
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.contains("Content-Length: 2\r\n"));
        assert!(!text.contains("keep-alive"));
        assert!(text.ends_with("\r\n\r\n{}"));

        let bad_path = WebHttpRequest { method: "GET".into(), path: "/a\r\nEvil: 1".into(), headers: vec![], body: vec![] };
        assert_eq!(encode_http_request(&bad_path, "h").unwrap_err().code, ErrorCode::InvalidInput);
        let bad_hdr = WebHttpRequest {
            method: "GET".into(),
            path: "/".into(),
            headers: vec![("X-Test".into(), "ok\r\nInjected: 1".into())],
            body: vec![],
        };
        assert_eq!(encode_http_request(&bad_hdr, "h").unwrap_err().code, ErrorCode::InvalidInput);
    }

    #[test]
    fn parse_response_supports_content_length_chunked_and_limit() {
        let cl = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello";
        let r = parse_http_response(cl, 1024).unwrap();
        assert_eq!((r.status, r.status_text.as_str(), r.body.as_slice()), (200, "OK", b"hello".as_slice()));

        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n";
        let r = parse_http_response(chunked, 1024).unwrap();
        assert_eq!(r.body, b"hello world");
        assert!(!r.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding")));

        // 超出单次响应上限时报错。
        assert_eq!(parse_http_response(chunked, 4).unwrap_err().code, ErrorCode::Internal);
        assert_eq!(parse_http_response(cl, 4).unwrap_err().code, ErrorCode::Internal);
    }

    #[test]
    fn rewrite_location_and_cookie_scope_to_service_prefix() {
        assert_eq!(rewrite_location("/dashboard?tab=1", "home", "web"), "/_svc/home/web/dashboard?tab=1");
        assert_eq!(rewrite_location("/_svc/home/web/already", "home", "web"), "/_svc/home/web/already");
        assert_eq!(rewrite_location("https://example.com/a", "home", "web"), "https://example.com/a");
        assert_eq!(rewrite_location("//cdn.example.com/a.js", "home", "web"), "//cdn.example.com/a.js");

        assert_eq!(
            rewrite_set_cookie("sid=abc; Path=/; HttpOnly", "home", "web"),
            "sid=abc; Path=/_svc/home/web/; HttpOnly"
        );
        assert_eq!(
            rewrite_set_cookie("sid=abc; Secure", "home", "web"),
            "sid=abc; Secure; Path=/_svc/home/web/"
        );
    }
}
