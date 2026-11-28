// covers: R41, R42, R43, R105, R106, R107
//! N3 纯逻辑测试用例（对应 docs/test-cases/network-runtime-n3-test-cases.md）。

use core::net::{IpAddr, Ipv4Addr};
use nexus_net_core::diag::{classify_http_status, FailureClass, FlowEvent, FlowRing, ServiceStatus};
use nexus_net_core::dns_policy::{evaluate_dns, DnsAction, DnsDecision, DnsRule};
use nexus_net_core::policy::{select_exit, ExitCandidate, FailureAction, PolicyConstraint, PolicyDecision};
use nexus_net_core::route::Matcher;

fn cand(device: &str, region: Option<&str>, stable: bool, healthy: bool, rtt: Option<u32>) -> ExitCandidate {
    ExitCandidate {
        device: device.into(),
        region: region.map(String::from),
        stable_egress: stable,
        healthy,
        rtt_ms: rtt,
        tags: vec![],
    }
}

/// TC-N3-01：地域硬约束选择延迟最优的健康候选，并完整解释淘汰原因
#[test]
fn region_constraint_selects_lowest_rtt_and_explains_rejections() {
    let candidates = vec![
        cand("us-1", Some("US"), true, true, Some(20)),
        cand("jp-1", Some("JP"), true, true, Some(80)),
        cand("jp-2", Some("jp"), true, true, Some(45)),
    ];
    let constraint = PolicyConstraint {
        region: Some("JP".into()),
        fixed_exit: None,
        require_stable_egress: false,
    };
    match select_exit(&constraint, &candidates, FailureAction::Block) {
        PolicyDecision::Selected { device, rejected, .. } => {
            assert_eq!(device, "jp-2");
            assert_eq!(rejected.len(), 1);
            assert_eq!(rejected[0].device, "us-1");
            assert!(rejected[0].reason.contains("地域不匹配"));
        }
        other => panic!("期望选中 jp-2，实际：{other:?}"),
    }
}

/// TC-N3-02：地域硬约束无健康候选时严格阻断，绝不跨地域漂移或降级直连
#[test]
fn region_constraint_blocks_when_no_healthy_candidate() {
    let candidates = vec![
        cand("us-1", Some("US"), true, true, Some(20)),
        cand("jp-1", Some("JP"), true, false, Some(45)),
    ];
    let constraint = PolicyConstraint {
        region: Some("JP".into()),
        fixed_exit: None,
        require_stable_egress: false,
    };
    match select_exit(&constraint, &candidates, FailureAction::DirectIfAllowed) {
        PolicyDecision::Blocked { notify, rejected, .. } => {
            assert!(!notify);
            assert_eq!(rejected.len(), 2);
        }
        other => panic!("存在地域硬约束时绝不能降级直连或漂移：{other:?}"),
    }
}

/// TC-N3-03：账号型服务固定稳定出口故障时按 NotifyAndBlock 阻断告警，绝不擅自漂移
#[test]
fn stable_fixed_exit_notifies_and_blocks_without_drift() {
    let candidates = vec![
        cand("jp-vps", Some("JP"), true, false, None),
        cand("jp-backup", Some("JP"), true, true, Some(30)),
    ];
    let constraint = PolicyConstraint {
        region: None,
        fixed_exit: Some("jp-vps".into()),
        require_stable_egress: true,
    };
    match select_exit(&constraint, &candidates, FailureAction::NotifyAndBlock) {
        PolicyDecision::Blocked { notify, rejected, .. } => {
            assert!(notify);
            assert_eq!(rejected.len(), 2);
            assert!(rejected.iter().any(|r| r.device == "jp-vps" && r.reason.contains("不健康")));
            assert!(rejected.iter().any(|r| r.device == "jp-backup" && r.reason.contains("固定出口")));
        }
        other => panic!("固定稳定出口故障时必须告警并阻断：{other:?}"),
    }
}

/// TC-N3-04：DNS 白名单优先解除过滤、自定义改写生效，且 .internal 永不泄露
#[test]
fn dns_policy_allowlist_overrides_block_and_protects_internal() {
    let dev_ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5));
    let rules = vec![
        DnsRule {
            id: "block-ads".into(),
            matcher: Matcher::parse_domain("ads.example.com").unwrap(),
            action: DnsAction::BlockNxdomain,
        },
        DnsRule {
            id: "allow-ok-ad".into(),
            matcher: Matcher::parse_domain_exact("ok.ads.example.com").unwrap(),
            action: DnsAction::Allow,
        },
        DnsRule {
            id: "dev-override".into(),
            matcher: Matcher::parse_domain_exact("api.dev.test").unwrap(),
            action: DnsAction::Override { ips: vec![dev_ip], ttl: 30 },
        },
        DnsRule {
            id: "nodata-tracker".into(),
            matcher: Matcher::parse_domain("track.test").unwrap(),
            action: DnsAction::BlockNodata,
        },
    ];

    assert_eq!(evaluate_dns("web.home.internal", &rules), DnsDecision::PrivateInternal);
    assert_eq!(
        evaluate_dns("ok.ads.example.com", &rules),
        DnsDecision::Allowed { rule_id: "allow-ok-ad".into() }
    );
    assert_eq!(
        evaluate_dns("bad.ads.example.com", &rules),
        DnsDecision::BlockedNxdomain { rule_id: "block-ads".into() }
    );
    assert_eq!(
        evaluate_dns("api.dev.test", &rules),
        DnsDecision::Overridden { rule_id: "dev-override".into(), ips: vec![dev_ip], ttl: 30 }
    );
    assert_eq!(
        evaluate_dns("a.track.test", &rules),
        DnsDecision::BlockedNodata { rule_id: "nodata-tracker".into() }
    );
    assert_eq!(evaluate_dns("normal.example.org", &rules), DnsDecision::Default);
}

/// TC-N3-05：服务级健康分类不把 403 臆断为 IP 封禁
#[test]
fn service_health_does_not_misclassify_403_as_ip_ban() {
    assert_eq!(classify_http_status(200), (ServiceStatus::Healthy, None));
    assert_eq!(classify_http_status(429), (ServiceStatus::Degraded, Some(FailureClass::HttpRateLimit)));
    assert_eq!(classify_http_status(403), (ServiceStatus::PolicyRestricted, Some(FailureClass::Unknown)));
    assert_eq!(classify_http_status(503), (ServiceStatus::Unavailable, Some(FailureClass::AppError)));
}

/// TC-N3-06：本地脱敏流事件环形缓冲淘汰最旧记录并按最新优先返回
#[test]
fn flow_ring_evicts_oldest_and_preserves_order() {
    let mut ring = FlowRing::new(2);
    ring.record(FlowEvent { flow_id: 0, ts_ms: 100, ingress: "dns".into(), target: "a.test".into(), decision: "default".into(), exit_id: None, result: "ok".into(), error_class: None });
    ring.record(FlowEvent { flow_id: 0, ts_ms: 200, ingress: "dns".into(), target: "b.test".into(), decision: "blocked_nxdomain".into(), exit_id: None, result: "nxdomain".into(), error_class: Some(FailureClass::Dns) });
    ring.record(FlowEvent { flow_id: 0, ts_ms: 300, ingress: "tun-tcp".into(), target: "c.test:443".into(), decision: "exit".into(), exit_id: Some("jp".into()), result: "ok".into(), error_class: None });

    assert_eq!(ring.len(), 2);
    let recent = ring.recent(10);
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].target, "c.test:443");
    assert_eq!(recent[0].flow_id, 3);
    assert_eq!(recent[1].target, "b.test");
    assert_eq!(recent[1].flow_id, 2);
}
