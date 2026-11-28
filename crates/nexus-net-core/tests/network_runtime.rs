// covers: R41, R46, R87, R100, R101, R103, R104
//! N1/N2 纯逻辑测试用例（对应 docs/test-cases/network-runtime-n0-n2-test-cases.md）。

use nexus_net_core::app_policy::AppPolicy;
use nexus_net_core::import::{parse_clash_rules, ImportOptions};
use nexus_net_core::power::{PowerMode, PowerPolicy};
use nexus_net_core::route::{audit_rules, decide, AuditKind, Decision, Matcher, Outbound, Rule};
use nexus_net_core::Target;

/// TC-N1-01：精确域名匹配（DomainExact）与后缀匹配（Domain）隔离
#[test]
fn domain_exact_does_not_match_subdomains() {
    let rules = vec![
        Rule {
            matcher: Matcher::parse_domain_exact("api.example.com").unwrap(),
            outbound: Outbound::Exit("jp".into()),
        },
        Rule {
            matcher: Matcher::parse_domain("example.com").unwrap(),
            outbound: Outbound::Direct,
        },
    ];
    assert_eq!(
        decide(&Target::parse("api.example.com:443").unwrap(), &rules),
        Decision::Rule { index: 0, outbound: Outbound::Exit("jp".into()) }
    );
    assert_eq!(
        decide(&Target::parse("API.EXAMPLE.COM.:443").unwrap(), &rules),
        Decision::Rule { index: 0, outbound: Outbound::Exit("jp".into()) }
    );
    assert_eq!(
        decide(&Target::parse("sub.api.example.com:443").unwrap(), &rules),
        Decision::Rule { index: 1, outbound: Outbound::Direct }
    );
    assert_eq!(
        decide(&Target::parse("www.example.com:80").unwrap(), &rules),
        Decision::Rule { index: 1, outbound: Outbound::Direct }
    );
}

/// TC-N1-02：规则静态审计检测遮蔽、重复与无效上游
#[test]
fn audit_detects_shadowed_duplicate_and_invalid_rules() {
    let rules = vec![
        Rule {
            matcher: Matcher::parse_domain("example.com").unwrap(),
            outbound: Outbound::Direct,
        },
        Rule {
            matcher: Matcher::parse_domain_exact("api.example.com").unwrap(),
            outbound: Outbound::Exit("jp".into()),
        },
        Rule {
            matcher: Matcher::parse_domain("example.com").unwrap(),
            outbound: Outbound::Reject,
        },
        Rule {
            matcher: Matcher::parse_cidr("10.0.0.0/8").unwrap(),
            outbound: Outbound::Direct,
        },
        Rule {
            matcher: Matcher::parse_cidr("10.1.2.0/24").unwrap(),
            outbound: Outbound::Exit("home".into()),
        },
        Rule {
            matcher: Matcher::parse_domain("bad.test").unwrap(),
            outbound: Outbound::Socks5("not-a-valid-addr".into()),
        },
    ];
    let audit = audit_rules(&rules);
    assert!(!audit.is_clean());
    assert!(audit
        .issues
        .iter()
        .any(|i| i.rule_index == 2 && i.kind == AuditKind::Shadowed && i.shadowed_by == Some(1)));
    assert!(audit
        .issues
        .iter()
        .any(|i| i.rule_index == 3 && i.kind == AuditKind::Duplicate && i.shadowed_by == Some(1)));
    assert!(audit
        .issues
        .iter()
        .any(|i| i.rule_index == 5 && i.kind == AuditKind::Shadowed && i.shadowed_by == Some(4)));
    assert!(audit
        .issues
        .iter()
        .any(|i| i.rule_index == 6 && i.kind == AuditKind::InvalidUpstream));
}

/// TC-N1-03：Clash / Mihomo 规则子集可审计导入，忽略并报告不支持项与高风险段落
#[test]
fn clash_import_parses_supported_subset_and_reports_skipped() {
    let yaml = r#"
port: 7890
mitm:
  enabled: true
rules:
  - DOMAIN,claude.ai,JP-Proxy
  - DOMAIN-SUFFIX,cn,DIRECT
  - DOMAIN-SUFFIX,ads.example.com,REJECT # 广告过滤
  - IP-CIDR,192.168.0.0/16,DIRECT,no-resolve
  - IP-CIDR6,fd00::/8,DIRECT,no-resolve
  - GEOIP,CN,DIRECT
  - SCRIPT,check,JP-Proxy
  - PROCESS-NAME,curl,DIRECT
  - MATCH,JP-Proxy
"#;
    let opts = ImportOptions {
        default_exit: Some("jp-vps".into()),
        default_socks5: None,
    };
    let report = parse_clash_rules(yaml, &opts);
    assert_eq!(report.imported.len(), 5);
    assert_eq!(
        report.imported[0],
        Rule {
            matcher: Matcher::DomainExact("claude.ai".into()),
            outbound: Outbound::Exit("jp-vps".into()),
        }
    );
    assert_eq!(
        report.imported[1],
        Rule {
            matcher: Matcher::Domain("cn".into()),
            outbound: Outbound::Direct,
        }
    );
    assert_eq!(
        report.imported[2],
        Rule {
            matcher: Matcher::Domain("ads.example.com".into()),
            outbound: Outbound::Reject,
        }
    );
    assert_eq!(report.skipped.len(), 3);
    assert!(report.skipped.iter().any(|s| s.raw.starts_with("GEOIP")));
    assert!(report.skipped.iter().any(|s| s.raw.starts_with("SCRIPT")));
    assert!(report.skipped.iter().any(|s| s.raw.starts_with("PROCESS-NAME")));
    assert!(report.notes.iter().any(|n| n.contains("MATCH")));
    assert!(report.notes.iter().any(|n| n.contains("mitm")));
}

/// TC-N1-04：未配置出口映射的代理组绝不静默降级为直连
#[test]
fn clash_import_never_silently_downgrades_unmapped_proxy_to_direct() {
    let yaml = "rules:\n  - DOMAIN,secret.example.com,JP-Group\n  - DOMAIN,local.example.com,DIRECT\n";
    let report = parse_clash_rules(yaml, &ImportOptions::default());
    assert_eq!(report.imported.len(), 1);
    assert_eq!(report.imported[0].outbound, Outbound::Direct);
    assert_eq!(report.skipped.len(), 1);
    assert!(report.skipped[0].reason.contains("不降级直连"));
}

/// TC-N1-10：Android 应用绕过策略归一化，始终强制包含自身包名防 VPN 环路
#[test]
fn app_policy_always_includes_self_and_sanitizes() {
    let policy = AppPolicy::parse_lines(
        "# 微信与支付宝直连\ncom.tencent.mm\n  \ncom.tencent.mm, com.eg.android.AlipayGphone\nbad pkg!\nno_dot\n",
    );
    let effective = policy.effective_disallowed_packages("link.bewith.nexus");
    assert_eq!(
        effective,
        vec![
            "com.eg.android.AlipayGphone".to_string(),
            "com.tencent.mm".to_string(),
            "link.bewith.nexus".to_string(),
        ]
    );
    // 即使列表为空，也必须强制排除自身
    let empty = AppPolicy::default().effective_disallowed_packages("link.bewith.nexus");
    assert_eq!(empty, vec!["link.bewith.nexus".to_string()]);
}

/// TC-N2-01：电源模式策略默认 Balanced 不保活蜂窝，仅 Realtime 显式开启
#[test]
fn power_policy_defaults_to_balanced_without_cellular_keepalive() {
    assert_eq!(PowerMode::default(), PowerMode::Balanced);
    assert_eq!(PowerMode::parse("unknown-value"), PowerMode::Balanced);
    assert_eq!(PowerMode::parse("powersave"), PowerMode::PowerSave);
    assert_eq!(PowerMode::parse("realtime"), PowerMode::Realtime);

    let balanced = PowerPolicy::for_mode(PowerMode::Balanced);
    assert!(!balanced.keep_cellular_with_wifi);
    assert!(!balanced.allow_multipath_redundancy);
    assert_eq!(balanced.idle_probe_interval_secs, 60);

    let powersave = PowerPolicy::for_mode(PowerMode::PowerSave);
    assert!(!powersave.keep_cellular_with_wifi);
    assert!(!powersave.allow_multipath_redundancy);
    assert_eq!(powersave.idle_probe_interval_secs, 180);

    let realtime = PowerPolicy::for_mode(PowerMode::Realtime);
    assert!(realtime.keep_cellular_with_wifi);
    assert!(realtime.allow_multipath_redundancy);
    assert_eq!(realtime.idle_probe_interval_secs, 15);
}
