//! Executable reference model for M1.5 agent authorization (bounded pilot).
//! Run: cargo test --locked -p nexus-core-proto --test authz_model -- --nocapture
//! The reference model uses a finite set of facts, not production authorization helpers.
use nexus_core_proto::authz::{decide, Action, AgentCert, Assertion, Context, Grant, Principal};
use nexus_core_proto::path::{Capability, CapabilityPattern, ResourcePath, ResourcePattern};
use nexus_core_proto::roster::{Device, DeviceRole, RevokedAgent, Roster};
use nexus_core_proto::{NodeId, SecretKey, Timestamp};
use std::collections::{HashSet, VecDeque};

const HOME: NodeId = NodeId([11; 32]);
const PEER: NodeId = NodeId([10; 32]);

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct State {
    peer_member: bool,
    old_revoked: bool,
    fresh_issued: bool,
    // Before validity, valid, exactly at expiry.
    clock: u8,
}

#[derive(Clone, Copy, Debug)]
enum Event {
    Advance,
    RevokeOld,
    IssueFresh,
    RemovePeer,
}

impl State {
    fn step(self, event: Event) -> Option<Self> {
        let mut next = self;
        match event {
            Event::Advance if self.clock < 2 => next.clock += 1,
            Event::RevokeOld if !self.old_revoked => next.old_revoked = true,
            Event::IssueFresh if self.old_revoked && !self.fresh_issued && self.clock == 1 => {
                next.fresh_issued = true;
            }
            Event::RemovePeer if self.peer_member => next.peer_member = false,
            _ => return None,
        }
        Some(next)
    }

    fn now(self) -> Timestamp {
        Timestamp([999, 1_000, 2_000][usize::from(self.clock)])
    }
}

#[derive(Clone, Copy, Debug)]
enum Credential {
    Old,
    Fresh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    Granted,
    WrongCapability,
    WrongResource,
    WrongTarget,
    WrongSigner,
    TamperedGrant,
}

// Contract (M1.5 FP-02/05/06/07): only a valid, unrevoked credential
// on a member peer, for exactly the granted and correctly signed action.
// Revocation is scoped to the key, not the human-readable label.
// This is a local-node contract AFTER the updated roster has arrived.
fn model_allows(s: State, credential: Credential, request: Request) -> bool {
    let identity_valid = match credential {
        Credential::Old => !s.old_revoked,
        Credential::Fresh => s.fresh_issued,
    };
    s.peer_member && s.clock == 1 && identity_valid && request == Request::Granted
}

fn action(cap: &str, path: &str) -> Action {
    Action {
        capability: Capability::parse(cap).unwrap(),
        resource: ResourcePath::parse(path).unwrap(),
    }
}

// Adapter: real keys, real signed certificates/assertions and real decide().
// ignore_revocation is a test-only negative control; production code is untouched.
fn implementation_allows(
    s: State,
    credential: Credential,
    request: Request,
    ignore_revocation: bool,
) -> bool {
    let root = SecretKey::from_seed([1; 32]);
    let old = SecretKey::from_seed([2; 32]);
    let fresh = SecretKey::from_seed([3; 32]);
    let signer = SecretKey::from_seed([4; 32]);
    let key = match credential {
        Credential::Old => &old,
        Credential::Fresh => &fresh,
    };
    let roster = Roster {
        root: root.public(),
        version: if s.old_revoked { 2 } else { 1 },
        devices: if s.peer_member {
            vec![Device {
                node_id: PEER,
                name: "laptop".into(),
                role: DeviceRole::Member,
                added_at: Timestamp(0),
            }]
        } else {
            vec![]
        },
        revoked_agents: if s.old_revoked && !ignore_revocation {
            vec![RevokedAgent { agent: old.public(), revoked_at: s.now() }]
        } else {
            vec![]
        },
        web_keys: vec![],
        anchor: None,
    };
    let mut cert = AgentCert {
        agent: key.public(),
        label: "builder".into(),
        not_before: Timestamp(1_000),
        not_after: Timestamp(2_000),
        grants: vec![Grant {
            capability: CapabilityPattern::parse("term.exec").unwrap(),
            resource: ResourcePattern::parse("home").unwrap(),
        }],
    }
    .issue(&root);
    let requested = match request {
        Request::WrongCapability => action("term.session.list", "home"),
        Request::WrongResource | Request::TamperedGrant => action("term.exec", "nas"),
        _ => action("term.exec", "home"),
    };
    if request == Request::TamperedGrant {
        cert.cert.grants[0].resource = ResourcePattern::parse("**").unwrap();
    }
    let assertion = Assertion {
        target: if request == Request::WrongTarget { NodeId([12; 32]) } else { HOME },
        action: requested.clone(),
        issued_at: s.now(),
        nonce: [1; 16],
    };
    let signature = assertion.sign(if request == Request::WrongSigner { &signer } else { key });
    let principal = Principal::Agent { cert, assertion, signature };
    let ctx = Context { roster: &roster, me: HOME, now: s.now() };
    let verdict = decide(&ctx, PEER, &principal, &requested);
    assert_eq!(verdict.outcome, verdict.audit.outcome, "audit must match authorization");
    verdict.allowed()
}

#[derive(Debug)]
struct Counterexample {
    trace: Vec<Event>,
    state: State,
    credential: Credential,
    request: Request,
    expected: bool,
    observed: bool,
}

fn explore(ignore_revocation: bool) -> Result<(usize, usize), Counterexample> {
    let initial = State { peer_member: true, old_revoked: false, fresh_issued: false, clock: 0 };
    let mut queue = VecDeque::from([(initial, Vec::new())]);
    let mut seen = HashSet::from([initial]);
    let mut checks = 0;
    while let Some((state, trace)) = queue.pop_front() {
        for credential in [Credential::Old, Credential::Fresh] {
            // No certificate exists for a fresh identity until the issue event.
            if matches!(credential, Credential::Fresh) && !state.fresh_issued {
                continue;
            }
            for request in [
                Request::Granted,
                Request::WrongCapability,
                Request::WrongResource,
                Request::WrongTarget,
                Request::WrongSigner,
                Request::TamperedGrant,
            ] {
                checks += 1;
                let expected = model_allows(state, credential, request);
                let observed = implementation_allows(state, credential, request, ignore_revocation);
                if expected != observed {
                    return Err(Counterexample { trace, state, credential, request, expected, observed });
                }
            }
        }
        for event in [Event::Advance, Event::RevokeOld, Event::IssueFresh, Event::RemovePeer] {
            if let Some(next) = state.step(event) {
                if seen.insert(next) {
                    let mut next_trace = trace.clone();
                    next_trace.push(event);
                    queue.push_back((next, next_trace));
                }
            }
        }
    }
    Ok((seen.len(), checks))
}

#[test]
fn executable_model_matches_real_authorization() {
    // covers: R12, R13, R14, R16, R17
    let (states, checks) = explore(false).unwrap_or_else(|ce| panic!("counterexample: {ce:#?}"));
    assert_eq!(states, 16, "finite model coverage changed");
    assert_eq!(checks, 120, "request coverage changed");
    println!("authz model: {states} reachable states, {checks} real authorization comparisons");
}

#[test]
fn executable_model_rejects_revocation_negative_control() {
    let ce = explore(true).expect_err("model must detect missing revocation");
    assert!(!ce.expected && ce.observed);
    assert!(ce.state.old_revoked && ce.state.peer_member);
    assert!(matches!(ce.credential, Credential::Old));
    assert_eq!(ce.request, Request::Granted);
    assert!(ce.trace.iter().any(|e| matches!(e, Event::RevokeOld)));
    println!("negative control counterexample: {ce:#?}");
}
