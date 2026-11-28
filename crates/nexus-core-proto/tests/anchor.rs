//! 名册里的锚点（M2.4，FP-2.4-02），以及加入锚点字段之前保存的名册仍然可用。

use nexus_core_proto::crypto::{canonical, PublicKey, Purpose, SecretKey, Signature};
use nexus_core_proto::roster::{
    valid_anchor_url, Device, DeviceRole, RevokedAgent, Roster, RosterError, SignedRoster, WebKey,
};
use nexus_core_proto::{NodeId, Timestamp};
use serde::Serialize;

fn root() -> SecretKey {
    SecretKey::from_seed([1; 32])
}

fn device() -> Device {
    Device {
        node_id: NodeId([2; 32]),
        name: "laptop".into(),
        role: DeviceRole::Admin,
        added_at: Timestamp(5),
    }
}

fn roster(anchor: Option<&str>) -> Roster {
    Roster {
        root: root().public(),
        version: 3,
        devices: vec![device()],
        revoked_agents: vec![],
        web_keys: vec![],
        anchor: anchor.map(String::from),
    }
}

/// 加入锚点字段之前的名册格式（M2.3 及以前）。
#[derive(Serialize)]
struct OldRoster {
    root: PublicKey,
    version: u64,
    devices: Vec<Device>,
    revoked_agents: Vec<RevokedAgent>,
    web_keys: Vec<WebKey>,
}

#[derive(Serialize)]
struct OldSignedRoster {
    roster: OldRoster,
    signature: Signature,
}

fn old_signed() -> OldSignedRoster {
    let r = OldRoster {
        root: root().public(),
        version: 3,
        devices: vec![device()],
        revoked_agents: vec![],
        web_keys: vec![],
    };
    let signature = root().sign(Purpose::Roster, &canonical(&r));
    OldSignedRoster {
        roster: r,
        signature,
    }
}

#[test]
fn rosters_saved_before_anchors_still_load_and_verify() {
    let bytes = postcard::to_allocvec(&old_signed()).unwrap();
    let loaded = SignedRoster::decode_stored(&bytes).expect("旧格式的名册应当能读出");
    assert_eq!(loaded.roster, roster(None));
    assert!(
        loaded.verify(&root().public()).is_ok(),
        "旧名册的签名仍然有效"
    );
    // 没有锚点的名册，签名和旧格式完全一样：旧设备签的名册和新设备签的可以互相验证。
    assert_eq!(roster(None).sign(&root()).signature, old_signed().signature);
    // 新格式原样读回。
    let anchored = roster(Some("https://bewith.link")).sign(&root());
    let again = SignedRoster::decode_stored(&postcard::to_allocvec(&anchored).unwrap()).unwrap();
    assert_eq!(again, anchored);
    assert!(SignedRoster::decode_stored(b"garbage").is_none());
}

#[test]
fn the_anchor_is_covered_by_the_signature() {
    let signed = roster(Some("https://bewith.link")).sign(&root());
    assert!(signed.verify(&root().public()).is_ok());
    let mut moved = signed.clone();
    moved.roster.anchor = Some("https://evil.example".into());
    assert_eq!(
        moved.verify(&root().public()),
        Err(RosterError::BadSignature)
    );
    let mut dropped = signed;
    dropped.roster.anchor = None;
    assert_eq!(
        dropped.verify(&root().public()),
        Err(RosterError::BadSignature)
    );
}

#[test]
fn anchors_must_be_https_except_on_this_machine() {
    for ok in [
        "https://bewith.link",
        "https://relay.example.com:8443",
        "http://localhost:3340",
        "http://localhost",
        "https://bewith.link/",
    ] {
        assert!(valid_anchor_url(ok), "{ok}");
    }
    // 通行密钥不接受 IP 地址作为域名，所以本机开发也只能用 localhost。
    for bad in [
        "http://bewith.link",
        "ftp://x",
        "https://",
        "https://a/b",
        "https://a?b",
        "bewith.link",
        "http://localhost.evil.com",
        "http://127.0.0.1:8080",
        "https://a:",
        "https://a:x",
    ] {
        assert!(!valid_anchor_url(bad), "{bad}");
    }
    let r = roster(Some("http://example.com"));
    assert!(matches!(r.validate(), Err(RosterError::InvalidAnchor(_))));
}
