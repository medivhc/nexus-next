//! 根密钥轮换的验收场景（M10.1，R75）。功能点见 docs/feature-points/M10.1.md。

use nexus_testkit::{scenario, World};

/// 建网：laptop 是管理设备，返回初始恢复码；members 依次加入。
async fn network(w: &World, members: &[&str]) -> String {
    let init = w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
    let code = init["recovery_code"].as_str().unwrap().to_string();
    for m in members {
        let join = w.cli_ok(m, &["join"]).await;
        w.cli_ok("laptop", &["device", "approve", join["code"].as_str().unwrap(), "--name", m]).await;
    }
    code
}

scenario! {
    name: rotate_root_updates_network_and_invalidates_old_recovery_code,
    covers: "R75, R64",
    nodes: ["laptop", "home", "office"],
    real: yes,
    run: |w: World| async move {
        let old_code = network(&w, &["home", "office"]).await;

        // 非管理设备不能轮换根密钥
        assert_eq!(w.cli("home", &["rotate-root", "--json"]).await.code, 106);

        // 在管理设备 laptop 上轮换根密钥
        let rot = w.cli_ok("laptop", &["rotate-root"]).await;
        let new_code = rot["recovery_code"].as_str().expect("应当输出新恢复码").to_string();
        assert_ne!(new_code, old_code, "新旧恢复码不能相同");
        assert_ne!(rot["old_root"], rot["new_root"], "{rot}");
        assert_eq!(rot["roster_version"], 4, "{rot}");
        assert_eq!(rot["pushed"], 2, "{rot}");

        // 轮换后网络无需重建：各设备照常互访与执行命令
        let out = w.cli("laptop", &["exec", "home", "--", "echo", "after-rotate"]).await;
        assert_eq!((out.code, out.stdout.trim()), (0, "after-rotate"), "{}", out.stderr);

        // 旧恢复码立即失效：当 laptop 离线后，home 用旧恢复码 recover 被拒绝（107）
        w.take_offline("laptop");
        let bad = w.cli("home", &["recover", &old_code, "--json"]).await;
        assert_eq!(bad.code, 107, "旧恢复码必须被拒绝：{}", bad.stderr);

        // 新恢复码在 home 上恢复管理设备成功
        let rec = w.cli_ok("home", &["recover", &new_code]).await;
        assert_eq!(rec["roster_version"], 5, "{rec}");
    },
}

scenario! {
    name: rotate_root_invalidates_old_agents_and_catches_up_offline_nodes,
    covers: "R75, R12",
    nodes: ["laptop", "home", "office"],
    real: yes,
    run: |w: World| async move {
        let _old_code = network(&w, &["home", "office"]).await;

        // 轮换前签发一份旧根 agent 凭据
        let old_agent = w.cli_ok("laptop", &["agent", "issue", "--label", "old-bot", "--grant", "term.exec@home", "--grant", "term.exec@office", "--ttl", "4h"]).await["credential"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(w.cli("laptop", &["--agent", &old_agent, "exec", "home", "--", "true"]).await.code, 0);

        // 让 office 暂时离线，在 laptop 上执行根密钥轮换
        w.take_offline("office");
        let rot = w.cli_ok("laptop", &["rotate-root"]).await;
        assert_eq!(rot["pushed"], 1, "{rot}");
        assert_eq!(rot["unreachable"], serde_json::json!(["office"]), "{rot}");

        // 已更新的 home 立即拒绝旧根签发的 agent 凭据（退出码 105）
        let denied = w.cli("laptop", &["--agent", &old_agent, "exec", "home", "--", "true"]).await;
        assert_eq!(denied.code, 105, "旧根签发的 agent 应当失效：{}", denied.stderr);

        // 用新根签发的 agent 凭据正常访问 home
        let new_agent = w.cli_ok("laptop", &["agent", "issue", "--label", "new-bot", "--grant", "term.exec@home", "--grant", "term.exec@office", "--ttl", "4h"]).await["credential"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(w.cli("laptop", &["--agent", &new_agent, "exec", "home", "--", "true"]).await.code, 0);

        // office 恢复上线后，laptop 访问它时自动把根密钥轮换链和新名册推过去，无需重建或重新加入
        w.bring_online("office");
        let out = w.cli("laptop", &["--agent", &new_agent, "exec", "office", "--", "echo", "caught-up"]).await;
        assert_eq!((out.code, out.stdout.trim()), (0, "caught-up"), "{}", out.stderr);
        assert_eq!(w.cli("laptop", &["--agent", &old_agent, "exec", "office", "--", "true"]).await.code, 105);
    },
}

scenario! {
    name: rotate_root_keeps_vault_entries_readable,
    covers: "R75, R74",
    nodes: ["laptop", "home", "office"],
    real: yes,
    run: |w: World| async move {
        network(&w, &["home", "office"]).await;
        w.cli_ok("laptop", &["vault", "put", "api_key", "--value", "sk-before", "--device", "home", "--device", "office"]).await;

        // office 离线时轮换：Vault 条目按「根密钥 × 设备密钥」加密，轮换后要重新加密，不能丢
        w.take_offline("office");
        w.cli_ok("laptop", &["rotate-root"]).await;
        assert_eq!(w.cli_ok("laptop", &["vault", "get", "api_key"]).await["value"], "sk-before");
        // 在线的 home 收到按新根重新加密的条目
        assert_eq!(w.cli_ok("home", &["vault", "get", "api_key"]).await["value"], "sk-before");

        // 错过轮换的 office 上线后，同步一次即可重新读到
        w.bring_online("office");
        assert_eq!(w.cli("laptop", &["exec", "office", "--", "true"]).await.code, 0);
        w.cli_ok("office", &["vault", "sync"]).await;
        assert_eq!(w.cli_ok("office", &["vault", "get", "api_key"]).await["value"], "sk-before");
    },
}
