//! Kani harness: clock skew boundary is exactly MAX_CLOCK_SKEW_MS (300,000ms)

#[cfg(kani)]
#[kani::proof]
fn clock_skew_boundary() {
    use nexus_core_proto::authz::*;
    use nexus_core_proto::{NodeId, SecretKey, Timestamp, PublicKey};
    use nexus_core_proto::path::{Capability, CapabilityPattern, ResourcePath, ResourcePattern};
    use nexus_core_proto::roster::{Device, DeviceRole, Roster};

    const MAX_SKEW: u64 = 300_000; // MAX_CLOCK_SKEW_MS

    // Fixed valid setup
    let root_sk = SecretKey::from_seed([1; 32]);
    let root_pk = root_sk.public();
    let peer_id = NodeId([2; 32]);
    let me = NodeId([3; 32]);
    let agent_sk = SecretKey::from_seed([4; 32]);
    let agent_pk = agent_sk.public();

    let roster = Roster {
        root: root_pk,
        version: 1,
        devices: vec![Device {
            node_id: peer_id,
            name: "peer".into(),
            role: DeviceRole::Member,
            added_at: Timestamp(0),
        }],
        revoked_agents: vec![],
        web_keys: vec![],
        anchor: None,
    };

    let cert = AgentCert {
        agent: agent_pk,
        label: "test".into(),
        not_before: Timestamp(0),
        not_after: Timestamp(u64::MAX),
        grants: vec![Grant {
            capability: CapabilityPattern::parse("*").unwrap(),
            resource: ResourcePattern::parse("**").unwrap(),
        }],
    }.issue(&root_sk);

    let action = Action {
        capability: Capability::parse("term.exec").unwrap(),
        resource: ResourcePath::parse("home").unwrap(),
    };

    let now = Timestamp(10_000_000); // fixed "now"

    // Test the boundary: offset from now
    // Allow at: -MAX_SKEW, -MAX_SKEW+1, ..., MAX_SKEW-1, MAX_SKEW
    // Deny at: -MAX_SKEW-1, MAX_SKEW+1

    let offsets: [i64; 6] = [
        -(MAX_SKEW as i64) - 1,  // stale (deny)
        -(MAX_SKEW as i64),      // boundary (allow)
        -1,                       // allow
        0,                        // allow
        MAX_SKEW as i64,         // boundary (allow)
        (MAX_SKEW as i64) + 1,   // stale (deny)
    ];

    for (i, offset) in offsets.iter().enumerate() {
        let issued_at = Timestamp((now.0 as i64 + *offset) as u64);
        let assertion = Assertion {
            target: me,
            action: action.clone(),
            issued_at,
            nonce: [i as u8; 16],
        };
        let sig = assertion.sign(&agent_sk);
        let principal = Principal::Agent {
            cert: cert.clone(),
            assertion,
            signature: sig,
        };

        let ctx = Context { roster: &roster, me, now };
        let verdict = decide(&ctx, peer_id, &principal, &action);

        let expect_allow = offset.abs() <= MAX_SKEW as i64;
        match (verdict.outcome, expect_allow) {
            (Outcome::Allow, true) => {} // correct
            (Outcome::Deny(DenyReason::Stale), false) => {} // correct
            (Outcome::Allow, false) => {
                panic!("UNEXPECTED ALLOW at offset {} (expected Stale)", offset);
            }
            (Outcome::Deny(DenyReason::Stale), true) => {
                panic!("UNEXPECTED STALE at offset {} (expected Allow)", offset);
            }
            (other, _) => {
                // Other deny reasons possible but not expected here (cert valid, target matches, grant matches)
                panic!("UNEXPECTED OUTCOME at offset {:?}: {:?}", offset, other);
            }
        }
    }
}