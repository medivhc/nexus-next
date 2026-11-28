//! 恢复管理设备的验收场景（M6.2，R64）。功能点见 docs/feature-points/M6.2.md。

use nexus_testkit::{scenario, World};
use serde_json::Value;

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("输出不是合法的 JSON（{e}）：{text:?}"))
}

fn role(ls: &Value, name: &str) -> String {
    ls["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == name)
        .unwrap_or_else(|| panic!("没有 {name}：{ls}"))["role"]
        .as_str()
        .unwrap()
        .to_string()
}

/// 建网：laptop 是管理设备，返回恢复码；members 依次加入。
async fn network(w: &World, members: &[&str]) -> String {
    let init = w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
    let code = init["recovery_code"].as_str().unwrap().to_string();
    for m in members {
        let join = w.cli_ok(m, &["join"]).await;
        w.cli_ok(
            "laptop",
            &[
                "device",
                "approve",
                join["code"].as_str().unwrap(),
                "--name",
                m,
            ],
        )
        .await;
    }
    code
}

scenario! {
    name: recover_admin_on_a_member,
    covers: "R64",
    nodes: ["laptop", "home", "office", "phone"],
    real: yes,
    run: |w: World| async move {
        let code = network(&w, &["home", "office"]).await;
        w.take_offline("laptop");
        // FP-6.2-01：分组、大写也行
        let grouped: String = code.to_uppercase().as_bytes().chunks(8).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join("-");
        let r = w.cli_ok("home", &["recover", &grouped]).await;
        assert_eq!(r["roster_version"], 4, "{r}");
        let ls = w.cli_ok("home", &["ls"]).await;
        assert_eq!(role(&ls, "home"), "admin");
        assert_eq!(role(&ls, "laptop"), "member");
        assert_eq!(role(&w.cli_ok("office", &["ls"]).await, "home"), "admin");
        // 新的管理设备能管理
        let join = w.cli_ok("phone", &["join"]).await;
        w.cli_ok("home", &["device", "approve", join["code"].as_str().unwrap(), "--name", "phone"]).await;
        w.cli_ok("home", &["device", "revoke", "laptop"]).await;
        let ls = w.cli_ok("office", &["ls"]).await;
        let names: Vec<&str> = ls["devices"].as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"phone") && !names.contains(&"laptop"), "{ls}");
    },
}

scenario! {
    name: recover_refuses_bad_input,
    covers: "R64",
    nodes: ["laptop", "home", "other", "lonely"],
    real: yes,
    run: |w: World| async move {
        let code = network(&w, &["home"]).await;
        let foreign = w.cli_ok("other", &["init", "--name", "other"]).await["recovery_code"].as_str().unwrap().to_string();
        // FP-6.2-02
        let wrong = w.cli("home", &["recover", &foreign, "--json"]).await;
        assert_eq!(wrong.code, 107, "stderr: {}", wrong.stderr);
        assert!(json(&wrong.stderr)["message"].as_str().unwrap().contains("不属于这个网络"), "{}", wrong.stderr);
        assert_eq!(w.cli_ok("home", &["status"]).await["roster_version"], 2);
        assert_ne!(w.cli("home", &["agent", "ls", "--json"]).await.code, 0, "失败后本机不应当是管理设备");
        assert_eq!(w.cli("home", &["recover", "1234", "--json"]).await.code, 107);
        let admin = w.cli("laptop", &["recover", &code, "--json"]).await;
        assert_eq!(admin.code, 106, "stderr: {}", admin.stderr);
        assert!(json(&admin.stderr)["message"].as_str().unwrap().contains("已经是管理设备"), "{}", admin.stderr);
        assert_eq!(w.cli("lonely", &["recover", &code, "--json"]).await.code, 106);
    },
}

scenario! {
    name: recovered_admin_revokes_an_unlisted_agent,
    covers: "R64",
    nodes: ["laptop", "home", "office"],
    real: yes,
    run: |w: World| async move {
        let code = network(&w, &["home", "office"]).await;
        let cred = w.cli_ok("laptop", &["agent", "issue", "--label", "ci", "--grant", "term.exec@office", "--ttl", "1h"]).await;
        let cred = cred["credential"].as_str().unwrap().to_string();
        let ran = w.cli("home", &["--agent", &cred, "exec", "office", "--", "true"]).await;
        assert_eq!(ran.code, 0, "stderr: {}", ran.stderr);
        w.take_offline("laptop");
        w.cli_ok("home", &["recover", &code]).await;
        // FP-6.2-03：从 office 的审计记录取 agent 的标识
        let audit = w.cli_ok("office", &["audit"]).await;
        let id = audit["events"].as_array().unwrap().iter()
            .find_map(|r| r["principal"]["agent_id"].as_str().map(str::to_string))
            .unwrap_or_else(|| panic!("审计里没有 agent：{audit}"));
        assert_eq!(w.cli("home", &["agent", "revoke", "ci", "--json"]).await.code, 109, "标签只在原来的管理设备上");
        w.cli_ok("home", &["agent", "revoke", &id]).await;
        let denied = w.cli("home", &["--agent", &cred, "exec", "office", "--", "true"]).await;
        assert_eq!(denied.code, 105, "stderr: {}", denied.stderr);
        assert_eq!(w.cli("home", &["agent", "revoke", "nobody", "--json"]).await.code, 109);
    },
}
