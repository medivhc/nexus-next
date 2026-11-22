//! Kani harness: ReplayGuard rejects second use, forgets old entries

#[cfg(kani)]
#[kani::proof]
fn replay_guard_rejects_second_use() {
    use nexus_core_proto::authz::*;
    use nexus_core_proto::{PublicKey, Timestamp};
    use nexus_core_proto::path::{Action, Capability, ResourcePath};

    const MAX_SKEW: u64 = 300_000;

    let agent_a = PublicKey::from([1; 33]); // secp256k1 compressed
    let agent_b = PublicKey::from([2; 33]);
    let action = Action {
        capability: Capability::parse("term.exec").unwrap(),
        resource: ResourcePath::parse("home").unwrap(),
    };

    let now = Timestamp(100 * MAX_SKEW);
    let mut guard = ReplayGuard::default();

    // First use: (agent_a, nonce=1) at `now`
    assert!(guard.first_use(&agent_a, &Assertion {
        target: NodeId([0; 32]),
        action: action.clone(),
        issued_at: now,
        nonce: [1; 16],
    }, now), "first use should succeed");

    // Second use of same (agent, nonce) at same `now` → reject
    assert!(!guard.first_use(&agent_a, &Assertion {
        target: NodeId([0; 32]),
        action: action.clone(),
        issued_at: now,
        nonce: [1; 16],
    }, now), "second use of same (agent, nonce) should be rejected");

    // Different nonce for same agent → accept
    assert!(guard.first_use(&agent_a, &Assertion {
        target: NodeId([0; 32]),
        action: action.clone(),
        issued_at: now,
        nonce: [2; 16],
    }, now), "different nonce should succeed");

    // Different agent, same nonce → accept
    assert!(guard.first_use(&agent_b, &Assertion {
        target: NodeId([0; 32]),
        action: action.clone(),
        issued_at: now,
        nonce: [1; 16],
    }, now), "different agent should succeed");

    assert_eq!(guard.len(), 3);
}

#[kani::proof]
fn replay_guard_forgets_old_entries() {
    use nexus_core_proto::authz::*;
    use nexus_core_proto::{PublicKey, Timestamp};
    use nexus_core_proto::path::{Action, Capability, ResourcePath};

    const MAX_SKEW: u64 = 300_000;

    let agent = PublicKey::from([1; 33]);
    let action = Action {
        capability: Capability::parse("term.exec").unwrap(),
        resource: ResourcePath::parse("home").unwrap(),
    };

    let now = Timestamp(100 * MAX_SKEW);
    let mut guard = ReplayGuard::default();

    // Record entry at `now`
    assert!(guard.first_use(&agent, &Assertion {
        target: NodeId([0; 32]),
        action: action.clone(),
        issued_at: now,
        nonce: [1; 16],
    }, now));

    // Advance to exactly 2*MAX_SKEW later — entry at horizon should still be remembered
    let later = Timestamp(now.0 + 2 * MAX_SKEW);
    assert!(!guard.first_use(&agent, &Assertion {
        target: NodeId([0; 32]),
        action: action.clone(),
        issued_at: now,
        nonce: [1; 16],
    }, later), "at horizon (2*skew) should still reject");

    // Advance one more millisecond — entry should be forgotten
    let much_later = Timestamp(later.0 + 1);
    assert!(guard.first_use(&agent, &Assertion {
        target: NodeId([0; 32]),
        action: action.clone(),
        issued_at: now,
        nonce: [1; 16],
    }, much_later), "past horizon (2*skew+1) should accept again");

    // Only the new entry should remain
    assert_eq!(guard.len(), 1);
}