//! 通行密钥（WebAuthn）断言的校验，纯计算。
//!
//! 只支持 ES256（P-256 + SHA-256），这是所有主流平台通行密钥的默认算法。
//! 不校验注册时的证明（attestation）：登记的可信来自管理设备发出的一次性登记码。

use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature as EcdsaSignature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// P-256 公钥的 SubjectPublicKeyInfo 前缀（DER），后面紧跟 65 字节的 SEC1 非压缩点。
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce,
    0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

const FLAG_USER_PRESENT: u8 = 0x01;
const FLAG_USER_VERIFIED: u8 = 0x04;

fn base64url(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, &b)| acc | (b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

#[derive(Deserialize)]
struct ClientData {
    #[serde(rename = "type")]
    kind: String,
    challenge: String,
    origin: String,
    #[serde(rename = "crossOrigin", default)]
    cross_origin: bool,
}

/// P-256 公钥，SEC1 非压缩格式（65 字节，以 0x04 开头）。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct P256Key(pub Vec<u8>);

impl core::fmt::Debug for P256Key {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "P256Key({} 字节)", self.0.len())
    }
}

/// 浏览器交回的一次登录断言（`navigator.credentials.get` 的结果）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebAuthnAssertion {
    pub credential_id: Vec<u8>,
    pub client_data_json: Vec<u8>,
    pub authenticator_data: Vec<u8>,
    /// DER 编码的 ECDSA 签名。
    pub signature: Vec<u8>,
}

/// 校验失败的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebAuthnError {
    BadKey,
    BadClientData,
    WrongType,
    WrongChallenge,
    WrongOrigin,
    CrossOrigin,
    BadAuthenticatorData,
    WrongRpId,
    UserNotPresent,
    UserNotVerified,
    BadSignature,
}

/// 校验通过后得到的信息。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verified {
    pub sign_count: u32,
}

impl P256Key {
    /// 从 `AuthenticatorAttestationResponse.getPublicKey()` 给出的 SubjectPublicKeyInfo（DER）读取。
    pub fn from_spki(der: &[u8]) -> Result<Self, WebAuthnError> {
        let point = der.strip_prefix(&P256_SPKI_PREFIX[..]).ok_or(WebAuthnError::BadKey)?;
        VerifyingKey::from_sec1_bytes(point).map_err(|_| WebAuthnError::BadKey)?;
        Ok(Self(point.to_vec()))
    }

    /// SubjectPublicKeyInfo（DER）形式。
    pub fn to_spki(&self) -> Vec<u8> {
        let mut out = P256_SPKI_PREFIX.to_vec();
        out.extend_from_slice(&self.0);
        out
    }
}

/// 登录挑战：设备发出的随机数，绑定这次连接的浏览器（临时节点标识）和被访问的设备。
/// 通行密钥对它签名，所以签名不能拿到别的连接或别的设备上使用。
pub fn login_challenge(nonce: &[u8; 32], browser: crate::NodeId, target: crate::NodeId) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"nexus/v1/web-login\0");
    h.update(nonce);
    h.update(browser.0);
    h.update(target.0);
    h.finalize().into()
}

/// 页面来源是否属于依赖方 `rp_id`：https 且主机是 `rp_id` 或它的子域名；
/// `rp_id` 为 localhost 时（开发和测试）也接受 http 和任意端口。
pub fn origin_allowed(origin: &str, rp_id: &str) -> bool {
    let Some((scheme, rest)) = origin.split_once("://") else { return false };
    // 来源只有协议、主机和端口，没有路径。
    if rest.contains('/') || rp_id.is_empty() {
        return false;
    }
    let host = match rest.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => rest,
    };
    if rp_id == "localhost" {
        return host == "localhost" && (scheme == "http" || scheme == "https");
    }
    scheme == "https" && (host == rp_id || host.strip_suffix(rp_id).is_some_and(|sub| sub.ends_with('.') && sub.len() > 1))
}

/// 校验一次登录断言。`challenge` 是设备发出的挑战原文。
pub fn verify_assertion(key: &P256Key, rp_id: &str, challenge: &[u8], a: &WebAuthnAssertion) -> Result<Verified, WebAuthnError> {
    // 1. 客户端数据：浏览器写的，说明这是一次登录、签的是哪个挑战、在哪个页面上。
    let cd: ClientData = serde_json::from_slice(&a.client_data_json).map_err(|_| WebAuthnError::BadClientData)?;
    if cd.kind != "webauthn.get" {
        return Err(WebAuthnError::WrongType);
    }
    if cd.challenge.trim_end_matches('=') != base64url(challenge) {
        return Err(WebAuthnError::WrongChallenge);
    }
    if !origin_allowed(&cd.origin, rp_id) {
        return Err(WebAuthnError::WrongOrigin);
    }
    if cd.cross_origin {
        return Err(WebAuthnError::CrossOrigin);
    }
    // 2. 认证器数据：依赖方哈希、标志、计数器。
    let ad = &a.authenticator_data;
    if ad.len() < 37 {
        return Err(WebAuthnError::BadAuthenticatorData);
    }
    if ad[..32] != Sha256::digest(rp_id.as_bytes())[..] {
        return Err(WebAuthnError::WrongRpId);
    }
    let flags = ad[32];
    if flags & FLAG_USER_PRESENT == 0 {
        return Err(WebAuthnError::UserNotPresent);
    }
    if flags & FLAG_USER_VERIFIED == 0 {
        return Err(WebAuthnError::UserNotVerified);
    }
    let sign_count = u32::from_be_bytes([ad[33], ad[34], ad[35], ad[36]]);
    // 3. 签名覆盖认证器数据和客户端数据的哈希。
    let vk = VerifyingKey::from_sec1_bytes(&key.0).map_err(|_| WebAuthnError::BadKey)?;
    let sig = EcdsaSignature::from_der(&a.signature).map_err(|_| WebAuthnError::BadSignature)?;
    let mut msg = ad.clone();
    msg.extend_from_slice(&Sha256::digest(&a.client_data_json));
    vk.verify(&msg, &sig).map_err(|_| WebAuthnError::BadSignature)?;
    Ok(Verified { sign_count })
}

/// 软件实现的认证器：测试和仿真里扮演浏览器加通行密钥。产生的数据格式和真实浏览器一致。
/// 它只是一个签名器，放在正式代码里没有安全影响。
pub mod soft {
    use super::*;

    pub struct SoftAuthenticator {
        pub rp_id: String,
        pub origin: String,
        pub credential_id: Vec<u8>,
        pub user_verified: bool,
        pub user_present: bool,
        seed: [u8; 32],
        counter: u32,
    }

    impl SoftAuthenticator {
        pub fn new(seed: [u8; 32], rp_id: &str, origin: &str) -> Self {
            Self { rp_id: rp_id.into(), origin: origin.into(), credential_id: seed[..16].to_vec(), user_verified: true, user_present: true, seed, counter: 0 }
        }

        /// 注册时交给依赖方的公钥（SubjectPublicKeyInfo，DER）。
        fn key(&self) -> p256::ecdsa::SigningKey {
            p256::ecdsa::SigningKey::from_bytes(&self.seed.into()).expect("种子不是合法的 P-256 私钥")
        }

        pub fn spki(&self) -> Vec<u8> {
            let point = self.key().verifying_key().to_encoded_point(false);
            P256Key(point.as_bytes().to_vec()).to_spki()
        }

        /// 对挑战签名，得到一次登录断言。
        pub fn sign(&mut self, challenge: &[u8]) -> WebAuthnAssertion {
            use p256::ecdsa::signature::Signer;
            self.counter += 1;
            let client_data = format!(
                r#"{{"type":"webauthn.get","challenge":"{}","origin":"{}","crossOrigin":false}}"#,
                base64url(challenge),
                self.origin
            );
            let mut ad = Sha256::digest(self.rp_id.as_bytes()).to_vec();
            let present = if self.user_present { FLAG_USER_PRESENT } else { 0 };
            ad.push(present | if self.user_verified { FLAG_USER_VERIFIED } else { 0 });
            ad.extend_from_slice(&self.counter.to_be_bytes());
            let mut msg = ad.clone();
            msg.extend_from_slice(&Sha256::digest(client_data.as_bytes()));
            let sig: EcdsaSignature = self.key().sign(&msg);
            WebAuthnAssertion {
                credential_id: self.credential_id.clone(),
                client_data_json: client_data.into_bytes(),
                authenticator_data: ad,
                signature: sig.to_der().as_bytes().to_vec(),
            }
        }
    }
}
