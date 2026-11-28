//! 通行密钥断言的校验（FP-2.1-01）。

use nexus_core_proto::webauthn::soft::SoftAuthenticator;
use nexus_core_proto::webauthn::{origin_allowed, verify_assertion, P256Key, WebAuthnAssertion, WebAuthnError};
use proptest::prelude::*;

fn b64(s: &str) -> Vec<u8> {
    // 标准 base64（带填充），只用于读测试向量。
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes().filter(|&c| c != b'=') {
        acc = (acc << 6) | table.iter().position(|&t| t == c).unwrap() as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

struct Chromium {
    key: P256Key,
    assertion: WebAuthnAssertion,
    challenge: Vec<u8>,
}

/// tools/webauthn-probe 用真实 Chromium 和虚拟认证器生成的数据。
fn chromium() -> Chromium {
    let v: serde_json::Value = serde_json::from_str(include_str!("fixtures/chromium-webauthn.json")).unwrap();
    let reg = &v["registration"];
    let a = &v["assertion"];
    let hex = v["challenge_hex"].as_str().unwrap();
    Chromium {
        key: P256Key::from_spki(&b64(reg["spki"].as_str().unwrap())).expect("Chromium 的公钥可以读取"),
        assertion: WebAuthnAssertion {
            credential_id: b64(reg["id"].as_str().unwrap()),
            client_data_json: a["clientData"].as_str().unwrap().as_bytes().to_vec(),
            authenticator_data: b64(a["authData"].as_str().unwrap()),
            signature: b64(a["sig"].as_str().unwrap()),
        },
        challenge: (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect(),
    }
}

#[test]
fn real_chromium_assertion_verifies() {
    let c = chromium();
    let v = verify_assertion(&c.key, "localhost", &c.challenge, &c.assertion).expect("真实断言应当通过");
    assert!(v.sign_count > 0);
}

#[test]
fn real_chromium_assertion_rejects_any_changed_byte() {
    let c = chromium();
    for field in 0..3 {
        let len = [c.assertion.client_data_json.len(), c.assertion.authenticator_data.len(), c.assertion.signature.len()][field];
        for i in 0..len {
            let mut a = c.assertion.clone();
            let bytes = match field {
                0 => &mut a.client_data_json,
                1 => &mut a.authenticator_data,
                _ => &mut a.signature,
            };
            bytes[i] ^= 0x01;
            assert!(verify_assertion(&c.key, "localhost", &c.challenge, &a).is_err(), "改动第 {field} 段第 {i} 个字节后仍然通过");
        }
    }
}

#[test]
fn wrong_context_is_rejected_with_the_right_reason() {
    let c = chromium();
    assert_eq!(verify_assertion(&c.key, "localhost", &[8; 32], &c.assertion), Err(WebAuthnError::WrongChallenge));
    assert_eq!(verify_assertion(&c.key, "bewith.link", &c.challenge, &c.assertion), Err(WebAuthnError::WrongOrigin));
    let other = SoftAuthenticator::new([5; 32], "localhost", "http://localhost:1");
    let other_key = P256Key::from_spki(&other.spki()).unwrap();
    assert_eq!(verify_assertion(&other_key, "localhost", &c.challenge, &c.assertion), Err(WebAuthnError::BadSignature));
    assert_eq!(P256Key::from_spki(&[1, 2, 3]), Err(WebAuthnError::BadKey));
}

#[test]
fn origins() {
    for (origin, rp, ok) in [
        ("https://bewith.link", "bewith.link", true),
        ("https://app.bewith.link", "bewith.link", true),
        ("https://bewith.link:8443", "bewith.link", true),
        ("http://bewith.link", "bewith.link", false),
        ("https://evilbewith.link", "bewith.link", false),
        ("https://bewith.link.evil.com", "bewith.link", false),
        ("https://localhost", "bewith.link", false),
        ("http://localhost:41787", "localhost", true),
        ("https://localhost", "localhost", true),
        ("http://127.0.0.1:41787", "localhost", false),
        ("bewith.link", "bewith.link", false),
        ("", "bewith.link", false),
        ("https://bewith.link/", "bewith.link", false),
        ("https://bewith.link/login", "bewith.link", false),
        ("https://bewith.link:abc", "bewith.link", false),
        ("https://bewith.link:", "bewith.link", false),
        ("https://.bewith.link", "bewith.link", false),
        ("https://bewith.link", "", false),
        ("https://", "", false),
    ] {
        assert_eq!(origin_allowed(origin, rp), ok, "{origin} 对 {rp}");
    }
}

fn soft() -> SoftAuthenticator {
    SoftAuthenticator::new([3; 32], "bewith.link", "https://bewith.link")
}

#[test]
fn soft_authenticator_flags_are_checked() {
    let mut auth = soft();
    let key = P256Key::from_spki(&auth.spki()).unwrap();
    let a = auth.sign(b"challenge-1");
    assert_eq!(verify_assertion(&key, "bewith.link", b"challenge-1", &a).unwrap().sign_count, 1);
    assert_eq!(verify_assertion(&key, "bewith.link", b"challenge-1", &auth.sign(b"challenge-1")).unwrap().sign_count, 2);

    auth.user_verified = false;
    assert_eq!(verify_assertion(&key, "bewith.link", b"c", &auth.sign(b"c")), Err(WebAuthnError::UserNotVerified));
    auth.user_verified = true;
    auth.user_present = false;
    assert_eq!(verify_assertion(&key, "bewith.link", b"c", &auth.sign(b"c")), Err(WebAuthnError::UserNotPresent));
    // 调试输出不泄露公钥的内容，只给长度。
    assert_eq!(format!("{key:?}"), "P256Key(65 字节)");

    // 来源属于依赖方，但认证器数据里的依赖方哈希不同（例如为别的域名签的）。
    let mut other_rp = SoftAuthenticator::new([3; 32], "app.bewith.link", "https://app.bewith.link");
    assert_eq!(verify_assertion(&key, "bewith.link", b"c", &other_rp.sign(b"c")), Err(WebAuthnError::WrongRpId));
}

#[test]
fn client_data_is_checked() {
    let mut auth = soft();
    let key = P256Key::from_spki(&auth.spki()).unwrap();
    let good = auth.sign(b"c");
    let with_client_data = |text: &str| {
        // 换掉客户端数据后签名必然失效，所以先确认客户端数据的检查发生在签名之前，给出准确的原因。
        let mut a = good.clone();
        a.client_data_json = text.as_bytes().to_vec();
        verify_assertion(&key, "bewith.link", b"c", &a)
    };
    assert_eq!(with_client_data("not json"), Err(WebAuthnError::BadClientData));
    assert_eq!(
        with_client_data(r#"{"type":"webauthn.create","challenge":"Yw","origin":"https://bewith.link"}"#),
        Err(WebAuthnError::WrongType)
    );
    assert_eq!(
        with_client_data(r#"{"type":"webauthn.get","challenge":"Yw","origin":"https://bewith.link","crossOrigin":true}"#),
        Err(WebAuthnError::CrossOrigin)
    );
    let mut short = good.clone();
    short.authenticator_data.truncate(36);
    assert_eq!(verify_assertion(&key, "bewith.link", b"c", &short), Err(WebAuthnError::BadAuthenticatorData));
}

proptest! {
    #[test]
    fn soft_assertions_verify_and_tampering_fails(
        seed in any::<[u8; 32]>().prop_filter("合法私钥", |s| s.iter().any(|&b| b != 0)),
        challenge in prop::collection::vec(any::<u8>(), 1..64),
        field in 0usize..3,
        at in any::<prop::sample::Index>(),
        bit in 0u8..8,
    ) {
        let mut auth = SoftAuthenticator::new(seed, "bewith.link", "https://bewith.link");
        let key = P256Key::from_spki(&auth.spki()).unwrap();
        let a = auth.sign(&challenge);
        prop_assert!(verify_assertion(&key, "bewith.link", &challenge, &a).is_ok());
        let mut t = a.clone();
        let bytes = match field {
            0 => &mut t.client_data_json,
            1 => &mut t.authenticator_data,
            _ => &mut t.signature,
        };
        let i = at.index(bytes.len());
        bytes[i] ^= 1 << bit;
        prop_assert!(verify_assertion(&key, "bewith.link", &challenge, &t).is_err());
    }
}
