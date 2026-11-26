//! 新旧版本互通（M7.1，FP-7.1-01、03）：已有消息的编码永远不变。
//!
//! `fixtures/<主版本>.<次版本>.txt` 每行是「样例名 十六进制编码」。测试检查：
//! - 每个样例现在的编码和它第一次出现的那个次版本里存着的逐字节相同；
//! - 存着的每一条都能解码成同一个样例。
//!
//! 加了新的请求、回应或错误码：把 `WIRE_MINOR` 加一，在下面的样例表里加样例（穷举匹配会提醒），
//! 然后 `NEXUS_BLESS=1 cargo test -p nexus-wire --test compat` 生成新次版本的 fixtures 文件并提交。
//! 已有样例的编码变了不能 bless：那是不兼容的改动，要升主版本。

use nexus_core_proto::authz::{
    Action, AgentCert, Assertion, Grant, Principal, PrincipalRef, SignedWebSession, WebSession,
};
use nexus_core_proto::crypto::SecretKey;
use nexus_core_proto::path::{Capability, CapabilityPattern, ResourcePath, ResourcePattern};
use nexus_core_proto::roster::{Device, DeviceRole, Roster, SignedRoster};
use nexus_core_proto::webauthn::WebAuthnAssertion;
use nexus_core_proto::{NodeId, NodeInfo, Timestamp};
use nexus_wire::*;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn key(n: u8) -> SecretKey {
    SecretKey::from_seed([n; 32])
}

fn node(n: u8) -> NodeId {
    NodeId(key(n).public().0)
}

fn roster() -> SignedRoster {
    Roster {
        root: key(1).public(),
        version: 7,
        devices: vec![
            Device { node_id: node(2), name: "laptop".into(), role: DeviceRole::Admin, added_at: Timestamp(1_000) },
            Device { node_id: node(3), name: "home".into(), role: DeviceRole::Member, added_at: Timestamp(2_000) },
        ],
        revoked_agents: vec![],
        web_keys: vec![],
        anchor: Some("https://bewith.link".into()),
    }
    .sign(&key(1))
}

fn web_session() -> SignedWebSession {
    WebSession {
        credential_id: vec![1, 2, 3],
        browser: node(9),
        label: "phone".into(),
        issuer: node(3),
        issued_at: Timestamp(5_000),
        not_after: Timestamp(6_000),
    }
    .sign(&key(3))
}

fn agent_principal() -> Principal {
    let cert = AgentCert {
        agent: key(8).public(),
        label: "ci".into(),
        not_before: Timestamp(1_000),
        not_after: Timestamp(9_000),
        grants: vec![Grant {
            capability: CapabilityPattern::parse("term.exec").unwrap(),
            resource: ResourcePattern::parse("home").unwrap(),
        }],
    }
    .issue(&key(1));
    let assertion = Assertion {
        target: node(3),
        action: Action { capability: Capability::parse("term.exec").unwrap(), resource: ResourcePath::parse("home").unwrap() },
        issued_at: Timestamp(4_000),
        nonce: [4; 16],
    };
    let signature = assertion.sign(&key(8));
    Principal::Agent { cert, assertion, signature }
}

fn size() -> Size {
    Size { cols: 80, rows: 24 }
}

/// 每种请求一个样例，名字和种类一一对应（穷举匹配：加了新请求不加样例就编译不过）。
fn request_samples() -> Vec<(&'static str, Request)> {
    let all = vec![
        Request::NodeInfo,
        Request::RosterSync,
        Request::RosterPush { roster: roster(), addrs: vec![(node(2), "hint".into())] },
        Request::JoinOffer { nonce: [5; 16], roster: roster(), addrs: vec![] },
        Request::Exec(ExecSpec {
            program: "uname".into(),
            args: vec!["-a".into()],
            cwd: Some("/tmp".into()),
            env: vec![("K".into(), "V".into())],
            timeout_ms: Some(60_000),
            max_output: 1 << 20,
        }),
        Request::WebEnroll {
            token: [6; 16],
            credential: NewCredential { credential_id: vec![1], spki: vec![2, 3], rp_id: "bewith.link".into() },
        },
        Request::WebLogin {
            issued_at: Timestamp(3_000),
            assertion: WebAuthnAssertion {
                credential_id: vec![1],
                client_data_json: b"{}".to_vec(),
                authenticator_data: vec![0; 37],
                signature: vec![7; 8],
            },
        },
        Request::SessionAttach { name: "main".into(), create: true, size: size() },
        Request::SessionList,
        Request::SessionKill { name: "build".into() },
        Request::SvcConnect { name: "web".into() },
        Request::SvcList,
        Request::ExitConnect { target: "example.com:443".into() },
        Request::DriveInfo,
        Request::DriveRegister { ws: "work".into() },
        Request::DriveGet { ws: "work".into(), hashes: vec![[1; 32]] },
        Request::DriveHave { ws: "work".into(), hashes: vec![[2; 32], [3; 32]] },
        Request::DrivePush { ws: "work".into(), head: [4; 32], count: 3 },
        Request::DriveProfilePut { ws: "work".into(), name: "build".into(), files: vec!["/a".into()] },
        Request::DriveProfileGet { ws: "work".into(), name: "build".into() },
        Request::HardwareList,
        Request::HardwareCapture { kind: HardwareKind::Location },
        Request::ShareOpen { token: [8; 16] },
        Request::ShareDriveList { token: [8; 16], path: "/sub".into() },
        Request::ShareDriveRead { token: [8; 16], path: "/a.txt".into(), max: 1024 },
        Request::ShareServiceConnect { token: [8; 16] },
        Request::ShareSessionWatch { token: [8; 16], size: Size { cols: 80, rows: 24 } },
        Request::VaultPush {
            name: "api_key".into(),
            entry: Some(VaultCipherEntry {
                name: "api_key".into(),
                version: 1,
                updated_at: Timestamp(100),
                devices: vec!["home".into()],
                sealed: vec![1, 2, 3],
            }),
        },
        Request::VaultPull,
        Request::RootRotate {
            rotation: nexus_core_proto::roster::RootRotation {
                old_root: SecretKey::from_seed([1; 32]).public(),
                new_root: SecretKey::from_seed([2; 32]).public(),
                version: 8,
            }
            .sign(&SecretKey::from_seed([1; 32])),
            roster: roster(),
            addrs: vec![(node(3), "hint".into())],
        },
        Request::SvcDatagram { name: "dns".into() },
    ];
    all.into_iter()
        .map(|r| {
            let name = match &r {
                Request::NodeInfo => "request.node_info",
                Request::RosterSync => "request.roster_sync",
                Request::RosterPush { .. } => "request.roster_push",
                Request::JoinOffer { .. } => "request.join_offer",
                Request::Exec(_) => "request.exec",
                Request::WebEnroll { .. } => "request.web_enroll",
                Request::WebLogin { .. } => "request.web_login",
                Request::SessionAttach { .. } => "request.session_attach",
                Request::SessionList => "request.session_list",
                Request::SessionKill { .. } => "request.session_kill",
                Request::SvcConnect { .. } => "request.svc_connect",
                Request::SvcList => "request.svc_list",
                Request::ExitConnect { .. } => "request.exit_connect",
                Request::DriveInfo => "request.drive_info",
                Request::DriveRegister { .. } => "request.drive_register",
                Request::DriveGet { .. } => "request.drive_get",
                Request::DriveHave { .. } => "request.drive_have",
                Request::DrivePush { .. } => "request.drive_push",
                Request::DriveProfilePut { .. } => "request.drive_profile_put",
                Request::DriveProfileGet { .. } => "request.drive_profile_get",
                Request::HardwareList => "request.hardware_list",
                Request::HardwareCapture { .. } => "request.hardware_capture",
                Request::ShareOpen { .. } => "request.share_open",
                Request::ShareDriveList { .. } => "request.share_drive_list",
                Request::ShareDriveRead { .. } => "request.share_drive_read",
                Request::ShareServiceConnect { .. } => "request.share_service_connect",
                Request::ShareSessionWatch { .. } => "request.share_session_watch",
                Request::VaultPush { .. } => "request.vault_push",
                Request::VaultPull => "request.vault_pull",
                Request::RootRotate { .. } => "request.root_rotate",
                Request::SvcDatagram { .. } => "request.svc_datagram",
            };
            (name, r)
        })
        .collect()
}

fn response_samples() -> Vec<(&'static str, Response)> {
    let all = vec![
        Response::NodeInfo(NodeInfo {
            node_id: node(3),
            name: "home".into(),
            version: "0.1.0".into(),
            started_at: Timestamp(100),
            now: Timestamp(200),
            roster_version: Some(7),
        }),
        Response::Roster { roster: Some(roster()), addrs: vec![(node(3), "hint".into())] },
        Response::Ok,
        Response::Sessions(vec![SessionInfo { name: "main".into(), clients: 2, created_at: Timestamp(10), size: size() }]),
        Response::Services(vec![ServiceInfo {
            name: "llm".into(),
            proto: "http".into(),
            env: vec![("OPENAI_BASE_URL".into(), "http://{addr}/v1".into())],
        }]),
        Response::WebEnrolled { label: "phone".into() },
        Response::WebLoggedIn {
            session: web_session(),
            devices: vec![WebDevice { name: "home".into(), node_id: node(3), hint: "hint".into() }],
        },
        Response::DriveInfo { serving: true, workspaces: vec![("work".into(), Some([5; 32])), ("new".into(), None)] },
        Response::DriveMissing(vec![[6; 32]]),
        Response::DrivePushed { accepted: false, head: Some([7; 32]) },
        Response::DriveProfile(Some(vec!["/a".into()])),
        Response::Error(WireError::new(ErrorCode::Denied, "不行")),
        Response::HardwareList(vec![HardwareKind::Camera, HardwareKind::Location]),
        Response::HardwareCapture(HardwareCapture::Location { lat_e7: 312_304_000, lon_e7: 1_214_737_000, accuracy_cm: 1250, timestamp_ms: 100 }),
        Response::ShareOpened(ShareInfo {
            id: "01020304".into(),
            device: "home".into(),
            target: ShareTarget::Service { name: "web".into() },
            expires_at: Timestamp(500),
            remaining_uses: Some(3),
        }),
        Response::ShareDriveEntries(vec![ShareDriveEntry { name: "a.txt".into(), kind: "file".into(), size: 5, target: None }]),
        Response::ShareDriveFile { size: 5, bytes: b"hello".to_vec(), truncated: false },
        Response::VaultEntries(vec![VaultCipherEntry {
            name: "api_key".into(),
            version: 1,
            updated_at: Timestamp(100),
            devices: vec!["home".into()],
            sealed: vec![1, 2, 3],
        }]),
    ];
    all.into_iter()
        .map(|r| {
            let name = match &r {
                Response::NodeInfo(_) => "response.node_info",
                Response::Roster { .. } => "response.roster",
                Response::Ok => "response.ok",
                Response::Sessions(_) => "response.sessions",
                Response::Services(_) => "response.services",
                Response::WebEnrolled { .. } => "response.web_enrolled",
                Response::WebLoggedIn { .. } => "response.web_logged_in",
                Response::DriveInfo { .. } => "response.drive_info",
                Response::DriveMissing(_) => "response.drive_missing",
                Response::DrivePushed { .. } => "response.drive_pushed",
                Response::DriveProfile(_) => "response.drive_profile",
                Response::Error(_) => "response.error",
                Response::HardwareList(_) => "response.hardware_list",
                Response::HardwareCapture(_) => "response.hardware_capture",
                Response::ShareOpened(_) => "response.share_opened",
                Response::ShareDriveEntries(_) => "response.share_drive_entries",
                Response::ShareDriveFile { .. } => "response.share_drive_file",
                Response::VaultEntries(_) => "response.vault_entries",
            };
            (name, r)
        })
        .collect()
}

fn error_code_samples() -> Vec<(&'static str, ErrorCode)> {
    let all = [
        ErrorCode::UnsupportedVersion,
        ErrorCode::UnknownNode,
        ErrorCode::Unreachable,
        ErrorCode::Internal,
        ErrorCode::Denied,
        ErrorCode::StaleRoster,
        ErrorCode::InvalidState,
        ErrorCode::InvalidInput,
        ErrorCode::SpawnFailed,
        ErrorCode::NotFound,
    ];
    all.into_iter()
        .filter_map(|c| {
            let name = match c {
                ErrorCode::UnsupportedVersion => "error.unsupported_version",
                ErrorCode::UnknownNode => "error.unknown_node",
                ErrorCode::Unreachable => "error.unreachable",
                ErrorCode::Internal => "error.internal",
                ErrorCode::Denied => "error.denied",
                ErrorCode::StaleRoster => "error.stale_roster",
                ErrorCode::InvalidState => "error.invalid_state",
                ErrorCode::InvalidInput => "error.invalid_input",
                ErrorCode::SpawnFailed => "error.spawn_failed",
                ErrorCode::NotFound => "error.not_found",
                // 只在解码时出现，从不发出去。
                ErrorCode::Unknown => return None,
            };
            Some((name, c))
        })
        .collect()
}

/// 所有样例的编码（名字 → 字节）。名字带种类前缀，解码时据此选类型。
fn encoded_samples() -> BTreeMap<&'static str, Vec<u8>> {
    let mut out = BTreeMap::new();
    for (n, r) in request_samples() {
        out.insert(n, postcard::to_allocvec(&r).unwrap());
    }
    for (n, r) in response_samples() {
        out.insert(n, postcard::to_allocvec(&r).unwrap());
    }
    for (n, c) in error_code_samples() {
        out.insert(n, postcard::to_allocvec(&c).unwrap());
    }
    let principals = [
        ("principal.device", Principal::Device),
        ("principal.agent", agent_principal()),
        ("principal.web", Principal::Web { session: web_session() }),
    ];
    for (n, p) in principals {
        // 请求头的形状不变；次版本取固定值，样例才不随 WIRE_MINOR 变。
        let mut header = RequestHeader::new(7, p, &Request::NodeInfo);
        header.minor = 0;
        out.insert(n, postcard::to_allocvec(&header).unwrap());
    }
    // 信封的形状不变；次版本取固定值，样例才不随 WIRE_MINOR 变。
    let mut envelope = ResponseEnvelope::new(&Response::Ok);
    envelope.minor = 0;
    out.insert("envelope.response", postcard::to_allocvec(&envelope).unwrap());
    out.insert("frame.input_resize", postcard::to_allocvec(&Input::Resize(size())).unwrap());
    let audit = PrincipalRef::Agent { id: key(8).public(), label: "ci".into() };
    out.insert("audit.principal_agent", postcard::to_allocvec(&audit).unwrap());
    out
}

/// 按名字解码再编码：能解码并且得到同样的字节。
fn round_trips(name: &str, bytes: &[u8]) -> bool {
    fn rt<T: serde::Serialize + serde::de::DeserializeOwned>(bytes: &[u8]) -> bool {
        postcard::from_bytes::<T>(bytes).ok().and_then(|v| postcard::to_allocvec(&v).ok()).as_deref() == Some(bytes)
    }
    match name.split('.').next().unwrap() {
        "request" => rt::<Request>(bytes),
        "response" => rt::<Response>(bytes),
        "error" => rt::<ErrorCode>(bytes),
        "principal" => rt::<RequestHeader>(bytes),
        "envelope" => rt::<ResponseEnvelope>(bytes),
        "frame" => rt::<Input>(bytes),
        "audit" => rt::<PrincipalRef>(bytes),
        other => panic!("fixtures 里有不认识的种类 {other}"),
    }
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// (次版本, 名字, 字节)，按次版本从小到大。
fn load_fixtures() -> Vec<(u16, String, Vec<u8>)> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir(fixtures_dir()) else { return out };
    for entry in dir {
        let path = entry.unwrap().path();
        let file = path.file_name().unwrap().to_str().unwrap().to_string();
        let Some(ver) = file.strip_suffix(".txt") else { continue };
        let (major, minor) = ver.split_once('.').expect("文件名是 主版本.次版本.txt");
        assert_eq!(major.parse::<u16>().unwrap(), WIRE_VERSION, "fixtures 里有别的主版本的文件 {file}：升主版本时删掉旧的");
        let minor: u16 = minor.parse().unwrap();
        for line in std::fs::read_to_string(&path).unwrap().lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')) {
            let (name, hex) = line.split_once(' ').expect("每行是 名字 十六进制");
            out.push((minor, name.to_string(), hex_decode(hex.trim()).expect("十六进制")));
        }
    }
    out.sort_by_key(|(m, _, _)| *m);
    out
}

/// FP-7.1-01：已有消息的编码不变，存着的都能解码。
#[test]
fn existing_messages_keep_their_encoding() {
    let fixtures = load_fixtures();
    let samples = encoded_samples();
    let bless = std::env::var("NEXUS_BLESS").is_ok_and(|v| v == "1");
    for (minor, name, bytes) in &fixtures {
        let ok = *minor <= WIRE_MINOR;
        assert!(ok, "fixtures 里有比当前次版本 {WIRE_MINOR} 新的文件");
        assert!(round_trips(name, bytes), "{WIRE_VERSION}.{minor} 的 {name} 解码不了或者再编码不一样");
        if let Some(now) = samples.get(name.as_str()) {
            assert_eq!(
                hex_encode(now),
                hex_encode(bytes),
                "{name} 的编码和 {WIRE_VERSION}.{minor} 里存着的不同：这是不兼容的改动（见 docs/protocol.md）"
            );
        }
    }
    let known: std::collections::BTreeSet<&str> = fixtures.iter().map(|(_, n, _)| n.as_str()).collect();
    let new: Vec<(&str, &Vec<u8>)> = samples.iter().filter(|(n, _)| !known.contains(*n)).map(|(n, b)| (*n, b)).collect();
    if new.is_empty() {
        return;
    }
    let file = fixtures_dir().join(format!("{WIRE_VERSION}.{WIRE_MINOR}.txt"));
    if bless {
        std::fs::create_dir_all(fixtures_dir()).unwrap();
        let mut text = std::fs::read_to_string(&file).unwrap_or_else(|_| {
            format!("# 协议 {WIRE_VERSION}.{WIRE_MINOR} 新增的消息的编码（tests/compat.rs 生成，不要手改）\n")
        });
        for (n, b) in &new {
            text.push_str(&format!("{n} {}\n", hex_encode(b)));
        }
        std::fs::write(&file, text).unwrap();
    } else {
        let names: Vec<&str> = new.iter().map(|(n, _)| *n).collect();
        panic!(
            "这些消息还没有 fixtures：{names:?}。如果是这个次版本新加的，用 NEXUS_BLESS=1 生成 {}",
            file.display()
        );
    }
}

/// FP-7.1-02、03：不认识的请求体、回应体、错误码。
#[test]
fn unknown_messages_are_reported_not_fatal() {
    // 一个「以后」的请求：种类编号超出现在的范围。
    let mut header = RequestHeader::new(3, Principal::Device, &Request::NodeInfo);
    let bytes = postcard::to_allocvec(&header).unwrap();
    // 请求体是最后一个字段：长度 1、内容是种类编号。换成一个很大的编号。
    let mut future = bytes.clone();
    let last = future.len() - 1;
    future[last] = 120;
    header = postcard::from_bytes(&future).expect("请求头本身照样能读");
    let e = header.request("home（Nexus 0.1.0）").unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedVersion);
    assert!(e.message.contains("home（Nexus 0.1.0）") && e.message.contains("升级"), "{}", e.message);

    let env = ResponseEnvelope::new(&Response::Ok);
    let mut b = postcard::to_allocvec(&env).unwrap();
    let last = b.len() - 1;
    b[last] = 120;
    let opened = postcard::from_bytes::<ResponseEnvelope>(&b).unwrap().open();
    match opened {
        Response::Error(e) => {
            assert_eq!(e.code, ErrorCode::UnsupportedVersion);
            assert!(e.message.contains("较新"), "{}", e.message);
        }
        other => panic!("{other:?}"),
    }

    // 以后的错误码：当作未知错误，说明保留。
    let mut err = postcard::to_allocvec(&WireError::new(ErrorCode::Denied, "说明")).unwrap();
    err[0] = 60;
    let decoded: WireError = postcard::from_bytes(&err).unwrap();
    assert_eq!(decoded.code, ErrorCode::Unknown);
    assert_eq!(decoded.message, "说明");
}
