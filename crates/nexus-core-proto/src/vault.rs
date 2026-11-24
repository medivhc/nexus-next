//! Vault（M9.2）：用户秘密的认证加密封装。
//!
//! 纯计算，不碰随机数源或系统时钟：加密用的 16 字节随机盐由调用方传入。

use crate::PublicKey;

const SALT_LEN: usize = 16;
const TAG_LEN: usize = 16;

fn entry_key(root: &PublicKey, name: &str) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("nexus/vault/entry-key/v1");
    h.update(&root.0);
    h.update(&(name.len() as u32).to_le_bytes());
    h.update(name.as_bytes());
    *h.finalize().as_bytes()
}

fn keystream(key: &[u8; 32], salt: &[u8], len: usize) -> Vec<u8> {
    let mut h = blake3::Hasher::new_keyed(key);
    h.update(b"vault-stream");
    h.update(salt);
    let mut out = vec![0u8; len];
    h.finalize_xof().fill(&mut out);
    out
}

fn tag(key: &[u8; 32], salt: &[u8], ct: &[u8]) -> [u8; TAG_LEN] {
    let mut h = blake3::Hasher::new_keyed(key);
    h.update(b"vault-tag");
    h.update(salt);
    h.update(ct);
    let mut t = [0u8; TAG_LEN];
    t.copy_from_slice(&h.finalize().as_bytes()[..TAG_LEN]);
    t
}

/// 加密一条秘密：`盐(16B) ‖ 密文 ‖ 认证标签(16B)`。
pub fn seal(root: &PublicKey, name: &str, salt: [u8; SALT_LEN], plain: &[u8]) -> Vec<u8> {
    let key = entry_key(root, name);
    let mut out = Vec::with_capacity(SALT_LEN + plain.len() + TAG_LEN);
    out.extend_from_slice(&salt);
    out.extend(plain.iter().zip(keystream(&key, &salt, plain.len())).map(|(a, b)| a ^ b));
    let t = tag(&key, &salt, &out[SALT_LEN..]);
    out.extend_from_slice(&t);
    out
}

/// 解密一条秘密；密钥、条目名不匹配或密文被篡改时返回 `None`。
pub fn open(root: &PublicKey, name: &str, sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < SALT_LEN + TAG_LEN {
        return None;
    }
    let key = entry_key(root, name);
    let (salt, rest) = sealed.split_at(SALT_LEN);
    let (ct, t) = rest.split_at(rest.len() - TAG_LEN);
    let expect = tag(&key, salt, ct);
    if expect.iter().zip(t).fold(0u8, |acc, (a, b)| acc | (a ^ b)) != 0 {
        return None;
    }
    Some(ct.iter().zip(keystream(&key, salt, ct.len())).map(|(a, b)| a ^ b).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_seal_and_open_roundtrip_and_detect_tampering() {
        let root = PublicKey([7; 32]);
        let salt = [3; 16];
        let sealed = seal(&root, "openai_key", salt, b"sk-live-12345");
        assert_ne!(&sealed[16..sealed.len() - 16], b"sk-live-12345");
        assert_eq!(open(&root, "openai_key", &sealed).unwrap(), b"sk-live-12345");
        // 换条目名或篡改密文都解不开。
        assert!(open(&root, "other_key", &sealed).is_none());
        let mut bad = sealed.clone();
        bad[17] ^= 1;
        assert!(open(&root, "openai_key", &bad).is_none());
    }
}
