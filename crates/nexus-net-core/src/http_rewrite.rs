//! 显式 HTTP 代理层可编程变换（N6：U08 Map Local / 重定向 / 请求头改写 / 故障模拟）。
//!
//! 严格纯逻辑、无网络 I/O、无 `unsafe`；不涉及 HTTPS MITM，仅对显式命中的域名与路径前缀生效。

use crate::route::Matcher;
use crate::{Host, Target};
use serde::{Deserialize, Serialize};

/// 单条 Map Local / 故障注入响应体最大字节预算（64 KiB），防止内存滥用。
pub const MAX_MOCK_BODY_BYTES: usize = 64 * 1024;

/// 显式 HTTP 变换动作（使用外部标签枚举，完全兼容 postcard 与 JSON）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HttpAction {
    /// 直接返回本地模拟响应（Map Local），不访问上游。
    MapLocal {
        status: u16,
        content_type: String,
        body: String,
    },
    /// 将请求重定向到测试服务（重写目标与 Host 请求头）。
    RedirectTarget {
        target_host: String,
        target_port: u16,
    },
    /// 增改或删除指定请求头。
    ModifyRequestHeaders {
        set: Vec<(String, String)>,
        remove: Vec<String>,
    },
    /// 开发环境故障注入（直接返回指定状态码，如 503/429）。
    FaultInject {
        status: u16,
        reason: String,
        body: String,
    },
}

/// 一条可审计、可独立开关的 HTTP 变换规则。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRule {
    pub id: String,
    pub host: Matcher,
    /// 路径前缀（例如 `"/v1/"` 或 `"/"` 匹配全部路径）。
    pub path_prefix: String,
    pub enabled: bool,
    pub action: HttpAction,
}

/// HTTP 规则持久化表。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRuleTable {
    pub rules: Vec<HttpRule>,
}

/// HTTP 变换执行结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpRewriteOutcome {
    PassThrough,
    MockResponse {
        rule_id: String,
        bytes: Vec<u8>,
    },
    RewrittenForward {
        rule_id: String,
        target: Target,
        head: Vec<u8>,
    },
}

/// 从已改写为源站格式的 HTTP 请求头中提取请求路径（`GET /path HTTP/1.1` → `"/path"`）。
pub fn extract_request_path(head: &[u8]) -> Option<&str> {
    let line_end = head.windows(2).position(|w| w == b"\r\n")?;
    let line = core::str::from_utf8(&head[..line_end]).ok()?;
    let mut parts = line.split(' ');
    let _method = parts.next()?;
    parts.next()
}

/// 对一条显式 HTTP 代理转发请求应用规则表；未命中时返回 `PassThrough`。
pub fn apply_http_rules(target: &Target, head: &[u8], rules: &[HttpRule]) -> HttpRewriteOutcome {
    let path = extract_request_path(head).unwrap_or("/");
    for rule in rules {
        if !rule.enabled {
            continue;
        }
        if !rule.host.matches(target) {
            continue;
        }
        let prefix = if rule.path_prefix.is_empty() { "/" } else { rule.path_prefix.as_str() };
        if !path.starts_with(prefix) {
            continue;
        }
        match &rule.action {
            HttpAction::MapLocal { status, content_type, body } => {
                let clipped = clip_body(body);
                let bytes = build_http_response(*status, reason_phrase(*status), content_type, clipped);
                return HttpRewriteOutcome::MockResponse {
                    rule_id: rule.id.clone(),
                    bytes,
                };
            }
            HttpAction::FaultInject { status, reason, body } => {
                let clipped = clip_body(body);
                let r = if reason.trim().is_empty() { reason_phrase(*status) } else { reason.as_str() };
                let bytes = build_http_response(*status, r, "text/plain; charset=utf-8", clipped);
                return HttpRewriteOutcome::MockResponse {
                    rule_id: rule.id.clone(),
                    bytes,
                };
            }
            HttpAction::RedirectTarget { target_host, target_port } => {
                let host = match target_host.parse::<core::net::IpAddr>() {
                    Ok(ip) => Host::Ip(ip),
                    Err(_) => Host::Name(target_host.clone()),
                };
                let new_target = Target { host, port: *target_port };
                let host_header = if *target_port == 80 {
                    target_host.clone()
                } else {
                    format!("{target_host}:{target_port}")
                };
                let new_head = rewrite_headers(head, &[("Host".into(), host_header)], &[]);
                return HttpRewriteOutcome::RewrittenForward {
                    rule_id: rule.id.clone(),
                    target: new_target,
                    head: new_head,
                };
            }
            HttpAction::ModifyRequestHeaders { set, remove } => {
                let new_head = rewrite_headers(head, set, remove);
                return HttpRewriteOutcome::RewrittenForward {
                    rule_id: rule.id.clone(),
                    target: target.clone(),
                    head: new_head,
                };
            }
        }
    }
    HttpRewriteOutcome::PassThrough
}

fn clip_body(body: &str) -> &str {
    if body.len() <= MAX_MOCK_BODY_BYTES {
        return body;
    }
    let mut end = MAX_MOCK_BODY_BYTES;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    &body[..end]
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Response",
    }
}

fn build_http_response(status: u16, reason: &str, content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nX-Nexus-Mock: true\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn rewrite_headers(head: &[u8], set: &[(String, String)], remove: &[String]) -> Vec<u8> {
    let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") else {
        return head.to_vec();
    };
    let Ok(text) = core::str::from_utf8(&head[..end]) else {
        return head.to_vec();
    };
    let mut lines = text.split("\r\n");
    let Some(req_line) = lines.next() else {
        return head.to_vec();
    };
    let mut out = format!("{req_line}\r\n");
    for line in lines {
        let name = line.split(':').next().unwrap_or("").trim();
        let drop_it = remove.iter().any(|r| r.eq_ignore_ascii_case(name))
            || set.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
        if drop_it {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    for (k, v) in set {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(&head[end + 4..]);
    bytes
}
