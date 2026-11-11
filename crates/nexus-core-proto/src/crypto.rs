//! 签名原语。对 ed25519-dalek 的薄封装，加上用途隔离：
//! 每个签名都绑定一个用途标签，为名册签的名不能拿去冒充证书或请求的签名。

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

/// 签名的用途。签名时把它和内容拼在一起，校验时必须给出同一个用途。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Roster,
    AgentCert,
    AgentAssertion,
}

impl Purpose {
    fn tag(self) -> &'static [u8] {
        match self {
            Purpose::Roster => b"nexus/v1/roster\0",
            Purpose::AgentCert => b"nexus/v1/agent-cert\0",
            Purpose::AgentAssertion => b"nexus/v1/agent-assertion\0",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PublicKey(pub [u8; 32]);

impl core::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "PublicKey({})", crate::NodeId(self.0).short())
    }
}

/// 64 字节的签名，拆成两半以便直接使用 serde。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    r: [u8; 32],
    s: [u8; 32],
}

impl Signature {
    fn from_bytes(b: [u8; 64]) -> Self {
        let mut r = [0u8; 32];
        let mut s = [0u8; 32];
        r.copy_from_slice(&b[..32]);
        s.copy_from_slice(&b[32..]);
        Self { r, s }
    }

    fn to_bytes(self) -> [u8; 64] {
        let mut b = [0u8; 64];
        b[..32].copy_from_slice(&self.r);
        b[32..].copy_from_slice(&self.s);
        b
    }
}

/// 私钥。由调用方提供的 32 字节随机种子构造，本层不读取随机数源。
pub struct SecretKey(SigningKey);

impl SecretKey {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self(SigningKey::from_bytes(&seed))
    }

    pub fn to_seed(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    pub fn public(&self) -> PublicKey {
        PublicKey(self.0.verifying_key().to_bytes())
    }

    /// 为某个用途签署一条消息。
    pub fn sign(&self, purpose: Purpose, msg: &[u8]) -> Signature {
        Signature::from_bytes(self.0.sign(&tagged(purpose, msg)).to_bytes())
    }
}

impl PublicKey {
    /// 校验签名。使用严格校验，拒绝弱公钥和非规范签名。
    pub fn verify(&self, purpose: Purpose, msg: &[u8], sig: &Signature) -> bool {
        let Ok(key) = VerifyingKey::from_bytes(&self.0) else { return false };
        let sig = ed25519_dalek::Signature::from_bytes(&sig.to_bytes());
        key.verify_strict(&tagged(purpose, msg), &sig).is_ok()
    }
}

fn tagged(purpose: Purpose, msg: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(purpose.tag().len() + msg.len());
    out.extend_from_slice(purpose.tag());
    out.extend_from_slice(msg);
    out
}

/// 一个值被签名时使用的规范字节。postcard 对同一个值总是产生相同的字节。
pub fn canonical<T: Serialize>(value: &T) -> Vec<u8> {
    postcard::to_allocvec(value).expect("内存中的值总能编码")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> SecretKey {
        SecretKey::from_seed([n; 32])
    }

    #[test]
    fn sign_then_verify() {
        let k = key(1);
        let sig = k.sign(Purpose::Roster, b"hello");
        assert!(k.public().verify(Purpose::Roster, b"hello", &sig));
    }

    #[test]
    fn verify_rejects_other_message_key_or_purpose() {
        // covers: R16
        let k = key(1);
        let sig = k.sign(Purpose::Roster, b"hello");
        assert!(!k.public().verify(Purpose::Roster, b"hellO", &sig));
        assert!(!key(2).public().verify(Purpose::Roster, b"hello", &sig));
        assert!(!k.public().verify(Purpose::AgentCert, b"hello", &sig));
        assert!(!k.public().verify(Purpose::AgentAssertion, b"hello", &sig));
    }

    #[test]
    fn debug_shows_a_short_id() {
        let k = PublicKey([0xab; 32]);
        assert_eq!(format!("{k:?}"), "PublicKey(abababab)");
    }

    #[test]
    fn seed_round_trip_and_garbage_public_key() {
        let k = key(3);
        assert_eq!(SecretKey::from_seed(k.to_seed()).public(), k.public());
        let sig = k.sign(Purpose::Roster, b"x");
        // 不是合法曲线点的公钥：校验失败而不是崩溃
        assert!(!PublicKey([0xff; 32]).verify(Purpose::Roster, b"x", &sig));
    }
}
