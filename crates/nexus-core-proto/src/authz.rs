//! 授权判定：谁可以对什么资源使用什么能力。
//!
//! 判定发生在被访问的节点上。输入是传输层验证过的对端设备、请求声明的主体、
//! 本机的名册和当前时间；输出是允许或拒绝，以及一条审计事件。

use crate::crypto::{canonical, PublicKey, Purpose, SecretKey, Signature};
use crate::path::{Capability, CapabilityPattern, ResourcePath, ResourcePattern};
use crate::roster::Roster;
use crate::{NodeId, Timestamp};
use serde::{Deserialize, Serialize};

/// agent 请求的签署时间与当前时间允许相差的最大毫秒数。
pub const MAX_CLOCK_SKEW_MS: u64 = 5 * 60 * 1000;

/// 一次网页登录最长的有效期：12 小时。
pub const MAX_WEB_SESSION_MS: u64 = 12 * 3600 * 1000;

/// 网页会话凭证：一台名册内的设备验证了浏览器的通行密钥之后签发，
/// 证明「这个浏览器（以它这次的临时节点标识）是通行密钥 label 的持有者」。
/// 浏览器拿着它直接访问其他设备，只需要用一次通行密钥。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSession {
    /// 签发时验证的通行密钥，不能仅按可复用的名字识别。
    pub credential_id: Vec<u8>,
    pub browser: NodeId,
    pub label: String,
    pub issuer: NodeId,
    pub issued_at: Timestamp,
    pub not_after: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedWebSession {
    pub session: WebSession,
    pub signature: Signature,
}

impl WebSession {
    pub fn sign(self, issuer: &SecretKey) -> SignedWebSession {
        let signature = issuer.sign(Purpose::WebSession, &canonical(&self));
        SignedWebSession { session: self, signature }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub capability: CapabilityPattern,
    pub resource: ResourcePattern,
}

impl Grant {
    pub fn allows(&self, action: &Action) -> bool {
        self.capability.matches(&action.capability) && self.resource.matches(&action.resource)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCert {
    pub agent: PublicKey,
    pub label: String,
    /// 生效时间（含）。
    pub not_before: Timestamp,
    /// 失效时间（不含）。
    pub not_after: Timestamp,
    pub grants: Vec<Grant>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedAgentCert {
    pub cert: AgentCert,
    pub signature: Signature,
}

impl AgentCert {
    /// 用根密钥签发。
    pub fn issue(self, root: &SecretKey) -> SignedAgentCert {
        let signature = root.sign(Purpose::AgentCert, &canonical(&self));
        SignedAgentCert { cert: self, signature }
    }
}

impl SignedAgentCert {
    /// 校验证书本身，不要求某个具体动作（用于设备发现和长连接复查）。
    pub fn validate(&self, roster: &Roster, now: Timestamp) -> Result<(), DenyReason> {
        use DenyReason::*;
        let c = &self.cert;
        if !roster.root.verify(Purpose::AgentCert, &canonical(c), &self.signature) {
            return Err(CertBadSignature);
        }
        if now < c.not_before {
            return Err(CertNotYetValid);
        }
        if now >= c.not_after {
            return Err(CertExpired);
        }
        if roster.is_agent_revoked(&c.agent) {
            return Err(AgentRevoked);
        }
        Ok(())
    }

    /// 这张证书的授权是否让它能看到 `path` 或 `path` 之下的东西。
    /// 只看授权条目，不检查证书本身是否有效；有效性由 [`decide`] 负责。
    pub fn can_see(&self, path: &ResourcePath) -> bool {
        self.cert.grants.iter().any(|g| g.resource.may_match_within(path))
    }
}

/// 一次访问要做的事。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    pub capability: Capability,
    pub resource: ResourcePath,
}

/// agent 对一次具体访问的声明，由 agent 自己的密钥签名。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assertion {
    /// 请求发往的节点。拿到别的节点上使用会被拒绝。
    pub target: NodeId,
    pub action: Action,
    pub issued_at: Timestamp,
    /// 供运行时做重放检测（M1.5）。
    pub nonce: [u8; 16],
}

impl Assertion {
    pub fn sign(&self, agent: &SecretKey) -> Signature {
        agent.sign(Purpose::AgentAssertion, &canonical(self))
    }
}

/// 请求声明的主体。
// 每个请求只构造一次，几百字节无关紧要；装箱反而让调用方的模式匹配变复杂。
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Principal {
    /// 对端设备以它自己的身份请求，代表用户本人。
    Device,
    /// 一个 agent，经由对端设备发出请求。
    Agent { cert: SignedAgentCert, assertion: Assertion, signature: Signature },
    /// 网页登录：对端是浏览器，凭一张网页会话凭证。
    Web { session: SignedWebSession },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DenyReason {
    /// 承载请求的设备不在名册里（从未加入，或已被吊销）。
    PeerNotInRoster,
    CertBadSignature,
    CertNotYetValid,
    CertExpired,
    AgentRevoked,
    AssertionBadSignature,
    /// 请求是发给另一个节点的。
    WrongTarget,
    /// 签署的动作与实际请求的动作不一致。
    ActionMismatch,
    /// 签署时间离当前时间太远。
    Stale,
    NoMatchingGrant,
    /// 同一个签过名的请求已经用过一次。由运行时的 [`ReplayGuard`] 发现。
    Replayed,
    /// 网页会话凭证的签发者不在名册里。
    IssuerNotInRoster,
    SessionBadSignature,
    SessionNotYetValid,
    SessionExpired,
    /// 凭证的有效期超过了 [`MAX_WEB_SESSION_MS`]。
    SessionTooLong,
    /// 对端不是凭证里写的那个浏览器。
    WrongBrowser,
    /// 通行密钥已不在名册里。
    WebKeyRevoked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Allow,
    Deny(DenyReason),
}

/// 审计记录里对主体的描述。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrincipalRef {
    Device(NodeId),
    Agent { id: PublicKey, label: String },
    Web { label: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub at: Timestamp,
    pub peer: NodeId,
    pub principal: PrincipalRef,
    pub capability: String,
    pub resource: String,
    pub outcome: Outcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub outcome: Outcome,
    pub audit: AuditEvent,
}

impl Verdict {
    pub fn allowed(&self) -> bool {
        self.outcome == Outcome::Allow
    }
}

/// 记住近期见过的 agent 请求，拒绝重复使用。
///
/// 只需要记住签署时间离现在不超过 [`MAX_CLOCK_SKEW_MS`] 的请求：更早的会被判定为过时。
#[derive(Default, Debug)]
pub struct ReplayGuard {
    seen: std::collections::BTreeMap<(PublicKey, [u8; 16]), Timestamp>,
}

impl ReplayGuard {
    /// 第一次见到这个请求时记下并返回 `true`；见过则返回 `false`。
    pub fn first_use(&mut self, agent: &PublicKey, assertion: &Assertion, now: Timestamp) -> bool {
        // 签署时间早于「现在减去两倍容差」的记录不可能再通过判定，可以忘掉。
        let horizon = now.0.saturating_sub(2 * MAX_CLOCK_SKEW_MS);
        self.seen.retain(|_, issued| issued.0 >= horizon);
        match self.seen.entry((*agent, assertion.nonce)) {
            std::collections::btree_map::Entry::Occupied(_) => false,
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert(assertion.issued_at);
                true
            }
        }
    }

    /// 当前记住的请求数。
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// 判定所需的本机状态。
pub struct Context<'a> {
    pub roster: &'a Roster,
    /// 本节点。
    pub me: NodeId,
    pub now: Timestamp,
}

/// 判定一次访问。
pub fn decide(ctx: &Context<'_>, peer: NodeId, principal: &Principal, action: &Action) -> Verdict {
    let outcome = match check(ctx, peer, principal, action) {
        Ok(()) => Outcome::Allow,
        Err(reason) => Outcome::Deny(reason),
    };
    let principal = match principal {
        Principal::Device => PrincipalRef::Device(peer),
        Principal::Agent { cert, .. } => {
            PrincipalRef::Agent { id: cert.cert.agent, label: cert.cert.label.clone() }
        }
        Principal::Web { session } => PrincipalRef::Web { label: session.session.label.clone() },
    };
    let audit = AuditEvent {
        at: ctx.now,
        peer,
        principal,
        capability: action.capability.as_str().to_string(),
        resource: action.resource.as_str().to_string(),
        outcome: outcome.clone(),
    };
    Verdict { outcome, audit }
}

fn check(ctx: &Context<'_>, peer: NodeId, principal: &Principal, action: &Action) -> Result<(), DenyReason> {
    use DenyReason::*;
    if let Principal::Web { session } = principal {
        return check_web(ctx, peer, session, action);
    }
    // 设备和 agent：承载请求的设备必须在名册里。
    ctx.roster.device(peer).ok_or(PeerNotInRoster)?;
    let Principal::Agent { cert, assertion, signature } = principal else {
        // 名册内的设备代表用户本人。
        return Ok(());
    };
    let c = &cert.cert;
    cert.validate(ctx.roster, ctx.now)?;
    if !c.agent.verify(Purpose::AgentAssertion, &canonical(assertion), signature) {
        return Err(AssertionBadSignature);
    }
    if assertion.target != ctx.me {
        return Err(WrongTarget);
    }
    if assertion.action != *action {
        return Err(ActionMismatch);
    }
    if ctx.now.0.abs_diff(assertion.issued_at.0) > MAX_CLOCK_SKEW_MS {
        return Err(Stale);
    }
    if !c.grants.iter().any(|g| g.allows(action)) {
        return Err(NoMatchingGrant);
    }
    Ok(())
}

/// 网页会话：对端是浏览器而不是名册内的设备，凭签发设备的签名和名册里的通行密钥判定。
fn check_web(ctx: &Context<'_>, peer: NodeId, s: &SignedWebSession, action: &Action) -> Result<(), DenyReason> {
    use DenyReason::*;
    let w = &s.session;
    ctx.roster.device(w.issuer).ok_or(IssuerNotInRoster)?;
    if !w.issuer.public_key().verify(Purpose::WebSession, &canonical(w), &s.signature) {
        return Err(SessionBadSignature);
    }
    if peer != w.browser {
        return Err(WrongBrowser);
    }
    if w.not_after.0.saturating_sub(w.issued_at.0) > MAX_WEB_SESSION_MS {
        return Err(SessionTooLong);
    }
    if ctx.now.0.saturating_add(MAX_CLOCK_SKEW_MS) < w.issued_at.0 {
        return Err(SessionNotYetValid);
    }
    if ctx.now >= w.not_after {
        return Err(SessionExpired);
    }
    let key = ctx.roster.web_key(&w.label).filter(|k| k.credential_id == w.credential_id).ok_or(WebKeyRevoked)?;
    if !key.grants.iter().any(|g| g.allows(action)) {
        return Err(NoMatchingGrant);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roster::{Device, DeviceRole};

    /// 时钟偏差的上限是规格的一部分（5 分钟），用字面量写死，而不是引用常量。
    #[test]
    fn clock_skew_limit_is_five_minutes() {
        // covers: R16
        let root = SecretKey::from_seed([1; 32]);
        let agent = SecretKey::from_seed([3; 32]);
        let me = NodeId([11; 32]);
        let peer = NodeId([10; 32]);
        let roster = Roster {
            root: root.public(),
            version: 1,
            devices: vec![Device { node_id: peer, name: "laptop".into(), role: DeviceRole::Member, added_at: Timestamp(0) }],
            revoked_agents: vec![],
            web_keys: vec![],
            anchor: None,
        };
        let action = Action {
            capability: Capability::parse("term.exec").unwrap(),
            resource: ResourcePath::parse("home").unwrap(),
        };
        let cert = AgentCert {
            agent: agent.public(),
            label: "t".into(),
            not_before: Timestamp(0),
            not_after: Timestamp(u64::MAX),
            grants: vec![Grant {
                capability: CapabilityPattern::parse("*").unwrap(),
                resource: ResourcePattern::parse("**").unwrap(),
            }],
        }
        .issue(&root);
        let now = Timestamp(10_000_000);
        let at = |offset_ms: i64| {
            let assertion = Assertion {
                target: me,
                action: action.clone(),
                issued_at: Timestamp((now.0 as i64 + offset_ms) as u64),
                nonce: [0; 16],
            };
            let signature = assertion.sign(&agent);
            let p = Principal::Agent { cert: cert.clone(), assertion, signature };
            decide(&Context { roster: &roster, me, now }, peer, &p, &action).outcome
        };
        for ok in [-299_999, -300_000, 0, 300_000] {
            assert_eq!(at(ok), Outcome::Allow, "偏差 {ok} 毫秒应当允许");
        }
        for stale in [-300_001, 300_001, -3_600_000] {
            assert_eq!(at(stale), Outcome::Deny(DenyReason::Stale), "偏差 {stale} 毫秒应当拒绝");
        }
    }

    #[test]
    fn replay_guard_rejects_second_use_and_forgets_old_requests() {
        let a = SecretKey::from_seed([3; 32]).public();
        let b = SecretKey::from_seed([4; 32]).public();
        let action = Action {
            capability: Capability::parse("term.exec").unwrap(),
            resource: ResourcePath::parse("home").unwrap(),
        };
        let assertion = |nonce: u8, at: u64| Assertion {
            target: NodeId([1; 32]),
            action: action.clone(),
            issued_at: Timestamp(at),
            nonce: [nonce; 16],
        };
        let now = Timestamp(100 * MAX_CLOCK_SKEW_MS);
        let mut g = ReplayGuard::default();
        assert!(g.is_empty());
        assert!(g.first_use(&a, &assertion(1, now.0), now));
        assert!(!g.first_use(&a, &assertion(1, now.0), now), "同一个请求第二次使用");
        assert!(g.first_use(&a, &assertion(2, now.0), now), "不同的随机数");
        assert!(g.first_use(&b, &assertion(1, now.0), now), "不同的 agent");
        assert_eq!(g.len(), 3);

        // 签署时间恰好在「现在减两倍容差」上的还要记住，再早的可以忘掉。
        let later = Timestamp(now.0 + 2 * MAX_CLOCK_SKEW_MS);
        assert!(!g.first_use(&a, &assertion(1, now.0), later));
        let much_later = Timestamp(later.0 + 1);
        assert!(g.first_use(&a, &assertion(9, much_later.0), much_later));
        assert_eq!(g.len(), 1, "过时的记录被清理");
    }
}
