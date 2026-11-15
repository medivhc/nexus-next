//! 网页登录的身份：名册里的通行密钥、登录挑战、网页会话凭证的判定（M2.1，R28、R29、R31、R33 的基础）。

use nexus_core_proto::authz::{
    decide, Action, Context, DenyReason, Grant, Outcome, Principal, PrincipalRef, WebSession, MAX_WEB_SESSION_MS,
};
use nexus_core_proto::path::{Capability, CapabilityPattern, ResourcePath, ResourcePattern};
use nexus_core_proto::roster::{Device, DeviceRole, Roster, RosterError, WebKey};
use nexus_core_proto::webauthn::soft::SoftAuthenticator;
use nexus_core_proto::webauthn::{login_challenge, P256Key};
use nexus_core_proto::{NodeId, SecretKey, Timestamp};

const HOUR: u64 = 3600 * 1000;

fn root() -> SecretKey {
    SecretKey::from_seed([1; 32])
}
fn laptop_key() -> SecretKey {
    SecretKey::from_seed([2; 32])
}
fn home_key() -> SecretKey {
    SecretKey::from_seed([3; 32])
}
fn id(k: &SecretKey) -> NodeId {
    NodeId(k.public().0)
}
const BROWSER: NodeId = NodeId([9; 32]);

fn grant(cap: &str, res: &str) -> Grant {
    Grant { capability: CapabilityPattern::parse(cap).unwrap(), resource: ResourcePattern::parse(res).unwrap() }
}

fn web_key(label: &str) -> WebKey {
    let auth = SoftAuthenticator::new([7; 32], "bewith.link", "https://bewith.link");
    WebKey {
        label: label.into(),
        credential_id: auth.credential_id.clone(),
        public_key: P256Key::from_spki(&auth.spki()).unwrap(),
        rp_id: "bewith.link".into(),
        grants: vec![grant("term.session.*", "**"), grant("node.info", "**")],
        added_at: Timestamp(0),
    }
}

fn roster(web_keys: Vec<WebKey>) -> Roster {
    let dev = |k: &SecretKey, name: &str, role| Device { node_id: id(k), name: name.into(), role, added_at: Timestamp(0) };
    Roster {
        root: root().public(),
        version: 1,
        devices: vec![dev(&laptop_key(), "laptop", DeviceRole::Admin), dev(&home_key(), "home", DeviceRole::Member)],
        revoked_agents: vec![],
        web_keys,
    }
}

fn action(cap: &str, res: &str) -> Action {
    Action { capability: Capability::parse(cap).unwrap(), resource: ResourcePath::parse(res).unwrap() }
}

/// laptop 验证了通行密钥后签发的网页会话凭证。
fn session(now: u64, hours: u64) -> Principal {
    let s = WebSession {
        credential_id: web_key("phone").credential_id,
        browser: BROWSER,
        label: "phone".into(),
        issuer: id(&laptop_key()),
        issued_at: Timestamp(now),
        not_after: Timestamp(now + hours * HOUR),
    };
    Principal::Web { session: s.sign(&laptop_key()) }
}

fn judge(r: &Roster, now: u64, peer: NodeId, p: &Principal, a: &Action) -> Outcome {
    let ctx = Context { roster: r, me: id(&home_key()), now: Timestamp(now) };
    decide(&ctx, peer, p, a).outcome
}

#[test]
fn a_web_session_allows_what_the_passkey_was_granted() {
    let r = roster(vec![web_key("phone")]);
    let now = 100 * HOUR;
    let p = session(now, 12);
    assert_eq!(judge(&r, now, BROWSER, &p, &action("term.session.attach", "home/session/main")), Outcome::Allow);
    assert_eq!(judge(&r, now, BROWSER, &p, &action("node.info", "home")), Outcome::Allow);
    assert_eq!(judge(&r, now, BROWSER, &p, &action("term.exec", "home")), Outcome::Deny(DenyReason::NoMatchingGrant));
    // 审计记录里的主体是通行密钥的名字，经由的是浏览器。
    let ctx = Context { roster: &r, me: id(&home_key()), now: Timestamp(now) };
    let v = decide(&ctx, BROWSER, &p, &action("node.info", "home"));
    assert_eq!(v.audit.principal, PrincipalRef::Web { label: "phone".into() });
    assert_eq!(v.audit.peer, BROWSER);
}

#[test]
fn the_session_is_bound_to_the_browser_that_logged_in() {
    let r = roster(vec![web_key("phone")]);
    let now = 100 * HOUR;
    let other = NodeId([8; 32]);
    assert_eq!(judge(&r, now, other, &session(now, 1), &action("node.info", "home")), Outcome::Deny(DenyReason::WrongBrowser));
}

#[test]
fn only_a_roster_device_can_issue_a_session() {
    let r = roster(vec![web_key("phone")]);
    let now = 100 * HOUR;
    let stranger = SecretKey::from_seed([5; 32]);
    let s = WebSession { credential_id: web_key("phone").credential_id, browser: BROWSER, label: "phone".into(), issuer: id(&stranger), issued_at: Timestamp(now), not_after: Timestamp(now + HOUR) };
    let p = Principal::Web { session: s.clone().sign(&stranger) };
    assert_eq!(judge(&r, now, BROWSER, &p, &action("node.info", "home")), Outcome::Deny(DenyReason::IssuerNotInRoster));
    // 声称由 laptop 签发，实际不是。
    let forged = WebSession { issuer: id(&laptop_key()), ..s };
    let p = Principal::Web { session: forged.sign(&stranger) };
    assert_eq!(judge(&r, now, BROWSER, &p, &action("node.info", "home")), Outcome::Deny(DenyReason::SessionBadSignature));
    // 签发后改动内容。
    let Principal::Web { mut session } = session(now, 1) else { unreachable!() };
    session.session.label = "kiosk".into();
    assert_eq!(
        judge(&roster(vec![web_key("phone"), web_key("kiosk")]), now, BROWSER, &Principal::Web { session }, &action("node.info", "home")),
        Outcome::Deny(DenyReason::SessionBadSignature)
    );
}

#[test]
fn sessions_expire_and_cannot_be_longer_than_twelve_hours() {
    assert_eq!(MAX_WEB_SESSION_MS, 12 * HOUR, "上限是规格的一部分");
    let r = roster(vec![web_key("phone")]);
    let now = 100 * HOUR;
    let a = action("node.info", "home");
    assert_eq!(judge(&r, now + 12 * HOUR - 1, BROWSER, &session(now, 12), &a), Outcome::Allow);
    assert_eq!(judge(&r, now + 12 * HOUR, BROWSER, &session(now, 12), &a), Outcome::Deny(DenyReason::SessionExpired));
    assert_eq!(judge(&r, now, BROWSER, &session(now, 13), &a), Outcome::Deny(DenyReason::SessionTooLong));
    // 签发时间在未来：容许 5 分钟的时钟偏差，再多就不行。
    let skew = 5 * 60 * 1000;
    assert_eq!(judge(&r, now - skew, BROWSER, &session(now, 1), &a), Outcome::Allow);
    assert_eq!(judge(&r, now - skew - 1, BROWSER, &session(now, 1), &a), Outcome::Deny(DenyReason::SessionNotYetValid));
    assert_eq!(judge(&r, now - HOUR, BROWSER, &session(now, 1), &a), Outcome::Deny(DenyReason::SessionNotYetValid));
}

#[test]
fn removing_the_passkey_from_the_roster_ends_its_sessions() {
    let now = 100 * HOUR;
    let p = session(now, 12);
    let a = action("node.info", "home");
    assert_eq!(judge(&roster(vec![]), now, BROWSER, &p, &a), Outcome::Deny(DenyReason::WebKeyRevoked));
    // 权限以名册里当前的为准：收窄后立即生效。
    let mut narrowed = web_key("phone");
    narrowed.grants = vec![grant("node.info", "laptop")];
    assert_eq!(judge(&roster(vec![narrowed]), now, BROWSER, &p, &a), Outcome::Deny(DenyReason::NoMatchingGrant));
}

#[test]
fn roster_rejects_bad_web_keys() {
    let bad = |keys: Vec<WebKey>| roster(keys).sign(&root()).verify(&root().public()).map(|_| ()).unwrap_err();
    assert_eq!(bad(vec![web_key("phone"), web_key("phone")]), RosterError::DuplicateName("phone".into()));
    assert_eq!(bad(vec![web_key("my phone")]), RosterError::InvalidName("my phone".into()));
    // 通行密钥不能和设备重名：资源路径和审计里的名字必须无歧义。
    assert_eq!(bad(vec![web_key("home")]), RosterError::DuplicateName("home".into()));
    assert!(roster(vec![web_key("phone")]).sign(&root()).verify(&root().public()).is_ok());
}

#[test]
fn login_challenge_binds_time_and_browser() {
    // 浏览器在知道入口设备之前就要发起通行密钥签名（入口写在通行密钥里，签完才知道），
    // 所以挑战只绑定时间和浏览器这次的临时身份；只有持有这个临时身份的浏览器才能使用签名。
    let base = login_challenge(Timestamp(1000), BROWSER);
    assert_eq!(base, login_challenge(Timestamp(1000), BROWSER));
    assert_ne!(base, login_challenge(Timestamp(1001), BROWSER));
    assert_ne!(base, login_challenge(Timestamp(1000), NodeId([8; 32])));
}

#[test]
fn reusing_a_label_does_not_reauthorize_a_previous_credential() {
    let now = 100 * HOUR;
    let old = session(now, 12);
    let mut replacement = web_key("phone");
    replacement.credential_id = vec![8; 16];
    assert_eq!(
        judge(&roster(vec![replacement]), now, BROWSER, &old, &action("node.info", "home")),
        Outcome::Deny(DenyReason::WebKeyRevoked)
    );
}
