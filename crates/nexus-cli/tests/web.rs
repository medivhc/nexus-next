//! 网页节点的验收场景（M2）。功能点见 docs/feature-points/M2.1.md 起。

use nexus_core_proto::webauthn::soft::SoftAuthenticator;
use nexus_testkit::{scenario, World};
use std::time::Duration;

fn phone_passkey() -> SoftAuthenticator {
    SoftAuthenticator::new([42; 32], "bewith.link", "https://bewith.link")
}

/// 在管理设备上发起登记，返回登记链接。
async fn start_enroll(w: &World, admin: &str, args: &[&str]) -> String {
    let mut full = vec!["web", "enroll"];
    full.extend_from_slice(args);
    let v = w.cli_ok(admin, &full).await;
    assert!(v["expires_at_ms"].is_u64(), "{v}");
    let url = v["url"].as_str().expect("应当输出登记链接").to_string();
    assert!(url.starts_with("https://bewith.link/#enroll="), "{url}");
    url
}

async fn passkeys(w: &World, node: &str) -> Vec<serde_json::Value> {
    w.cli_ok(node, &["web", "ls"]).await["passkeys"].as_array().expect("应当有 passkeys 数组").clone()
}

scenario! {
    name: passkey_can_be_enrolled,
    covers: "R29",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let link = start_enroll(&w, "laptop", &["--label", "phone"]).await;
        let browser = w.browser().await;
        let label = browser.enroll(&link, &phone_passkey()).await.expect("登记应当成功");
        assert_eq!(label, "phone");

        // 名册推送到了其他设备。
        let keys = passkeys(&w, "home").await;
        assert_eq!(keys.len(), 1, "{keys:?}");
        assert_eq!(keys[0]["label"], "phone");
        assert_eq!(keys[0]["rp_id"], "bewith.link");
        assert_eq!(keys[0]["grants"], serde_json::json!(["term.session.*@**", "node.info@**"]));
        assert!(keys[0]["added_at_ms"].is_u64());
        let text = w.cli("home", &["web", "ls"]).await;
        assert!(text.stdout.contains("phone"), "{}", text.stdout);

        // 登记码只能用一次。
        let again = SoftAuthenticator::new([43; 32], "bewith.link", "https://bewith.link");
        assert!(browser.enroll(&link, &again).await.is_err());
        assert_eq!(passkeys(&w, "laptop").await.len(), 1);
        // 编出来的登记码不行。
        let forged = link.replace("enroll=", "enroll=00");
        assert!(browser.enroll(&forged, &again).await.is_err());
    },
}

scenario! {
    name: enroll_codes_expire_and_must_match_the_page,
    covers: "R29",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let browser = w.browser().await;
        let link = start_enroll(&w, "laptop", &["--label", "phone", "--ttl", "2s"]).await;
        w.run_for(Duration::from_secs(3)).await;
        assert!(browser.enroll(&link, &phone_passkey()).await.is_err(), "过期的登记码不能用");

        // 通行密钥必须在登记链接所指的页面上创建（依赖方一致）。
        let link = start_enroll(&w, "laptop", &["--label", "phone"]).await;
        let elsewhere = SoftAuthenticator::new([42; 32], "evil.example", "https://evil.example");
        assert!(browser.enroll(&link, &elsewhere).await.is_err());
        assert!(passkeys(&w, "laptop").await.is_empty());
        // 同一个登记码仍然可以完成正确的登记。
        assert_eq!(browser.enroll(&link, &phone_passkey()).await.as_deref(), Ok("phone"));
    },
}

scenario! {
    name: only_the_admin_enrolls_and_names_are_unique,
    covers: "R29",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let out = w.cli("home", &["web", "enroll", "--label", "phone"]).await;
        assert_eq!(out.code, 106, "{}", out.stderr);

        let link = start_enroll(&w, "laptop", &["--label", "phone"]).await;
        w.browser().await.enroll(&link, &phone_passkey()).await.unwrap();
        for label in ["phone", "home", "my phone"] {
            let out = w.cli("laptop", &["web", "enroll", "--label", label]).await;
            assert_eq!(out.code, 107, "{label}: {}", out.stderr);
        }
    },
}

scenario! {
    name: passkey_can_be_revoked,
    covers: "R29",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let link = start_enroll(&w, "laptop", &["--label", "phone"]).await;
        w.browser().await.enroll(&link, &phone_passkey()).await.unwrap();

        let v = w.cli_ok("laptop", &["web", "revoke", "phone"]).await;
        assert_eq!(v["label"], "phone");
        assert_eq!(v["pushed"], 1, "{v}");
        assert!(passkeys(&w, "home").await.is_empty());
        let out = w.cli("laptop", &["web", "revoke", "phone"]).await;
        assert_eq!(out.code, 109, "{}", out.stderr);
        let out = w.cli("home", &["web", "revoke", "phone"]).await;
        assert_eq!(out.code, 106, "{}", out.stderr);
    },
}

scenario! {
    name: passkey_grants_can_be_narrowed,
    covers: "R29",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let link = start_enroll(&w, "laptop", &["--label", "kiosk", "--grant", "node.info@home"]).await;
        w.browser().await.enroll(&link, &phone_passkey()).await.unwrap();
        let keys = passkeys(&w, "laptop").await;
        assert_eq!(keys[0]["grants"], serde_json::json!(["node.info@home"]));
        let bad = w.cli("laptop", &["web", "enroll", "--label", "x", "--grant", "nonsense"]).await;
        assert_eq!(bad.code, 107, "{}", bad.stderr);
    },
}

/// 一个输出 `text` 的命令，但命令本身（回显在屏幕上）不包含完整的 `text`。
fn say(text: &str) -> String {
    let (a, b) = text.split_at(text.len() / 2);
    format!("printf '%s%s\\n' {a} {b}\r")
}

/// 登记 `label`（可带额外参数），返回入口链接（就是登记链接：包含入口设备和地址提示）。
async fn enrolled(w: &World, label: &str, extra: &[&str], auth: &SoftAuthenticator) -> String {
    let mut args = vec!["--label", label];
    args.extend_from_slice(extra);
    let link = start_enroll(w, "laptop", &args).await;
    w.browser().await.enroll(&link, auth).await.expect("登记应当成功");
    link
}

scenario! {
    name: web_login_reaches_every_device,
    covers: "R28",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let mut phone = phone_passkey();
        let entry = enrolled(&w, "phone", &[], &phone).await;

        let browser = w.browser().await;
        browser.login(&w, &entry, &mut phone).await.expect("登录应当成功");
        assert_eq!(browser.devices(), ["home", "laptop"]);
        assert_eq!(browser.status("home").await.expect("能查看 home 的状态").name, "home");

        let t = browser.term("home", "main").await.expect("能连上 home 的会话");
        t.expect_prompt().await;
        t.send(&say("hello-from-web")).await;
        t.expect_line("hello-from-web").await;
        assert_eq!(t.detach().await, 0);
        assert_eq!(browser.session_ls("home").await.unwrap(), ["main"]);

        // 审计记录里的主体是通行密钥。
        let audit = w.cli_ok("home", &["audit"]).await;
        assert!(
            audit["events"].as_array().unwrap().iter().any(|e| e["principal"]["passkey"] == "phone" && e["outcome"] == "allow"),
            "{audit}"
        );
    },
}

scenario! {
    name: a_signature_only_works_for_its_browser,
    covers: "R28",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let mut phone = phone_passkey();
        let entry = enrolled(&w, "phone", &[], &phone).await;

        let a = w.browser().await;
        let at = w.now();
        let signed_for_a = a.sign(&mut phone, at);
        let b = w.browser().await;
        assert!(b.login_with(&entry, at, signed_for_a.clone()).await.is_err(), "别的浏览器不能用这个签名");

        // 签名时间离现在超过 5 分钟。
        let late = w.browser().await;
        let old = nexus_core_proto::Timestamp(w.now().0 - 6 * 60 * 1000);
        let assertion = late.sign(&mut phone, old);
        assert!(late.login_with(&entry, old, assertion).await.is_err(), "过时的签名不能用");

        // 签名的浏览器自己可以。
        assert!(a.login_with(&entry, at, signed_for_a).await.is_ok());
    },
}

scenario! {
    name: web_login_sees_only_its_grants,
    covers: "R33",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let mut kiosk = phone_passkey();
        let entry = enrolled(&w, "kiosk", &["--grant", "node.info@home"], &kiosk).await;

        let browser = w.browser().await;
        browser.login(&w, &entry, &mut kiosk).await.unwrap();
        assert_eq!(browser.devices(), ["home"]);
        assert!(browser.status("home").await.is_ok());
        let denied = browser.term("home", "main").await.err().expect("不能连会话");
        assert_eq!(denied.code, nexus_wire::ErrorCode::Denied, "{denied:?}");
        assert_eq!(browser.session_ls("home").await.unwrap_err().code, nexus_wire::ErrorCode::Denied);
        assert!(browser.status("laptop").await.is_err(), "看不到的设备无从访问");
    },
}

scenario! {
    name: revoking_a_passkey_cuts_off_its_browser,
    covers: "R31",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let mut phone = phone_passkey();
        let entry = enrolled(&w, "phone", &[], &phone).await;
        let browser = w.browser().await;
        browser.login(&w, &entry, &mut phone).await.unwrap();
        let t = browser.term("home", "main").await.unwrap();
        t.expect_prompt().await;

        w.cli_ok("laptop", &["web", "revoke", "phone"]).await;
        assert_ne!(t.wait_exit().await, 0, "会话连接应当被断开");
        assert!(browser.status("home").await.is_err());
        assert!(w.browser().await.login(&w, &entry, &mut phone).await.is_err(), "撤销后不能再登录");
    },
}

scenario! {
    name: web_login_expires_after_twelve_hours,
    covers: "R31",
    nodes: ["laptop", "home"],
    real: (pending "真实后端不等 12 小时"),
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let mut phone = phone_passkey();
        let entry = enrolled(&w, "phone", &[], &phone).await;
        let browser = w.browser().await;
        browser.login(&w, &entry, &mut phone).await.unwrap();
        assert!(browser.status("home").await.is_ok());
        w.advance(Duration::from_secs(12 * 3600)).await;
        let e = browser.status("home").await.unwrap_err();
        assert_eq!(e.code, nexus_wire::ErrorCode::Denied);
        assert!(e.message.contains("过期"), "{e:?}");
    },
}
