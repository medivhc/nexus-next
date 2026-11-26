//! Android 应用内加入网络的验收场景（M11.2，R79、R80）。功能点见 docs/feature-points/M11.2.md。
//!
//! 应用的界面经控制通道调用 `nexus_cli::app::refresh`；这里用测试工具的 `World::app_screen` 做同样的调用。

use nexus_cli::app::{JoinView, Screen, RENEW_BEFORE_MS};
use nexus_testkit::{scenario, World};
use serde_json::Value;

fn joining(screen: Screen) -> JoinView {
    match screen {
        Screen::Joining(v) => v,
        other => panic!("界面应当显示加入码：{other:?}"),
    }
}

fn joined(screen: &Screen) -> (String, Vec<String>) {
    match screen {
        Screen::Joined { name, devices } => {
            let mut names: Vec<String> = devices.iter().map(|d| d.name.clone()).collect();
            names.sort();
            (name.clone(), names)
        }
        other => panic!("界面应当显示已加入：{other:?}"),
    }
}

fn short(v: &JoinView) -> String {
    v.short_code.clone().unwrap_or_else(|| panic!("应当有短码：{v:?}"))
}

async fn approve_fails(w: &World, code: &str) -> i32 {
    let out = w.cli("laptop", &["device", "approve", code, "--json"]).await;
    assert_ne!(out.code, 0, "旧短码不该还能用：{}", out.stdout);
    out.code
}

scenario! {
    name: join_from_the_app_screen,
    covers: "R79, R80",
    nodes: ["laptop", "phone"],
    real: yes,
    run: |w: World| async move {
        w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
        // FP-11.2-01：还没加入的手机显示加入码
        let view = joining(w.app_screen("phone", None, false, None).await);
        let code = short(&view);
        let (a, b) = code.split_once('-').expect("两组");
        assert_eq!((a.len(), b.len()), (5, 5), "{code}");
        assert!(view.code.starts_with("nxj2-"), "{view:?}");
        assert_eq!(view.qr_text, view.code.to_ascii_uppercase());
        assert_eq!(view.qr.rows.len(), view.qr.size);
        assert!(view.expires_at_ms > w.now().0);
        // 管理设备用短码批准
        let ok = w.cli_ok("laptop", &["device", "approve", &code, "--name", "phone"]).await;
        assert_eq!(ok["name"], "phone");
        // 界面带着刚才的加入码再刷新：已加入
        let screen = w.app_screen("phone", Some(view), false, None).await;
        assert_eq!(joined(&screen), ("phone".to_string(), vec!["laptop".to_string(), "phone".to_string()]));
    },
}

scenario! {
    name: the_code_stays_until_it_is_about_to_expire,
    covers: "R79",
    nodes: ["laptop", "phone"],
    real: yes,
    run: |w: World| async move {
        w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
        // FP-11.2-02：带着 A 刷新几次，一直是 A
        let a = joining(w.app_screen("phone", None, false, None).await);
        for _ in 0..3 {
            let again = joining(w.app_screen("phone", Some(a.clone()), false, None).await);
            assert_eq!(again, a);
        }
        // 离到期不到 30 秒：换成 B，A 失效
        let near = a.expires_at_ms - RENEW_BEFORE_MS + 1;
        let b = joining(w.app_screen("phone", Some(a.clone()), false, Some(near)).await);
        assert_ne!(short(&a), short(&b));
        assert_eq!(approve_fails(&w, &short(&a)).await, 109);
        let ok = w.cli_ok("laptop", &["device", "approve", &short(&b), "--name", "phone"]).await;
        assert_eq!(ok["name"], "phone");
    },
}

scenario! {
    name: renew_the_code_by_hand,
    covers: "R79",
    nodes: ["laptop", "phone"],
    real: yes,
    run: |w: World| async move {
        w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
        // FP-11.2-03：点「换一个加入码」
        let a = joining(w.app_screen("phone", None, false, None).await);
        let b = joining(w.app_screen("phone", Some(a.clone()), true, None).await);
        assert_ne!(short(&a), short(&b));
        assert_ne!(a.code, b.code);
        assert_eq!(approve_fails(&w, &short(&a)).await, 109);
        w.cli_ok("laptop", &["device", "approve", &short(&b), "--name", "phone"]).await;
        let (name, _) = joined(&w.app_screen("phone", Some(b), false, None).await);
        assert_eq!(name, "phone");
    },
}

scenario! {
    name: an_admin_device_shows_joined,
    covers: "R79",
    nodes: ["laptop", "phone"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["phone"]).await;
        let before = w.cli_ok("laptop", &["status"]).await["roster_version"].clone();
        // FP-11.2-04：已经加入的设备直接显示已加入，不发起加入
        for node in ["laptop", "phone"] {
            let screen = w.app_screen(node, None, false, None).await;
            let (name, names) = joined(&screen);
            assert_eq!(name, node);
            assert_eq!(names, ["laptop", "phone"]);
            if let Screen::Joined { devices, .. } = &screen {
                let admin: Vec<&str> = devices.iter().filter(|d| d.admin).map(|d| d.name.as_str()).collect();
                assert_eq!(admin, ["laptop"]);
                assert!(devices.iter().filter(|d| d.is_self).count() == 1);
            }
        }
        assert_eq!(w.cli_ok("laptop", &["status"]).await["roster_version"], before);
    },
}

scenario! {
    name: scan_the_qr_without_rendezvous,
    covers: "R79, R80",
    nodes: ["laptop", "phone"],
    sim: (pending "仿真里碰头服务总在：在真实后端用 --rendezvous disabled 测"),
    real: yes,
    run: |w: World| async move {
        w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
        // FP-11.2-05：没有碰头服务，只有长码和二维码
        w.restart("phone", &["--local-only", "--rendezvous", "disabled"]).await;
        let view = joining(w.app_screen("phone", None, false, None).await);
        assert_eq!(view.short_code, None);
        assert!(view.short_code_error.as_deref().unwrap_or_default().contains("碰头服务"), "{view:?}");
        // FP-11.2-07：扫码得到的文字（大写，可能带换行）直接用来批准
        let scanned = format!("{}\n", view.qr_text);
        let ok: Value = w.cli_ok("laptop", &["device", "approve", &scanned, "--name", "phone"]).await;
        assert_eq!(ok["name"], "phone");
        let (name, _) = joined(&w.app_screen("phone", Some(view), false, None).await);
        assert_eq!(name, "phone");
    },
}
