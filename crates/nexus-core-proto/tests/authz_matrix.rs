//! 授权矩阵：穷举主体 × 授权形态 × 能力 × 资源，与一个独立写成的参考判定逐格比对。
//!
//! 每一种「有缺陷的主体」只带一个缺陷，所以参考判定不依赖检查顺序：
//! 有缺陷就拒绝并给出这个缺陷对应的原因；没有缺陷时才看授权是否匹配。
// covers: R12, R13, R14, R16, R17, R18

use nexus_core_proto::authz::*;
use nexus_core_proto::crypto::{canonical, Purpose};
use nexus_core_proto::path::*;
use nexus_core_proto::roster::*;
use nexus_core_proto::{NodeId, SecretKey, Timestamp};

const NOW: Timestamp = Timestamp(1_800_000_000_000);

fn root() -> SecretKey {
    SecretKey::from_seed([1; 32])
}
fn other_root() -> SecretKey {
    SecretKey::from_seed([2; 32])
}
fn agent_key() -> SecretKey {
    SecretKey::from_seed([3; 32])
}
fn other_agent() -> SecretKey {
    SecretKey::from_seed([4; 32])
}
const LAPTOP: NodeId = NodeId([10; 32]);
const HOME: NodeId = NodeId([11; 32]); // 本节点
const STRANGER: NodeId = NodeId([12; 32]);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    DeviceInRoster,
    DeviceStranger,
    AgentOk,
    AgentSkewEdgePast,
    AgentSkewEdgeFuture,
    AgentStartsNow,
    AgentViaStranger,
    AgentCertForged,
    AgentCertTampered,
    AgentCertRosterPurpose,
    AgentNotYetValid,
    AgentExpiredExactly,
    AgentRevoked,
    AgentAssertionByOtherKey,
    AgentAssertionCertPurpose,
    AgentWrongTarget,
    AgentActionMismatch,
    AgentStalePast,
    AgentStaleFuture,
}

const KINDS: [Kind; 19] = [
    Kind::DeviceInRoster,
    Kind::DeviceStranger,
    Kind::AgentOk,
    Kind::AgentSkewEdgePast,
    Kind::AgentSkewEdgeFuture,
    Kind::AgentStartsNow,
    Kind::AgentViaStranger,
    Kind::AgentCertForged,
    Kind::AgentCertTampered,
    Kind::AgentCertRosterPurpose,
    Kind::AgentNotYetValid,
    Kind::AgentExpiredExactly,
    Kind::AgentRevoked,
    Kind::AgentAssertionByOtherKey,
    Kind::AgentAssertionCertPurpose,
    Kind::AgentWrongTarget,
    Kind::AgentActionMismatch,
    Kind::AgentStalePast,
    Kind::AgentStaleFuture,
];

const GRANT_SHAPES: [&[(&str, &str)]; 7] = [
    &[],
    &[("term.exec", "home")],
    &[("term.*", "home/**")],
    &[("*", "*/session/*")],
    &[("node.info", "office")],
    &[
        ("node.info", "office"),
        ("term.session.*", "home/session/*"),
    ],
    &[("*", "**")],
];
const CAPS: [&str; 3] = ["term.exec", "term.session.attach", "node.info"];
const RESOURCES: [&str; 5] = [
    "home",
    "home/session/main",
    "home/files",
    "office",
    "office/session/x",
];

fn action(cap: &str, res: &str) -> Action {
    Action {
        capability: Capability::parse(cap).unwrap(),
        resource: ResourcePath::parse(res).unwrap(),
    }
}

fn grants(shape: &[(&str, &str)]) -> Vec<Grant> {
    shape
        .iter()
        .map(|(c, r)| Grant {
            capability: CapabilityPattern::parse(c).unwrap(),
            resource: ResourcePattern::parse(r).unwrap(),
        })
        .collect()
}

fn roster(kind: Kind) -> Roster {
    let dev = |id, name: &str| Device {
        node_id: id,
        name: name.into(),
        role: DeviceRole::Member,
        added_at: Timestamp(0),
    };
    let mut r = Roster {
        root: root().public(),
        version: 3,
        devices: vec![dev(LAPTOP, "laptop"), dev(HOME, "home")],
        revoked_agents: vec![],
        web_keys: vec![],
        anchor: None,
    };
    if kind == Kind::AgentRevoked {
        r.revoked_agents.push(RevokedAgent {
            agent: agent_key().public(),
            revoked_at: Timestamp(NOW.0 - 1),
        });
    }
    r
}

/// 构造某种主体，返回承载请求的对端设备和主体本身。
fn build(kind: Kind, shape: &[(&str, &str)], act: &Action) -> (NodeId, Principal) {
    use Kind::*;
    match kind {
        DeviceInRoster => return (LAPTOP, Principal::Device),
        DeviceStranger => return (STRANGER, Principal::Device),
        _ => {}
    }
    let mut cert = AgentCert {
        agent: agent_key().public(),
        label: "builder".into(),
        not_before: Timestamp(NOW.0 - 60_000),
        not_after: Timestamp(NOW.0 + 60_000),
        grants: grants(shape),
    };
    match kind {
        AgentNotYetValid => cert.not_before = Timestamp(NOW.0 + 1),
        AgentExpiredExactly => cert.not_after = NOW,
        AgentStartsNow => cert.not_before = NOW,
        _ => {}
    }
    let mut signed = match kind {
        AgentCertForged => cert.issue(&other_root()),
        AgentCertRosterPurpose => {
            let signature = root().sign(Purpose::Roster, &canonical(&cert));
            SignedAgentCert { cert, signature }
        }
        _ => cert.issue(&root()),
    };
    if kind == AgentCertTampered {
        // 签名之后再改：加一条全权授权，并把有效期延长一天。两处都一定改变了内容。
        signed.cert.grants.extend(grants(&[("*", "**")]));
        signed.cert.not_after = Timestamp(signed.cert.not_after.0 + 86_400_000);
    }

    let signed_action = if kind == AgentActionMismatch {
        // 与实际请求只差一处
        let other_res = if act.resource.as_str() == "office" {
            "home"
        } else {
            "office"
        };
        action(act.capability.as_str(), other_res)
    } else {
        act.clone()
    };
    let issued_at = match kind {
        AgentStalePast => Timestamp(NOW.0 - MAX_CLOCK_SKEW_MS - 1),
        AgentStaleFuture => Timestamp(NOW.0 + MAX_CLOCK_SKEW_MS + 1),
        AgentSkewEdgePast => Timestamp(NOW.0 - MAX_CLOCK_SKEW_MS),
        AgentSkewEdgeFuture => Timestamp(NOW.0 + MAX_CLOCK_SKEW_MS),
        _ => NOW,
    };
    let assertion = Assertion {
        target: if kind == AgentWrongTarget {
            LAPTOP
        } else {
            HOME
        },
        action: signed_action,
        issued_at,
        nonce: [9; 16],
    };
    let signature = match kind {
        AgentAssertionByOtherKey => assertion.sign(&other_agent()),
        AgentAssertionCertPurpose => agent_key().sign(Purpose::AgentCert, &canonical(&assertion)),
        _ => assertion.sign(&agent_key()),
    };
    let peer = if kind == AgentViaStranger {
        STRANGER
    } else {
        LAPTOP
    };
    (
        peer,
        Principal::Agent {
            cert: signed,
            assertion,
            signature,
        },
    )
}

// ---- 参考判定：与正式实现分开写，只依赖字符串 ----

fn ref_resource_match(pat: &[&str], path: &[&str]) -> bool {
    match (pat.split_first(), path.split_first()) {
        (None, None) => true,
        (Some((&"**", _)), _) => true,
        (Some((p, prest)), Some((q, qrest))) if *p == "*" || p == q => {
            ref_resource_match(prest, qrest)
        }
        _ => false,
    }
}

fn ref_cap_match(pat: &[&str], cap: &[&str]) -> bool {
    match (pat.split_first(), cap.split_first()) {
        (None, None) => true,
        (Some((&"*", [])), Some(_)) => true,
        (Some((p, prest)), Some((q, qrest))) if p == q => ref_cap_match(prest, qrest),
        _ => false,
    }
}

fn reference(kind: Kind, shape: &[(&str, &str)], cap: &str, res: &str) -> Outcome {
    use DenyReason::*;
    use Kind::*;
    let deny = |r| Outcome::Deny(r);
    match kind {
        DeviceInRoster => Outcome::Allow,
        DeviceStranger | AgentViaStranger => deny(PeerNotInRoster),
        AgentCertForged | AgentCertTampered | AgentCertRosterPurpose => deny(CertBadSignature),
        AgentNotYetValid => deny(CertNotYetValid),
        AgentExpiredExactly => deny(CertExpired),
        Kind::AgentRevoked => deny(DenyReason::AgentRevoked),
        AgentAssertionByOtherKey | AgentAssertionCertPurpose => deny(AssertionBadSignature),
        AgentWrongTarget => deny(WrongTarget),
        AgentActionMismatch => deny(ActionMismatch),
        AgentStalePast | AgentStaleFuture => deny(Stale),
        AgentOk | AgentSkewEdgePast | AgentSkewEdgeFuture | AgentStartsNow => {
            let rsegs: Vec<&str> = res.split('/').collect();
            let csegs: Vec<&str> = cap.split('.').collect();
            let hit = shape.iter().any(|(gc, gr)| {
                ref_cap_match(&gc.split('.').collect::<Vec<_>>(), &csegs)
                    && ref_resource_match(&gr.split('/').collect::<Vec<_>>(), &rsegs)
            });
            if hit {
                Outcome::Allow
            } else {
                deny(NoMatchingGrant)
            }
        }
    }
}

#[test]
fn matrix_matches_reference() {
    let mut checked = 0;
    let mut allowed = 0;
    let mut mismatches = Vec::new();
    for kind in KINDS {
        let roster = roster(kind);
        let ctx = Context {
            roster: &roster,
            me: HOME,
            now: NOW,
        };
        for shape in GRANT_SHAPES {
            for cap in CAPS {
                for res in RESOURCES {
                    let act = action(cap, res);
                    let (peer, principal) = build(kind, shape, &act);
                    let verdict = decide(&ctx, peer, &principal, &act);
                    let want = reference(kind, shape, cap, res);
                    if verdict.outcome != want {
                        mismatches.push(format!(
                            "{kind:?} {shape:?} {cap} {res}: 得到 {:?}，应为 {want:?}",
                            verdict.outcome
                        ));
                    }
                    // 每次判定都附带完整的审计事件
                    let a = &verdict.audit;
                    assert_eq!(a.at, NOW);
                    assert_eq!(a.peer, peer);
                    assert_eq!(a.capability, cap);
                    assert_eq!(a.resource, res);
                    assert_eq!(a.outcome, verdict.outcome);
                    match (&principal, &a.principal) {
                        (Principal::Device, PrincipalRef::Device(id)) => assert_eq!(*id, peer),
                        (Principal::Agent { cert, .. }, PrincipalRef::Agent { id, label }) => {
                            assert_eq!(*id, cert.cert.agent);
                            assert_eq!(label, "builder");
                        }
                        other => panic!("审计中的主体与请求不符：{other:?}"),
                    }
                    assert_eq!(verdict.allowed(), verdict.outcome == Outcome::Allow);
                    checked += 1;
                    allowed += (want == Outcome::Allow) as usize;
                }
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} 处不一致：\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
    assert_eq!(
        checked,
        KINDS.len() * GRANT_SHAPES.len() * CAPS.len() * RESOURCES.len()
    );
    // 矩阵里既要有大量允许，也要有大量拒绝，否则说明构造本身出了问题
    assert!(
        allowed > 100 && checked - allowed > 1000,
        "允许 {allowed} / 共 {checked}"
    );
}

#[test]
fn visibility_follows_grants() {
    // covers: R15
    let act = action("node.info", "home");
    let (_, p) = build(Kind::AgentOk, &[("term.exec", "home/session/build")], &act);
    let Principal::Agent { cert, .. } = p else {
        unreachable!()
    };
    let see = |s: &str| cert.can_see(&ResourcePath::parse(s).unwrap());
    assert!(see("home"));
    assert!(see("home/session/build"));
    assert!(!see("home/session/other"));
    assert!(!see("office"));

    let (_, p) = build(Kind::AgentOk, &[], &act);
    let Principal::Agent { cert, .. } = p else {
        unreachable!()
    };
    assert!(!cert.can_see(&ResourcePath::parse("home").unwrap()));
}
