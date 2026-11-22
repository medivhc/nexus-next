//! 升级与回滚（M7.3，R67）。功能点见 docs/feature-points/M7.3.md。
//! 只在真实后端：要真的换二进制、重新启动常驻程序。home 用自己的一份二进制，升级不会改到构建出来的那个。

use nexus_testkit::{scenario, World};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("输出不是合法的 JSON（{e}）：{text:?}"))
}

/// 一个可执行的脚本。
fn script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

async fn started_at(w: &World, node: &str) -> Value {
    w.cli_ok(node, &["status"]).await["started_at_ms"].clone()
}

/// 等 `nexus upgrade --status` 变成 `state`。
async fn wait_state(w: &World, node: &str, state: &str, within: Duration) -> Value {
    let deadline = std::time::Instant::now() + within;
    loop {
        let out = w.cli(node, &["upgrade", "--status", "--json"]).await;
        if out.code == 0 {
            let v = json(&out.stdout);
            if v["state"] == state {
                return v;
            }
        }
        assert!(std::time::Instant::now() < deadline, "{node} 的升级状态没有变成 {state}：{} {}", out.stdout, out.stderr);
        w.run_for(Duration::from_millis(250)).await;
    }
}

scenario! {
    name: upgrade_restarts_and_confirms,
    covers: "R67",
    nodes: ["laptop", "home"],
    sim: (pending "要真的换二进制、重新启动常驻程序"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let exe = w.use_own_binary("home", &["--local-only"]).await;
        let t = w.term("laptop", &["ssh", "home"]).await;
        t.expect_prompt().await;
        let before = started_at(&w, "home").await;
        // 「新版本」：构建出来的二进制的另一份
        let new = w.home_dir("home").join("nexus-new");
        std::fs::copy(w.nexus_bin(), &new).unwrap();
        // FP-7.3-01
        let r = w.cli_ok("home", &["upgrade", "--file", new.to_str().unwrap()]).await;
        assert_eq!(r["to"], env!("CARGO_PKG_VERSION"), "{r}");
        assert_eq!(r["state"], "pending", "{r}");
        let st = wait_state(&w, "home", "confirmed", Duration::from_secs(20)).await;
        assert_eq!(st["from"], env!("CARGO_PKG_VERSION"), "{st}");
        assert_ne!(started_at(&w, "home").await, before, "常驻程序应当重新启动过");
        assert!(exe.with_extension("prev").exists(), "原来的二进制应当留作 .prev");
        // 会话没断
        t.send("printf '%s%s\\n' up graded\r").await;
        t.expect_line("upgraded").await;
        // FP-7.3-04：手动换回
        let back = w.cli_ok("home", &["upgrade", "--rollback"]).await;
        assert_eq!(back["state"], "rolled_back", "{back}");
        let st = wait_state(&w, "home", "rolled_back", Duration::from_secs(20)).await;
        assert!(st["message"].as_str().unwrap().contains("手动"), "{st}");
        w.cli_ok("laptop", &["status", "home"]).await;
    },
}

scenario! {
    name: upgrade_refuses_a_bad_binary,
    covers: "R67",
    nodes: ["home"],
    sim: (pending "要真的换二进制"),
    real: yes,
    run: |w: World| async move {
        w.cli_ok("home", &["init", "--name", "home"]).await;
        let exe = w.use_own_binary("home", &["--local-only"]).await;
        let original = std::fs::read(&exe).unwrap();
        let before = started_at(&w, "home").await;
        // FP-7.3-02
        let bad = w.home_dir("home").join("not-nexus");
        script(&bad, "echo hello");
        let out = w.cli("home", &["upgrade", "--file", bad.to_str().unwrap(), "--json"]).await;
        assert_eq!(out.code, 107, "stderr: {}", out.stderr);
        assert!(json(&out.stderr)["message"].as_str().unwrap().contains("--version"), "{}", out.stderr);
        let missing = w.cli("home", &["upgrade", "--file", "/nonexistent/nexus", "--json"]).await;
        assert_eq!(missing.code, 107, "stderr: {}", missing.stderr);
        assert_eq!(started_at(&w, "home").await, before, "不应当重新启动");
        assert_eq!(std::fs::read(&exe).unwrap(), original, "二进制不应当变");
    },
}

scenario! {
    name: upgrade_rolls_back_when_offline,
    covers: "R67",
    nodes: ["laptop", "home"],
    sim: (pending "要真的换二进制、重新启动常驻程序"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let exe = w.use_own_binary("home", &["--local-only"]).await;
        let original = std::fs::read(&exe).unwrap();
        // FP-7.3-03：新版本启动后不用任何网络
        let real = w.home_dir("home").join("nexus-real");
        std::fs::copy(w.nexus_bin(), &real).unwrap();
        let wrapper = w.home_dir("home").join("nexus-offline");
        script(&wrapper, &format!("NEXUS_TEST_NO_NETWORK=1 exec {} \"$@\"", real.display()));
        let r = w.cli_ok("home", &["upgrade", "--file", wrapper.to_str().unwrap(), "--confirm-within", "4s"]).await;
        assert_eq!(r["state"], "pending", "{r}");
        let st = wait_state(&w, "home", "rolled_back", Duration::from_secs(30)).await;
        assert!(st["message"].as_str().unwrap().contains("连不上"), "{st}");
        assert_eq!(std::fs::read(&exe).unwrap(), original, "应当换回原来的二进制");
        // 换回之后又在线
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let s = w.cli("laptop", &["status", "home", "--json"]).await;
            if s.code == 0 && json(&s.stdout)["online"] == true {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "换回之后 home 应当又在线：{}", s.stdout);
            w.run_for(Duration::from_millis(500)).await;
        }
    },
}
