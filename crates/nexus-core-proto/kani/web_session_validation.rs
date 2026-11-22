//! Kani harness: web session validation enforces all 6 conditions

#[cfg(kani)]
#[kani::proof]
fn web_session_validation_all_conditions() {
    use nexus_core_proto::authz::*;
    use nexus_core_proto::{NodeId, SecretKey, Timestamp, PublicKey};
    use nexus_core_proto::path::{Capability, CapabilityPattern, ResourcePath, ResourcePattern};
    use nexus_core_proto::roster::{Device, DeviceRole, Roster, WebKey};

    const MAX_WEB_SESSION_MS: u64 = 12 * 3600 * 1000; // 12 hours
    const MAX_SKEW: u64 = 300_000;

    // Fixed setup
    let root_sk = SecretKey::from_seed([1; 32]);
    let root_pk = root_sk.public();
    let issuer_id = NodeId([2; 32]);
    let browser_id = NodeId([3; 32]);
    let me = NodeId([4; 32]);

    let roster = Roster {
        root: root_pk,
        version: 1,
        devices: vec![Device {
            node_id: issuer_id,
            name: "issuer".into(),
            role: DeviceRole::Member,
            added_at: Timestamp(0),
        }],
        revoked_agents: vec![],
        web_keys: vec![WebKey {
            credential_id: vec![1, 2, 3, 4],
            label: "phone".into(),
            grants: vec![Grant {
                capability: CapabilityPattern::parse("*").unwrap(),
                resource: ResourcePattern::parse("**").unwrap(),
            }],
        }],
        anchor: None,
    };

    let action = Action {
        capability: Capability::parse("term.exec").unwrap(),
        resource: ResourcePath::parse("home").unwrap(),
    };

    let now = Timestamp(10_000_000);

    // Helper to build a valid web session
    let build_session = |issued_at: Timestamp, not_after: Timestamp, credential_id: Vec<u8>, browser: NodeId| {
        let session = WebSession {
            credential_id,
            browser,
            label: "phone".into(),
            issuer: issuer_id,
            issued_at,
            not_after,
        };
        let signature = issuer_id.public_key().sign(Purpose::WebSession, &canonical(&session));
        SignedWebSession { session, signature }
    };

    // Test each of the 6 deny conditions

    // 1. Issuer not in roster
    {
        let bad_issuer = NodeId([99; 32]);
        let mut session = WebSession {
            credential_id: vec![1, 2, 3, 4],
            browser: browser_id,
            label: "phone".into(),
            issuer: bad_issuer,
            issued_at: now,
            not_after: Timestamp(now.0 + 3600_000),
        };
        let signature = issuer_id.public_key().sign(Purpose::WebSession, &canonical(&session)); // signed by real issuer!
        let s = SignedWebSession { session, signature };

        let ctx = Context { roster: &roster, me, now };
        let res = check_web(&ctx, browser_id, &s, &action);
        assert_eq!(res, Err(DenyReason::IssuerNotInRoster));
    }

    // 2. Bad signature (signed by wrong key)
    {
        let wrong_sk = SecretKey::from_seed([5; 32]);
        let mut session = WebSession {
            credential_id: vec![1, 2, 3, 4],
            browser: browser_id,
            label: "phone".into(),
            issuer: issuer_id,
            issued_at: now,
            not_after: Timestamp(now.0 + 3600_000),
        };
        let signature = wrong_sk.sign(Purpose::WebSession, &canonical(&session));
        let s = SignedWebSession { session, signature };

        let ctx = Context { roster: &roster, me, now };
        let res = check_web(&ctx, browser_id, &s, &action);
        assert_eq!(res, Err(DenyReason::SessionBadSignature));
    }

    // 3. Wrong browser (peer != session.browser)
    {
        let wrong_browser = NodeId([99; 32]);
        let s = build_session(now, Timestamp(now.0 + 3600_000), vec![1, 2, 3, 4], browser_id);

        let ctx = Context { roster: &roster, me, now };
        let res = check_web(&ctx, wrong_browser, &s, &action);
        assert_eq!(res, Err(DenyReason::WrongBrowser));
    }

    // 4. TTL > MAX_WEB_SESSION_MS
    {
        let s = build_session(now, Timestamp(now.0 + MAX_WEB_SESSION_MS + 1), vec![1, 2, 3, 4], browser_id);

        let ctx = Context { roster: &roster, me, now };
        let res = check_web(&ctx, browser_id, &s, &action);
        assert_eq!(res, Err(DenyReason::SessionTooLong));
    }

    // 5. Not yet valid (issued_at > now + skew)
    {
        let future = Timestamp(now.0 + MAX_SKEW + 1);
        let s = build_session(future, Timestamp(future.0 + 3600_000), vec![1, 2, 3, 4], browser_id);

        let ctx = Context { roster: &roster, me, now };
        let res = check_web(&ctx, browser_id, &s, &action);
        assert_eq!(res, Err(DenyReason::SessionNotYetValid));
    }

    // 6. Expired (now >= not_after)
    {
        let s = build_session(Timestamp(now.0 - 3600_000), now, vec![1, 2, 3, 4], browser_id);

        let ctx = Context { roster: &roster, me, now };
        let res = check_web(&ctx, browser_id, &s, &action);
        assert_eq!(res, Err(DenyReason::SessionExpired));
    }

    // 7. Credential ID mismatch (web_key in roster has different credential_id)
    {
        let s = build_session(now, Timestamp(now.0 + 3600_000), vec![9, 9, 9, 9], browser_id); // different cred_id

        let ctx = Context { roster: &roster, me, now };
        let res = check_web(&ctx, browser_id, &s, &action);
        assert_eq!(res, Err(DenyReason::WebKeyRevoked));
    }

    // 8. No matching grant
    {
        let roster_no_grant = Roster {
            web_keys: vec![WebKey {
                credential_id: vec![1, 2, 3, 4],
                label: "phone".into(),
                grants: vec![Grant {
                    capability: CapabilityPattern::parse("drive.*").unwrap(), // only drive
                    resource: ResourcePattern::parse("**").unwrap(),
                }],
            }],
            ..roster.clone()
        };
        let s = build_session(now, Timestamp(now.0 + 3600_000), vec![1, 2, 3, 4], browser_id);
        let action_term = Action {
            capability: Capability::parse("term.exec").unwrap(),
            resource: ResourcePath::parse("home").unwrap(),
        };

        let ctx = Context { roster: &roster_no_grant, me, now };
        let res = check_web(&ctx, browser_id, &s, &action_term);
        assert_eq!(res, Err(DenyReason::NoMatchingGrant));
    }

    // 9. VALID case — all conditions satisfied
    {
        let s = build_session(now, Timestamp(now.0 + 3600_000), vec![1, 2, 3, 4], browser_id);

        let ctx = Context { roster: &roster, me, now };
        let res = check_web(&ctx, browser_id, &s, &action);
        assert_eq!(res, Ok(()), "Valid web session should be allowed");
    }
}