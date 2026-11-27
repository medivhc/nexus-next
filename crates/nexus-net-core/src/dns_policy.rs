//! 独立 DNS 改写与广告/追踪过滤管线（N3：U07）。
//!
//! 优先级：
//!   1. `.internal` 私有命名空间永远交由本地/授权服务解析，禁止向公网泄露；
//!   2. `Allow` 白名单优先于阻断/改写，支持对误杀站点局部解除过滤；
//!   3. 按规则顺序匹配 `Override` / `BlockNxdomain` / `BlockNodata`；
//!   4. 未命中则返回 `Default`，交由后续路由 FakeIP 或系统解析。

use crate::route::Matcher;
use crate::{Host, Target, SERVICE_SUFFIX};
use core::net::IpAddr;
use serde::{Deserialize, Serialize};

/// DNS 规则动作。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DnsAction {
    /// 白名单放行（优先于过滤规则，用于局部解除误杀）。
    Allow,
    /// 过滤并返回 NXDOMAIN。
    BlockNxdomain,
    /// 过滤并返回空 NOERROR（NODATA）。
    BlockNodata,
    /// 自定义 hosts / DNS 改写记录。
    Override { ips: Vec<IpAddr>, ttl: u32 },
}

/// 一条可解释、可单独增删的 DNS 规则。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsRule {
    pub id: String,
    pub matcher: Matcher,
    pub action: DnsAction,
}

/// DNS 规则表（本机持久化）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsRuleTable {
    pub rules: Vec<DnsRule>,
}

/// DNS 管线判定结果（附带命中规则 ID，便于可解释诊断）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DnsDecision {
    PrivateInternal,
    Allowed { rule_id: String },
    BlockedNxdomain { rule_id: String },
    BlockedNodata { rule_id: String },
    Overridden { rule_id: String, ips: Vec<IpAddr>, ttl: u32 },
    Default,
}

/// 对查询域名执行确定性 DNS 策略判定。
pub fn evaluate_dns(domain: &str, rules: &[DnsRule]) -> DnsDecision {
    let norm = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if norm == SERVICE_SUFFIX || norm.ends_with(&format!(".{SERVICE_SUFFIX}")) {
        return DnsDecision::PrivateInternal;
    }
    let target = Target {
        host: Host::Name(norm),
        port: 0,
    };

    // 先检查白名单（允许局部解除广告过滤）
    for rule in rules {
        if matches!(rule.action, DnsAction::Allow) && rule.matcher.matches(&target) {
            return DnsDecision::Allowed {
                rule_id: rule.id.clone(),
            };
        }
    }

    // 再按顺序执行改写或过滤
    for rule in rules {
        if !rule.matcher.matches(&target) {
            continue;
        }
        match &rule.action {
            DnsAction::Allow => {}
            DnsAction::BlockNxdomain => {
                return DnsDecision::BlockedNxdomain {
                    rule_id: rule.id.clone(),
                };
            }
            DnsAction::BlockNodata => {
                return DnsDecision::BlockedNodata {
                    rule_id: rule.id.clone(),
                };
            }
            DnsAction::Override { ips, ttl } => {
                return DnsDecision::Overridden {
                    rule_id: rule.id.clone(),
                    ips: ips.clone(),
                    ttl: *ttl,
                };
            }
        }
    }

    DnsDecision::Default
}
