//! 网页节点发布硬件能力（M8.3）：摄像头、麦克风、位置。功能点见 docs/feature-points/M8.3.md。

use nexus_core_proto::webauthn::soft::SoftAuthenticator;
use nexus_testkit::nexus_web_core::{HardwareCapture, HardwareKind, HardwareProvider, HardwareState};
use nexus_testkit::{scenario, World};
use nexus_wire::WireError;

fn phone_passkey() -> SoftAuthenticator {
    SoftAuthenticator::new([43; 32], "bewith.link", "https://bewith.link")
}

struct FakeHardware;

impl HardwareProvider for FakeHardware {
    async fn capture(&self, kind: HardwareKind) -> Result<HardwareCapture, WireError> {
        Ok(match kind {
            HardwareKind::Camera => HardwareCapture::Camera { mime: "image/jpeg".into(), data: vec![0xff, 0xd8, 0x01, 0x02] },
            HardwareKind::Microphone => HardwareCapture::Microphone { mime: "audio/webm".into(), duration_ms: 500, data: vec![0x1a, 0x45, 0xdf, 0xa3] },
            HardwareKind::Location => HardwareCapture::Location { lat_e7: 312_304_000, lon_e7: 1_214_737_000, accuracy_cm: 1250, timestamp_ms: 1_700_000_000_000 },
        })
    }
}

scenario! {
    name: web_hardware_requires_explicit_consent_per_capability,
    covers: "R72, R31",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let link = w.cli_ok("laptop", &["web", "enroll", "--label", "phone"]).await["url"].as_str().unwrap().to_string();
        let browser = w.browser().await;
        let mut auth = phone_passkey();
        browser.enroll(&link, &auth).await.expect("登记应当成功");
        browser.login(&w, &link, &mut auth).await.expect("登录应当成功");

        let state = HardwareState::new();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let web_id = browser.node_id().to_string();
        let web_hint = browser.hint();

        let serve = browser.serve_hardware(state.clone(), FakeHardware, stop_rx, |_| {});
        let test = async {
            // 默认全部关闭：列表为空，采集任何能力都被拒绝（退出码 105）。
            let v = w.cli_ok("laptop", &["hw", "ls", &web_id, "--hint", &web_hint]).await;
            assert_eq!(v["capabilities"], serde_json::json!([]), "{v}");
            for kind in ["camera", "microphone", "location"] {
                let out = w.cli("laptop", &["hw", "capture", &web_id, kind, "--hint", &web_hint]).await;
                assert_eq!(out.code, 105, "{kind}: {}", out.stderr);
            }

            // 仅显式同意位置：位置放行，摄像头与麦克风仍然拒绝。
            state.enable(HardwareKind::Location);
            let v = w.cli_ok("laptop", &["hw", "ls", &web_id, "--hint", &web_hint]).await;
            assert_eq!(v["capabilities"], serde_json::json!(["location"]), "{v}");
            let loc = w.cli_ok("laptop", &["hw", "capture", &web_id, "location", "--hint", &web_hint]).await;
            assert_eq!(loc["kind"], "location", "{loc}");
            assert_eq!(loc["lat_e7"], 312_304_000, "{loc}");
            assert_eq!(w.cli("laptop", &["hw", "capture", &web_id, "camera", "--hint", &web_hint]).await.code, 105);
            assert_eq!(w.cli("laptop", &["hw", "capture", &web_id, "microphone", "--hint", &web_hint]).await.code, 105);

            // 撤回同意后立即拒绝。
            state.disable(HardwareKind::Location);
            assert_eq!(w.cli("laptop", &["hw", "capture", &web_id, "location", "--hint", &web_hint]).await.code, 105);
            let _ = stop_tx.send(());
        };
        tokio::join!(serve, test);
    },
}

scenario! {
    name: web_hardware_captures_camera_mic_location_and_shows_caller,
    covers: "R72",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let link = w.cli_ok("laptop", &["web", "enroll", "--label", "phone"]).await["url"].as_str().unwrap().to_string();
        let browser = w.browser().await;
        let mut auth = phone_passkey();
        browser.enroll(&link, &auth).await.unwrap();
        browser.login(&w, &link, &mut auth).await.unwrap();

        let state = HardwareState::new();
        state.enable(HardwareKind::Camera);
        state.enable(HardwareKind::Microphone);
        state.enable(HardwareKind::Location);

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let web_id = browser.node_id().to_string();
        let web_hint = browser.hint();

        let ui_events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let ui_events_cb = ui_events.clone();
        let serve = browser.serve_hardware(state.clone(), FakeHardware, stop_rx, move |ev| {
            ui_events_cb.lock().unwrap().push(ev);
        });
        let test = async {
            let cam = w.cli_ok("laptop", &["hw", "capture", &web_id, "camera", "--hint", &web_hint]).await;
            assert_eq!(cam["kind"], "camera", "{cam}");
            assert_eq!(cam["mime"], "image/jpeg", "{cam}");
            assert_eq!(cam["data_base64"], "/9gBAg==", "{cam}");

            let mic = w.cli_ok("laptop", &["hw", "capture", &web_id, "microphone", "--hint", &web_hint]).await;
            assert_eq!(mic["kind"], "microphone", "{mic}");
            assert_eq!(mic["duration_ms"], 500, "{mic}");

            let loc = w.cli_ok("laptop", &["hw", "capture", &web_id, "location", "--hint", &web_hint]).await;
            assert_eq!(loc["lat_e7"], 312_304_000, "{loc}");
            assert_eq!(loc["lon_e7"], 1_214_737_000, "{loc}");

            let _ = stop_tx.send(());
        };
        tokio::join!(serve, test);

        // 页面侧收到每一次「谁在用」的开始与结束通知，且调用方准确标明为 laptop。
        let events = ui_events.lock().unwrap();
        assert_eq!(events.len(), 6, "{events:?}");
        for kind in [HardwareKind::Camera, HardwareKind::Microphone, HardwareKind::Location] {
            assert!(events.iter().any(|e| e.caller == "laptop" && e.kind == kind && e.active), "{events:?}");
            assert!(events.iter().any(|e| e.caller == "laptop" && e.kind == kind && !e.active), "{events:?}");
        }
        assert!(state.active_users().is_empty());
    },
}

scenario! {
    name: web_hardware_rejects_untrusted_device,
    covers: "R72, R31",
    nodes: ["laptop", "stranger"],
    real: yes,
    run: |w: World| async move {
        w.cli_ok("laptop", &["init"]).await;
        w.cli_ok("stranger", &["init"]).await;
        let link = w.cli_ok("laptop", &["web", "enroll", "--label", "phone"]).await["url"].as_str().unwrap().to_string();
        let browser = w.browser().await;
        let mut auth = phone_passkey();
        browser.enroll(&link, &auth).await.unwrap();
        browser.login(&w, &link, &mut auth).await.unwrap();

        let state = HardwareState::new();
        state.enable(HardwareKind::Camera);

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let web_id = browser.node_id().to_string();
        let web_hint = browser.hint();

        let serve = browser.serve_hardware(state.clone(), FakeHardware, stop_rx, |_| {});
        let test = async {
            assert_eq!(w.cli("stranger", &["hw", "ls", &web_id, "--hint", &web_hint]).await.code, 105);
            assert_eq!(w.cli("stranger", &["hw", "capture", &web_id, "camera", "--hint", &web_hint]).await.code, 105);
            let _ = stop_tx.send(());
        };
        tokio::join!(serve, test);
    },
}

scenario! {
    name: web_hardware_stops_when_session_closes,
    covers: "R72, R32",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        w.cli_ok("laptop", &["init"]).await;
        let link = w.cli_ok("laptop", &["web", "enroll", "--label", "phone"]).await["url"].as_str().unwrap().to_string();
        let browser = w.browser().await;
        let mut auth = phone_passkey();
        browser.enroll(&link, &auth).await.unwrap();
        browser.login(&w, &link, &mut auth).await.unwrap();

        let state = HardwareState::new();
        state.enable(HardwareKind::Location);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let _ = stop_tx.send(());
        browser.serve_hardware(state.clone(), FakeHardware, stop_rx, |_| {}).await;
        assert!(state.enabled_list().is_empty());
        assert!(state.active_users().is_empty());
    },
}

scenario! {
    name: web_hardware_page_shows_commands_that_work,
    covers: "R72",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let link = w.cli_ok("laptop", &["web", "enroll", "--label", "phone"]).await["url"].as_str().unwrap().to_string();
        let browser = w.browser().await;
        let mut auth = phone_passkey();
        browser.enroll(&link, &auth).await.unwrap();
        browser.login(&w, &link, &mut auth).await.unwrap();

        // 页面上显示的命令原样复制到设备上就能用：不需要用户另外去找网页节点的标识和地址提示。
        let ls_cmd = browser.hardware_command(None);
        let capture_cmd = browser.hardware_command(Some(HardwareKind::Location));
        assert!(ls_cmd.starts_with("nexus hw ls "), "{ls_cmd}");
        assert!(capture_cmd.starts_with("nexus hw capture "), "{capture_cmd}");
        let args = |cmd: &str| cmd.split_whitespace().skip(1).map(str::to_string).collect::<Vec<_>>();

        let state = HardwareState::new();
        state.enable(HardwareKind::Location);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let serve = browser.serve_hardware(state.clone(), FakeHardware, stop_rx, |_| {});
        let test = async {
            let ls_args = args(&ls_cmd);
            let v = w.cli_ok("home", &ls_args.iter().map(String::as_str).collect::<Vec<_>>()).await;
            assert_eq!(v["capabilities"], serde_json::json!(["location"]), "{v}");
            let cap_args = args(&capture_cmd);
            let loc = w.cli_ok("home", &cap_args.iter().map(String::as_str).collect::<Vec<_>>()).await;
            assert_eq!(loc["lat_e7"], 312_304_000, "{loc}");
            let _ = stop_tx.send(());
        };
        tokio::join!(serve, test);
    },
}
