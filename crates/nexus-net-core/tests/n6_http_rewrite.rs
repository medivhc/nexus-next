// covers: R40, R41
//! N6 显式 HTTP 变换纯逻辑单测（对应 docs/test-cases/network-runtime-n6-test-cases.md）。

use nexus_net_core::http_rewrite::{
    apply_http_rules, HttpAction, HttpRewriteOutcome, HttpRule, MAX_MOCK_BODY_BYTES,
};
use nexus_net_core::route::Matcher;
use nexus_net_core::Target;

/// TC-N6-01：Map Local 仅对命中域名与路径返回模拟响应，不影响未匹配生产域名
#[test]
fn map_local_returns_mock_without_touching_unmatched_hosts() {
    let rules = vec![HttpRule {
        id: "mock-api".into(),
        host: Matcher::parse_domain_exact("api.dev.test").unwrap(),
        path_prefix: "/v1/mock".into(),
        enabled: true,
        action: HttpAction::MapLocal {
            status: 200,
            content_type: "application/json".into(),
            body: r#"{"ok":true}"#.into(),
        },
    }];
    let req = b"GET /v1/mock HTTP/1.1\r\nHost: api.dev.test\r\n\r\n";
    let dev = Target::parse("api.dev.test:80").unwrap();
    match apply_http_rules(&dev, req, &rules) {
        HttpRewriteOutcome::MockResponse { rule_id, bytes } => {
            assert_eq!(rule_id, "mock-api");
            let text = String::from_utf8(bytes).unwrap();
            assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
            assert!(text.ends_with(r#"{"ok":true}"#), "{text}");
        }
        other => panic!("期望 MockResponse：{other:?}"),
    }

    let prod = Target::parse("prod.example.com:80").unwrap();
    assert_eq!(apply_http_rules(&prod, req, &rules), HttpRewriteOutcome::PassThrough);
}

/// TC-N6-02：测试服务重定向与请求头增改/删除正确改写
#[test]
fn redirect_target_and_header_modification_rewrite_correctly() {
    let redirect_rules = vec![HttpRule {
        id: "redir".into(),
        host: Matcher::parse_domain("api.dev.test").unwrap(),
        path_prefix: "/v1/".into(),
        enabled: true,
        action: HttpAction::RedirectTarget {
            target_host: "127.0.0.1".into(),
            target_port: 18080,
        },
    }];
    let req = b"GET /v1/users HTTP/1.1\r\nHost: api.dev.test\r\nX-Trace: 1\r\n\r\n";
    match apply_http_rules(&Target::parse("api.dev.test:80").unwrap(), req, &redirect_rules) {
        HttpRewriteOutcome::RewrittenForward { target, head, .. } => {
            assert_eq!(target.to_string(), "127.0.0.1:18080");
            let text = String::from_utf8(head).unwrap();
            assert!(text.contains("Host: 127.0.0.1:18080\r\n"), "{text}");
            assert!(text.contains("X-Trace: 1\r\n"), "{text}");
        }
        other => panic!("期望 RewrittenForward：{other:?}"),
    }

    let header_rules = vec![HttpRule {
        id: "hdr".into(),
        host: Matcher::parse_domain_exact("api.dev.test").unwrap(),
        path_prefix: "/".into(),
        enabled: true,
        action: HttpAction::ModifyRequestHeaders {
            set: vec![("X-Env".into(), "staging".into())],
            remove: vec!["X-Trace".into()],
        },
    }];
    match apply_http_rules(&Target::parse("api.dev.test:80").unwrap(), req, &header_rules) {
        HttpRewriteOutcome::RewrittenForward { head, .. } => {
            let text = String::from_utf8(head).unwrap();
            assert!(text.contains("X-Env: staging\r\n"), "{text}");
            assert!(!text.contains("X-Trace"), "{text}");
        }
        other => panic!("期望 RewrittenForward：{other:?}"),
    }
}

/// TC-N6-03：故障注入返回指定状态码，且超大响应体受 64 KiB 预算裁剪
#[test]
fn fault_inject_and_body_size_budget_are_enforced() {
    let huge = "a".repeat(MAX_MOCK_BODY_BYTES + 1024);
    let rules = vec![HttpRule {
        id: "fault-503".into(),
        host: Matcher::parse_domain("flaky.dev.test").unwrap(),
        path_prefix: "/".into(),
        enabled: true,
        action: HttpAction::FaultInject {
            status: 503,
            reason: "Service Unavailable".into(),
            body: huge,
        },
    }];
    let req = b"GET / HTTP/1.1\r\nHost: flaky.dev.test\r\n\r\n";
    match apply_http_rules(&Target::parse("flaky.dev.test:80").unwrap(), req, &rules) {
        HttpRewriteOutcome::MockResponse { bytes, .. } => {
            let text = String::from_utf8(bytes).unwrap();
            assert!(text.starts_with("HTTP/1.1 503 Service Unavailable\r\n"));
            let body = text.split("\r\n\r\n").nth(1).unwrap();
            assert_eq!(body.len(), MAX_MOCK_BODY_BYTES);
        }
        other => panic!("期望 MockResponse：{other:?}"),
    }
}
