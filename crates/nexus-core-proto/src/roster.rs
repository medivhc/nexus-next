//! 名册：由根密钥签名的「谁是我的设备」清单，是判断对端是否可信的唯一依据。

use crate::authz::Grant;
use crate::crypto::{canonical, PublicKey, Purpose, SecretKey, Signature};
use crate::webauthn::P256Key;
use crate::path::valid_segment;
use crate::{NodeId, Timestamp};
use core::fmt;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceRole {
    /// 持有根密钥，可以批准设备、签发 agent 证书。
    Admin,
    Member,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub node_id: NodeId,
    /// 同时用作资源路径的第一段，所以必须是合法的路径段。
    pub name: String,
    pub role: DeviceRole,
    pub added_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokedAgent {
    pub agent: PublicKey,
    pub revoked_at: Timestamp,
}

/// 登记进网络的一个通行密钥：网页登录的身份。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebKey {
    /// 名字：审计里的主体。和设备名在同一个命名空间里，不能重复。
    pub label: String,
    pub credential_id: Vec<u8>,
    pub public_key: P256Key,
    /// 创建通行密钥时页面所在的依赖方（域名），登录时必须一致。
    pub rp_id: String,
    pub grants: Vec<Grant>,
    pub added_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roster {
    pub root: PublicKey,
    /// 单调递增。新名册的版本号必须严格大于旧的。
    pub version: u64,
    pub devices: Vec<Device>,
    pub revoked_agents: Vec<RevokedAgent>,
    pub web_keys: Vec<WebKey>,
    /// 锚点（M2.4）：网络自己的中继和页面所在的地址，例如 `https://bewith.link`。
    /// 各设备把它加为中继，登记链接默认指向它。
    pub anchor: Option<String>,
}

/// 锚点地址是否可用：https 的「方案://主机[:端口]」；开发时允许 http://localhost。
pub fn valid_anchor_url(url: &str) -> bool {
    let (rest, local_only) = if let Some(r) = url.strip_prefix("https://") {
        (r, false)
    } else if let Some(r) = url.strip_prefix("http://") {
        (r, true)
    } else {
        return false;
    };
    let host_port = rest.strip_suffix('/').unwrap_or(rest);
    if host_port.is_empty() || host_port.contains(['/', '?', '#', '@', ' ']) {
        return false;
    }
    let host = match host_port.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        Some(_) => return false,
        None => host_port,
    };
    if host.is_empty() {
        return false;
    }
    // 页面也在锚点上，通行密钥绑定它的域名；WebAuthn 不接受 IP 地址，本机开发只能用 localhost。
    !local_only || host == "localhost"
}

/// 加入锚点之前的名册格式。没有锚点的名册按它签名，所以旧设备签的名册新设备照样能验证，
/// 加入锚点之前保存在磁盘上的名册也照样能读出。
#[derive(Serialize, Deserialize)]
struct RosterV1 {
    root: PublicKey,
    version: u64,
    devices: Vec<Device>,
    revoked_agents: Vec<RevokedAgent>,
    web_keys: Vec<WebKey>,
}

#[derive(Serialize)]
struct RosterV1Ref<'a> {
    root: &'a PublicKey,
    version: u64,
    devices: &'a [Device],
    revoked_agents: &'a [RevokedAgent],
    web_keys: &'a [WebKey],
}

#[derive(Deserialize)]
struct SignedRosterV1 {
    roster: RosterV1,
    signature: Signature,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedRoster {
    pub roster: Roster,
    pub signature: Signature,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RosterError {
    /// 名册声明的根与本机信任的根不同。
    WrongRoot,
    BadSignature,
    DuplicateDevice(NodeId),
    DuplicateName(String),
    InvalidName(String),
    InvalidAnchor(String),
    /// 版本号没有前进。
    NotNewer { current: u64, candidate: u64 },
}

impl fmt::Display for RosterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RosterError::WrongRoot => f.write_str("名册不属于本机信任的根"),
            RosterError::BadSignature => f.write_str("名册签名无效"),
            RosterError::DuplicateDevice(id) => write!(f, "名册中设备 {} 重复", id.short()),
            RosterError::DuplicateName(n) => write!(f, "名册中设备名 {n} 重复"),
            RosterError::InvalidName(n) => write!(f, "名册中设备名 {n:?} 不合法"),
            RosterError::InvalidAnchor(a) => {
                write!(f, "锚点地址 {a:?} 不合法：应当是 https://主机[:端口]（本机开发可以用 http://localhost）")
            }
            RosterError::NotNewer { current, candidate } => {
                write!(f, "名册版本 {candidate} 不比当前的 {current} 新")
            }
        }
    }
}

impl Roster {
    pub fn sign(self, root: &SecretKey) -> SignedRoster {
        let signature = root.sign(Purpose::Roster, &self.signed_bytes());
        SignedRoster { roster: self, signature }
    }

    /// 签名覆盖的内容。没有锚点时与加入锚点之前的格式逐字节相同。
    fn signed_bytes(&self) -> Vec<u8> {
        match &self.anchor {
            None => canonical(&RosterV1Ref {
                root: &self.root,
                version: self.version,
                devices: &self.devices,
                revoked_agents: &self.revoked_agents,
                web_keys: &self.web_keys,
            }),
            Some(_) => canonical(self),
        }
    }

    pub fn device(&self, id: NodeId) -> Option<&Device> {
        self.devices.iter().find(|d| d.node_id == id)
    }

    pub fn device_by_name(&self, name: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.name == name)
    }

    pub fn web_key(&self, label: &str) -> Option<&WebKey> {
        self.web_keys.iter().find(|k| k.label == label)
    }

    pub fn is_agent_revoked(&self, agent: &PublicKey) -> bool {
        self.revoked_agents.iter().any(|r| r.agent == *agent)
    }

    /// 内容是否自洽：设备标识和名字都不重复，名字都合法。
    pub fn validate(&self) -> Result<(), RosterError> {
        for (i, d) in self.devices.iter().enumerate() {
            if !valid_segment(&d.name) {
                return Err(RosterError::InvalidName(d.name.clone()));
            }
            let earlier = &self.devices[..i];
            if earlier.iter().any(|e| e.node_id == d.node_id) {
                return Err(RosterError::DuplicateDevice(d.node_id));
            }
            if earlier.iter().any(|e| e.name == d.name) {
                return Err(RosterError::DuplicateName(d.name.clone()));
            }
        }
        for (i, k) in self.web_keys.iter().enumerate() {
            if !valid_segment(&k.label) {
                return Err(RosterError::InvalidName(k.label.clone()));
            }
            if self.web_keys[..i].iter().any(|e| e.label == k.label) || self.device_by_name(&k.label).is_some() {
                return Err(RosterError::DuplicateName(k.label.clone()));
            }
        }
        if let Some(a) = &self.anchor {
            if !valid_anchor_url(a) {
                return Err(RosterError::InvalidAnchor(a.clone()));
            }
        }
        Ok(())
    }
}

impl SignedRoster {
    /// 读出保存的名册：当前格式，或者加入锚点之前的格式（读出后锚点为空，签名仍然有效）。
    pub fn decode_stored(bytes: &[u8]) -> Option<Self> {
        if let Ok(s) = postcard::from_bytes::<SignedRoster>(bytes) {
            return Some(s);
        }
        let old = postcard::from_bytes::<SignedRosterV1>(bytes).ok()?;
        let r = old.roster;
        let roster = Roster {
            root: r.root,
            version: r.version,
            devices: r.devices,
            revoked_agents: r.revoked_agents,
            web_keys: r.web_keys,
            anchor: None,
        };
        Some(SignedRoster { roster, signature: old.signature })
    }

    /// 校验根、签名和内容，成功后返回名册本身。
    pub fn verify(&self, trusted_root: &PublicKey) -> Result<&Roster, RosterError> {
        if self.roster.root != *trusted_root {
            return Err(RosterError::WrongRoot);
        }
        if !trusted_root.verify(Purpose::Roster, &self.roster.signed_bytes(), &self.signature) {
            return Err(RosterError::BadSignature);
        }
        self.roster.validate()?;
        Ok(&self.roster)
    }
}

/// 根密钥轮换声明（M10.1）：把网络的信任根从 `old_root` 切换到 `new_root`，在名册版本 `version` 生效。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootRotation {
    pub old_root: PublicKey,
    pub new_root: PublicKey,
    pub version: u64,
}

/// 由旧根私钥签名的根密钥轮换证书（M10.1）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedRootRotation {
    pub rotation: RootRotation,
    pub old_sig: Signature,
}

impl RootRotation {
    pub fn sign(self, old_root: &SecretKey) -> SignedRootRotation {
        let old_sig = old_root.sign(Purpose::RootRotation, &canonical(&self));
        SignedRootRotation { rotation: self, old_sig }
    }
}

/// 校验一次根密钥轮换（M10.1）：
/// 1. `rotation.old_root` 必须等于本机当前信任的 `trusted_root`，且 `old_sig` 必须由旧根私钥有效签署；
/// 2. `new_roster` 的根必须等于 `rotation.new_root`、版本号等于 `rotation.version` 且严格大于当前名册版本；
/// 3. `new_roster` 本身必须由新根私钥有效签署且内容自洽。
pub fn verify_root_rotation<'a>(
    trusted_root: &PublicKey,
    current: Option<&SignedRoster>,
    rotation: &SignedRootRotation,
    new_roster: &'a SignedRoster,
) -> Result<&'a Roster, RosterError> {
    let r = &rotation.rotation;
    if r.old_root != *trusted_root || r.new_root == *trusted_root {
        return Err(RosterError::WrongRoot);
    }
    if !trusted_root.verify(Purpose::RootRotation, &canonical(r), &rotation.old_sig) {
        return Err(RosterError::BadSignature);
    }
    if new_roster.roster.root != r.new_root {
        return Err(RosterError::WrongRoot);
    }
    if new_roster.roster.version != r.version {
        return Err(RosterError::BadSignature);
    }
    if let Some(cur) = current {
        if r.version <= cur.roster.version {
            return Err(RosterError::NotNewer { current: cur.roster.version, candidate: r.version });
        }
    }
    new_roster.verify(&r.new_root)
}

/// 是否应当用 `candidate` 替换 `current`。
pub fn accept_update(
    current: Option<&SignedRoster>,
    candidate: &SignedRoster,
    trusted_root: &PublicKey,
) -> Result<(), RosterError> {
    let new = candidate.verify(trusted_root)?;
    match current {
        Some(cur) if new.version <= cur.roster.version => {
            Err(RosterError::NotNewer { current: cur.roster.version, candidate: new.version })
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> SecretKey {
        SecretKey::from_seed([1; 32])
    }

    fn dev(n: u8, name: &str) -> Device {
        Device { node_id: NodeId([n; 32]), name: name.into(), role: DeviceRole::Member, added_at: Timestamp(0) }
    }

    fn roster(version: u64, devices: Vec<Device>) -> Roster {
        Roster { root: root().public(), version, devices, revoked_agents: vec![], web_keys: vec![], anchor: None }
    }

    #[test]
    fn signed_roster_verifies_under_its_root() {
        // covers: R02
        let s = roster(1, vec![dev(1, "laptop")]).sign(&root());
        assert_eq!(s.verify(&root().public()).map(|r| r.version), Ok(1));
    }

    #[test]
    fn roster_from_another_root_is_rejected() {
        // covers: R02
        let other = SecretKey::from_seed([2; 32]);
        // 声明了别的根
        let mut r = roster(1, vec![]);
        r.root = other.public();
        assert_eq!(r.sign(&other).verify(&root().public()), Err(RosterError::WrongRoot));
        // 声明了本机的根，但由别的密钥签名
        let forged = roster(1, vec![]).sign(&other);
        assert_eq!(forged.verify(&root().public()), Err(RosterError::BadSignature));
    }

    #[test]
    fn tampering_after_signing_is_detected() {
        // covers: R02, R18
        let mut s = roster(1, vec![dev(1, "laptop")]).sign(&root());
        s.roster.devices.push(dev(9, "intruder"));
        assert_eq!(s.verify(&root().public()), Err(RosterError::BadSignature));

        let mut s = roster(1, vec![dev(1, "laptop")]).sign(&root());
        s.roster.version = 99;
        assert_eq!(s.verify(&root().public()), Err(RosterError::BadSignature));
    }

    #[test]
    fn inconsistent_content_is_rejected_even_when_signed() {
        let r = root().public();
        let dup_id = roster(1, vec![dev(1, "a"), dev(1, "b")]).sign(&root());
        assert_eq!(dup_id.verify(&r), Err(RosterError::DuplicateDevice(NodeId([1; 32]))));
        let dup_name = roster(1, vec![dev(1, "a"), dev(2, "a")]).sign(&root());
        assert_eq!(dup_name.verify(&r), Err(RosterError::DuplicateName("a".into())));
        let bad_name = roster(1, vec![dev(1, "my laptop")]).sign(&root());
        assert_eq!(bad_name.verify(&r), Err(RosterError::InvalidName("my laptop".into())));
    }

    #[test]
    fn only_strictly_newer_rosters_are_accepted() {
        // covers: R02, R18
        let r = root().public();
        let v1 = roster(1, vec![dev(1, "laptop")]).sign(&root());
        let v2 = roster(2, vec![dev(1, "laptop"), dev(2, "home")]).sign(&root());
        assert_eq!(accept_update(None, &v1, &r), Ok(()));
        assert_eq!(accept_update(Some(&v1), &v2, &r), Ok(()));
        assert_eq!(accept_update(Some(&v2), &v1, &r), Err(RosterError::NotNewer { current: 2, candidate: 1 }));
        assert_eq!(accept_update(Some(&v2), &v2, &r), Err(RosterError::NotNewer { current: 2, candidate: 2 }));
        let forged = roster(3, vec![]).sign(&SecretKey::from_seed([2; 32]));
        assert_eq!(accept_update(Some(&v2), &forged, &r), Err(RosterError::BadSignature));
    }

    #[test]
    fn errors_explain_themselves() {
        let msg = RosterError::NotNewer { current: 4, candidate: 3 }.to_string();
        assert!(msg.contains('3') && msg.contains('4'), "{msg}");
        assert!(RosterError::DuplicateName("home".into()).to_string().contains("home"));
        assert!(!RosterError::BadSignature.to_string().is_empty());
    }

    #[test]
    fn lookups() {
        let mut r = roster(1, vec![dev(1, "laptop"), dev(2, "home")]);
        let agent = SecretKey::from_seed([7; 32]).public();
        r.revoked_agents.push(RevokedAgent { agent, revoked_at: Timestamp(5) });
        assert_eq!(r.device(NodeId([2; 32])).map(|d| d.name.as_str()), Some("home"));
        assert_eq!(r.device(NodeId([3; 32])), None);
        assert_eq!(r.device_by_name("laptop").map(|d| d.node_id), Some(NodeId([1; 32])));
        assert!(r.device_by_name("office").is_none());
        assert!(r.is_agent_revoked(&agent));
        assert!(!r.is_agent_revoked(&SecretKey::from_seed([8; 32]).public()));
    }

    #[test]
    fn root_rotation_requires_both_old_and_new_signatures() {
        // covers: R75
        let old_root = SecretKey::from_seed([1; 32]);
        let new_root = SecretKey::from_seed([2; 32]);
        let attacker = SecretKey::from_seed([9; 32]);
        let v1 = roster(1, vec![dev(1, "laptop")]).sign(&old_root);

        let mut r2 = roster(2, vec![dev(1, "laptop")]);
        r2.root = new_root.public();
        let v2 = r2.clone().sign(&new_root);

        let rot = RootRotation { old_root: old_root.public(), new_root: new_root.public(), version: 2 }.sign(&old_root);
        assert_eq!(verify_root_rotation(&old_root.public(), Some(&v1), &rot, &v2).map(|r| r.version), Ok(2));

        // 攻击者不知道旧根私钥，伪造轮换证书被拒
        let forged_rot = RootRotation { old_root: old_root.public(), new_root: new_root.public(), version: 2 }.sign(&attacker);
        assert_eq!(verify_root_rotation(&old_root.public(), Some(&v1), &forged_rot, &v2), Err(RosterError::BadSignature));

        // 攻击者篡改新名册签名被拒
        let forged_roster = r2.sign(&attacker);
        assert_eq!(verify_root_rotation(&old_root.public(), Some(&v1), &rot, &forged_roster), Err(RosterError::BadSignature));
    }
}
