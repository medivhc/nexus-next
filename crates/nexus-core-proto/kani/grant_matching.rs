//! Kani harness: grant pattern matching implements documented semantics

#[cfg(kani)]
#[kani::proof]
fn capability_pattern_matches_spec() {
    use nexus_core_proto::path::{Capability, CapabilityPattern};

    // Capability: segments separated by '.', each segment [a-zA-Z0-9_-]{1,64}
    // Pattern: same, but last segment may be '*' which matches ≥1 segments

    // Valid capabilities to test
    let caps = [
        Capability::parse("term.exec").unwrap(),
        Capability::parse("term.session.attach").unwrap(),
        Capability::parse("drive.push").unwrap(),
        Capability::parse("drive.sync.files").unwrap(),
        Capability::parse("node.info").unwrap(),
        Capability::parse("a").unwrap(),
        Capability::parse(&"a.".repeat(10) + "b").unwrap(), // 10 segments
    ];

    // Valid patterns
    let patterns = [
        CapabilityPattern::parse("*").unwrap(),           // matches all
        CapabilityPattern::parse("term.*").unwrap(),      // matches term.exec, term.session.attach, etc.
        CapabilityPattern::parse("drive.*").unwrap(),     // matches drive.push, drive.sync.files
        CapabilityPattern::parse("node.*").unwrap(),      // matches node.info
        CapabilityPattern::parse("term.exec").unwrap(),   // exact match
        CapabilityPattern::parse("drive.push").unwrap(),  // exact match
        CapabilityPattern::parse("nonexistent.*").unwrap(), // matches nothing in our set
    ];

    for pat in &patterns {
        for cap in &caps {
            let matches = pat.matches(cap);
            let expected = match (pat.as_str(), cap.as_str()) {
                ("*", _) => true,
                ("term.*", c) => c.starts_with("term.") && c != "term",
                ("drive.*", c) => c.starts_with("drive.") && c != "drive",
                ("node.*", c) => c.starts_with("node.") && c != "node",
                (p, c) => p == c,
            };
            assert_eq!(matches, expected,
                "CapabilityPattern::matches mismatch: pat={} cap={} got={} expected={}",
                pat.as_str(), cap.as_str(), matches, expected);
        }
    }

    // Property: '*' at end matches ≥1 segments, not 0
    assert!(!CapabilityPattern::parse("term.*").unwrap().matches(&Capability::parse("term").unwrap()));
    assert!(CapabilityPattern::parse("term.*").unwrap().matches(&Capability::parse("term.x").unwrap()));
    assert!(CapabilityPattern::parse("term.*").unwrap().matches(&Capability::parse("term.x.y").unwrap()));

    // Property: exact match without * requires identical segments
    assert!(CapabilityPattern::parse("term.exec").unwrap().matches(&Capability::parse("term.exec").unwrap()));
    assert!(!CapabilityPattern::parse("term.exec").unwrap().matches(&Capability::parse("term.exec.more").unwrap()));
    assert!(!CapabilityPattern::parse("term.exec").unwrap().matches(&Capability::parse("term").unwrap()));
}

#[kani::proof]
fn resource_pattern_matches_spec() {
    use nexus_core_proto::path::{ResourcePath, ResourcePattern};

    // Resource: segments separated by '/', each [a-zA-Z0-9_-]{1,64}
    // Pattern: '*' matches exactly one segment, '**' only at end matches ≥0 segments

    let paths = [
        ResourcePath::parse("home").unwrap(),
        ResourcePath::parse("home/session").unwrap(),
        ResourcePath::parse("home/session/build").unwrap(),
        ResourcePath::parse("office").unwrap(),
        ResourcePath::parse("office/files").unwrap(),
        ResourcePath::parse("office/files/doc").unwrap(),
    ];

    let patterns = [
        ResourcePattern::parse("home").unwrap(),
        ResourcePattern::parse("home/*").unwrap(),
        ResourcePattern::parse("home/**").unwrap(),
        ResourcePattern::parse("*/session/*").unwrap(),
        ResourcePattern::parse("**").unwrap(),
        ResourcePattern::parse("office/**").unwrap(),
        ResourcePattern::parse("office/*").unwrap(),
    ];

    for pat in &patterns {
        for path in &paths {
            let matches = pat.matches(path);
            let expected = match (pat.as_str(), path.as_str()) {
                ("home", "home") => true,
                ("home", _) => false,
                ("home/*", p) => p.split('/').count() == 2 && p.starts_with("home/"),
                ("home/**", p) => p == "home" || p.starts_with("home/"),
                ("*/session/*", p) => {
                    let segs: Vec<&str> = p.split('/').collect();
                    segs.len() == 3 && segs[1] == "session"
                }
                ("**", _) => true,
                ("office/**", p) => p == "office" || p.starts_with("office/"),
                ("office/*", p) => p.split('/').count() == 2 && p.starts_with("office/"),
                _ => false,
            };
            assert_eq!(matches, expected,
                "ResourcePattern::matches mismatch: pat={} path={} got={} expected={}",
                pat.as_str(), path.as_str(), matches, expected);
        }
    }

    // Property: '*' matches exactly one segment
    assert!(ResourcePattern::parse("home/*").unwrap().matches(&ResourcePath::parse("home/session").unwrap()));
    assert!(!ResourcePattern::parse("home/*").unwrap().matches(&ResourcePath::parse("home").unwrap()));
    assert!(!ResourcePattern::parse("home/*").unwrap().matches(&ResourcePath::parse("home/session/build").unwrap()));

    // Property: '**' at end matches zero or more segments
    assert!(ResourcePattern::parse("home/**").unwrap().matches(&ResourcePath::parse("home").unwrap()));
    assert!(ResourcePattern::parse("home/**").unwrap().matches(&ResourcePath::parse("home/session").unwrap()));
    assert!(ResourcePattern::parse("home/**").unwrap().matches(&ResourcePath::parse("home/session/build").unwrap()));
    assert!(!ResourcePattern::parse("home/**").unwrap().matches(&ResourcePath::parse("homeoffice").unwrap()));

    // Property: '**' only allowed at end (enforced by parse, but verify)
    assert!(ResourcePattern::parse("a/**/b").is_err());
}

#[kani::proof]
fn may_match_within_spec() {
    use nexus_core_proto::path::{ResourcePath, ResourcePattern};

    // may_match_within(prefix) = ∃ path ≥ prefix that pattern matches
    // Used for "can this principal see this directory?"

    let test_cases = [
        // (pattern, prefix, expected)
        ("home/session/build", "home", true),
        ("home/session/build", "home/session", true),
        ("home/session/build", "home/session/build", true),
        ("home/session/build", "home/session/other", false),
        ("home/session/build", "home/session/build/deeper", false),
        ("home/session/build", "office", false),
        ("*/session/*", "office", true),
        ("home/**", "home/a/b/c", true),
        ("home/**", "home", true),
        ("home/**", "office", false),
    ];

    for (pat_str, prefix_str, expected) in test_cases {
        let pat = ResourcePattern::parse(pat_str).unwrap();
        let prefix = ResourcePath::parse(prefix_str).unwrap();
        let result = pat.may_match_within(&prefix);
        assert_eq!(result, expected,
            "may_match_within mismatch: pat={} prefix={} got={} expected={}",
            pat_str, prefix_str, result, expected);
    }
}