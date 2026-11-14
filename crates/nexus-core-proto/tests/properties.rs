//! 属性测试：在随机输入上检查不变量。

use nexus_core_proto::path::*;
use nexus_core_proto::roster::*;
use nexus_core_proto::{NodeId, SecretKey, Timestamp};
use proptest::prelude::*;

fn ref_resource_match(pat: &[String], path: &[String]) -> bool {
    match (pat.split_first(), path.split_first()) {
        (None, None) => true,
        (Some((p, _)), _) if p == "**" => true,
        (Some((p, prest)), Some((q, qrest))) if p == "*" || p == q => ref_resource_match(prest, qrest),
        _ => false,
    }
}

fn ref_cap_match(pat: &[String], cap: &[String]) -> bool {
    match (pat.split_first(), cap.split_first()) {
        (None, None) => true,
        (Some((p, rest)), Some(_)) if p == "*" && rest.is_empty() => true,
        (Some((p, prest)), Some((q, qrest))) if p == q => ref_cap_match(prest, qrest),
        _ => false,
    }
}

fn segs() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(prop::sample::select(vec!["a", "b", "c"]).prop_map(String::from), 1..5)
}

fn resource_pattern() -> impl Strategy<Value = Vec<String>> {
    (
        prop::collection::vec(prop::sample::select(vec!["a", "b", "*"]).prop_map(String::from), 0..4),
        any::<bool>(),
    )
        .prop_filter_map("模式不能为空", |(mut v, tail)| {
            if tail {
                v.push("**".into());
            }
            (!v.is_empty()).then_some(v)
        })
}

fn capability_pattern() -> impl Strategy<Value = Vec<String>> {
    (
        prop::collection::vec(prop::sample::select(vec!["a", "b"]).prop_map(String::from), 0..4),
        any::<bool>(),
    )
        .prop_filter_map("模式不能为空", |(mut v, tail)| {
            if tail {
                v.push("*".into());
            }
            (!v.is_empty()).then_some(v)
        })
}

/// 给出一个能被模式匹配、并且位于 `prefix` 之下的具体路径，前提是模式在 prefix 处还没被用完。
fn witness(pat: &[String], prefix: &[String]) -> Option<Vec<String>> {
    if pat.len() < prefix.len() && pat.last().map(String::as_str) != Some("**") {
        return None;
    }
    let mut out = prefix.to_vec();
    for p in pat.iter().skip(prefix.len()) {
        match p.as_str() {
            "**" => break,
            "*" => out.push("z".into()),
            other => out.push(other.to_string()),
        }
    }
    Some(out)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    // covers: R12
    #[test]
    fn resource_matching_agrees_with_reference(pat in resource_pattern(), path in segs()) {
        let p = ResourcePattern::parse(&pat.join("/")).unwrap();
        let r = ResourcePath::parse(&path.join("/")).unwrap();
        prop_assert_eq!(p.matches(&r), ref_resource_match(&pat, &path));
    }

    // covers: R12
    #[test]
    fn capability_matching_agrees_with_reference(pat in capability_pattern(), cap in segs()) {
        let p = CapabilityPattern::parse(&pat.join(".")).unwrap();
        let c = Capability::parse(&cap.join(".")).unwrap();
        prop_assert_eq!(p.matches(&c), ref_cap_match(&pat, &cap));
    }

    /// 能看到 = 存在一个位于其下且被匹配的路径。两个方向都检查。
    // covers: R15
    #[test]
    fn visibility_is_exact(pat in resource_pattern(), prefix in segs(), suffix in prop::collection::vec(prop::sample::select(vec!["a", "b", "z"]).prop_map(String::from), 0..3)) {
        let p = ResourcePattern::parse(&pat.join("/")).unwrap();
        let pre = ResourcePath::parse(&prefix.join("/")).unwrap();
        let within = p.may_match_within(&pre);
        // 没有漏报：任何被匹配的下级路径都意味着可见
        let mut full = prefix.clone();
        full.extend(suffix);
        if p.matches(&ResourcePath::parse(&full.join("/")).unwrap()) {
            prop_assert!(within, "{} 匹配 {}，却判定看不到 {}", pat.join("/"), full.join("/"), prefix.join("/"));
        }
        // 没有误报：判定可见时，能构造出一个被匹配的下级路径
        if within {
            let w = witness(&pat, &prefix);
            prop_assert!(w.as_ref().is_some_and(|w| ref_resource_match(&pat, w)),
                "{} 被判定在 {} 之下可见，但找不到被匹配的路径", pat.join("/"), prefix.join("/"));
        }
    }

    /// 无论以什么顺序收到，最终停在合法名册中版本最高的那一份；伪造的从不被接受。
    // covers: R02, R18
    #[test]
    fn roster_updates_converge_to_highest_version(
        versions in prop::collection::btree_set(1u64..50, 1..6),
        forged in prop::collection::vec(1u64..60, 0..3),
        seed in any::<u64>(),
    ) {
        let root = SecretKey::from_seed([1; 32]);
        let other = SecretKey::from_seed([2; 32]);
        let make = |v: u64, key: &SecretKey| Roster {
            root: root.public(),
            version: v,
            devices: vec![Device { node_id: NodeId([v as u8; 32]), name: format!("d{v}"), role: DeviceRole::Member, added_at: Timestamp(v) }],
            revoked_agents: vec![],
            web_keys: vec![],
        }.sign(key);
        let mut all: Vec<(SignedRoster, bool)> = versions.iter().map(|v| (make(*v, &root), true)).collect();
        all.extend(forged.iter().map(|v| (make(*v, &other), false)));
        // 用种子确定的方式打乱顺序
        let mut s = seed;
        for i in (1..all.len()).rev() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            all.swap(i, (s >> 33) as usize % (i + 1));
        }
        let mut current: Option<SignedRoster> = None;
        for (candidate, genuine) in &all {
            if accept_update(current.as_ref(), candidate, &root.public()).is_ok() {
                prop_assert!(*genuine, "接受了伪造的名册");
                current = Some(candidate.clone());
            }
        }
        prop_assert_eq!(current.map(|c| c.roster.version), versions.iter().max().copied());
    }
}
