//! Vault（M9.2，R74）：用户秘密加密后同步到被授权的设备，按条目授权，每次取用有记录；
//! 吊销设备时列出要轮换的条目。功能点见 docs/feature-points/M9.2.md。

use nexus_testkit::{scenario, World};

/// 在管理设备上签发一份 agent 凭据。
async fn issue(w: &World, label: &str, grants: &[&str]) -> String {
    let mut args = vec!["agent", "issue", "--label", label, "--ttl", "4h"];
    for g in grants {
        args.extend(["--grant", g]);
    }
    w.cli_ok("laptop", &args).await["credential"].as_str().expect("应当输出凭据").to_string()
}

scenario! {
    name: vault_encrypts_and_syncs_only_to_authorized_devices,
    covers: "R74",
    nodes: ["laptop", "home", "office"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home", "office"]).await;

        // 1. 只授权给 home 的条目 api_key：自动同步到 home，office 拿不到。
        let put1 = w.cli_ok("laptop", &["vault", "put", "api_key", "--value", "sk-secret-999", "--device", "home"]).await;
        assert_eq!(put1["name"], "api_key", "{put1}");
        assert_eq!(put1["version"], 1, "{put1}");
        assert_eq!(put1["devices"], serde_json::json!(["home", "laptop"]), "{put1}");

        // home 能直接列出和读取解密后的明文
        let ls_home = w.cli_ok("home", &["vault", "ls"]).await;
        assert_eq!(ls_home["entries"].as_array().unwrap().len(), 1, "{ls_home}");
        let get_home = w.cli_ok("home", &["vault", "get", "api_key"]).await;
        assert_eq!(get_home["value"], "sk-secret-999", "{get_home}");

        // office 不在授权设备列表里：看不见也读不到，即使主动 sync 也不会拿到
        w.cli_ok("office", &["vault", "sync"]).await;
        let ls_office = w.cli_ok("office", &["vault", "ls"]).await;
        assert!(ls_office["entries"].as_array().unwrap().is_empty(), "{ls_office}");
        assert_eq!(w.cli("office", &["vault", "get", "api_key"]).await.code, 109);

        // 2. 更新授权把 home 移除、只留 laptop：home 本地副本被立即擦除
        w.cli_ok("laptop", &["vault", "put", "api_key", "--value", "sk-rotated-111"]).await;
        assert_eq!(w.cli("home", &["vault", "get", "api_key"]).await.code, 109);
        assert_eq!(w.cli_ok("laptop", &["vault", "get", "api_key"]).await["value"], "sk-rotated-111");
    },
}

scenario! {
    name: vault_enforces_per_entry_agent_grants_and_audits_every_read,
    covers: "R74, R12, R15, R17",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        w.cli_ok("laptop", &["vault", "put", "openai", "--value", "sk-openai", "--device", "home"]).await;
        w.cli_ok("laptop", &["vault", "put", "db_pass", "--value", "pg-secret", "--device", "home"]).await;

        // 只授权读取 home 上的 openai，不授权 db_pass
        let bot = issue(&w, "bot", &["vault.read@home/vault/openai"]).await;

        // 1. R15 未授权即不可见：ls 只列出 openai
        let ls = w.cli("home", &["--agent", &bot, "vault", "ls", "--json"]).await;
        assert_eq!(ls.code, 0, "{}", ls.stderr);
        let ls_v: serde_json::Value = serde_json::from_str(&ls.stdout).unwrap();
        let names: Vec<&str> = ls_v["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["openai"], "{ls_v}");

        // 2. 读取被授权的 openai 成功，读取未授权的 db_pass 被拒绝（105）
        let ok = w.cli("home", &["--agent", &bot, "vault", "get", "openai", "--json"]).await;
        assert_eq!(ok.code, 0, "{}", ok.stderr);
        let ok_v: serde_json::Value = serde_json::from_str(&ok.stdout).unwrap();
        assert_eq!(ok_v["value"], "sk-openai");

        let deny = w.cli("home", &["--agent", &bot, "vault", "get", "db_pass", "--json"]).await;
        assert_eq!(deny.code, 105, "{}", deny.stderr);

        // 3. 每次取用（包括允许与拒绝）都记入审计日志
        let audit = w.cli_ok("home", &["audit"]).await;
        let events = audit["events"].as_array().unwrap();
        assert!(
            events.iter().any(|e| e["capability"] == "vault.read"
                && e["resource"] == "home/vault/openai"
                && e["outcome"] == "allow"
                && e["principal"]["label"] == "bot"),
            "{audit}"
        );
        assert!(
            events.iter().any(|e| e["capability"] == "vault.read"
                && e["resource"] == "home/vault/db_pass"
                && e["outcome"] == "deny"
                && e["principal"]["label"] == "bot"),
            "{audit}"
        );
    },
}

scenario! {
    name: revoking_device_lists_vault_entries_needing_rotation,
    covers: "R74, R18",
    nodes: ["laptop", "home", "office"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home", "office"]).await;
        w.cli_ok("laptop", &["vault", "put", "aws_key", "--value", "AKIA-1", "--device", "home"]).await;
        w.cli_ok("laptop", &["vault", "put", "github_pat", "--value", "ghp-1", "--device", "home", "--device", "office"]).await;
        w.cli_ok("laptop", &["vault", "put", "office_only", "--value", "off-1", "--device", "office"]).await;

        // 吊销设备 home：应当直接列出 home 接触过的 aws_key 和 github_pat（不含 office_only）
        let text = w.cli("laptop", &["device", "revoke", "home"]).await;
        assert_eq!(text.code, 0, "{}", text.stderr);
        assert!(text.stdout.contains("aws_key") && text.stdout.contains("github_pat"), "吊销输出应提示需轮换条目：{}", text.stdout);

        let rot = w.cli_ok("laptop", &["vault", "rotate-needed"]).await;
        let items = rot["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{rot}");
        assert_eq!(items[0]["revoked_device"], "home");
        assert_eq!(items[0]["entries"], serde_json::json!(["aws_key", "github_pat"]));

        // 重新 put 轮换 aws_key 后，待轮换清单中只剩 github_pat
        w.cli_ok("laptop", &["vault", "put", "aws_key", "--value", "AKIA-2"]).await;
        let rot2 = w.cli_ok("laptop", &["vault", "rotate-needed"]).await;
        assert_eq!(rot2["items"][0]["entries"], serde_json::json!(["github_pat"]), "{rot2}");

        // 删除 github_pat 后，待轮换清单清空
        w.cli_ok("laptop", &["vault", "rm", "github_pat"]).await;
        let rot3 = w.cli_ok("laptop", &["vault", "rotate-needed"]).await;
        assert!(rot3["items"].as_array().unwrap().is_empty(), "{rot3}");
    },
}

/// 建网并返回恢复码：laptop 是管理设备，members 依次加入。
async fn network_with_code(w: &World, members: &[&str]) -> String {
    let init = w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
    let code = init["recovery_code"].as_str().unwrap().to_string();
    for m in members {
        let join = w.cli_ok(m, &["join"]).await;
        w.cli_ok("laptop", &["device", "approve", join["code"].as_str().unwrap(), "--name", m]).await;
    }
    code
}

fn entry_names(ls: &serde_json::Value) -> Vec<String> {
    ls["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect()
}

scenario! {
    name: recovered_admin_collects_vault_entries_from_members,
    covers: "R74, R64",
    nodes: ["laptop", "home", "office"],
    real: yes,
    run: |w: World| async move {
        let code = network_with_code(&w, &["home", "office"]).await;
        w.cli_ok("laptop", &["vault", "put", "shared", "--value", "v-shared", "--device", "home", "--device", "office"]).await;
        w.cli_ok("laptop", &["vault", "put", "office_only", "--value", "v-office", "--device", "office"]).await;

        // 管理设备丢了，home 用恢复码接管：恢复时从在线的设备收回条目。
        w.take_offline("laptop");
        w.cli_ok("home", &["recover", &code]).await;
        let ls = w.cli_ok("home", &["vault", "ls"]).await;
        assert_eq!(entry_names(&ls), ["office_only", "shared"], "{ls}");
        assert_eq!(w.cli_ok("home", &["vault", "get", "office_only"]).await["value"], "v-office");

        // office 照常同步，不丢任何条目。
        w.cli_ok("office", &["vault", "sync"]).await;
        assert_eq!(w.cli_ok("office", &["vault", "get", "office_only"]).await["value"], "v-office");
        assert_eq!(w.cli_ok("office", &["vault", "get", "shared"]).await["value"], "v-shared");

        // 新的管理设备照常轮换和删除。
        w.cli_ok("home", &["vault", "put", "shared", "--value", "v-shared-2", "--device", "office"]).await;
        assert_eq!(w.cli_ok("office", &["vault", "get", "shared"]).await["value"], "v-shared-2");
        w.cli_ok("home", &["vault", "rm", "office_only"]).await;
        assert_eq!(w.cli("office", &["vault", "get", "office_only"]).await.code, 109);
    },
}

scenario! {
    name: member_keeps_vault_entries_the_recovered_admin_has_not_seen,
    covers: "R74, R64",
    nodes: ["laptop", "home", "office"],
    real: (pending "需要让掉线的节点原样恢复，只有仿真后端能做到"),
    run: |w: World| async move {
        let code = network_with_code(&w, &["home", "office"]).await;
        w.cli_ok("laptop", &["vault", "put", "shared", "--value", "v-shared", "--device", "home", "--device", "office"]).await;
        w.cli_ok("laptop", &["vault", "put", "office_only", "--value", "v-office", "--device", "office"]).await;

        // office 恢复时不在线：新的管理设备收不回它的条目。
        w.take_offline("laptop");
        w.take_offline("office");
        w.cli_ok("home", &["recover", &code]).await;
        assert_eq!(entry_names(&w.cli_ok("home", &["vault", "ls"]).await), ["shared"]);

        // office 回来后同步：新管理设备的清单短，office 不能因此丢掉 office_only。
        w.bring_online("office");
        w.cli_ok("office", &["status", "home"]).await;
        w.cli_ok("office", &["vault", "sync"]).await;
        assert_eq!(w.cli_ok("office", &["vault", "get", "office_only"]).await["value"], "v-office");

        // 管理设备上 vault sync 从在线的设备收回条目。
        w.cli_ok("home", &["vault", "sync"]).await;
        assert_eq!(entry_names(&w.cli_ok("home", &["vault", "ls"]).await), ["office_only", "shared"]);

        // 之后新管理设备的删除照常生效，包括 office 离线时删的。
        w.cli_ok("office", &["vault", "sync"]).await;
        w.take_offline("office");
        w.cli_ok("home", &["vault", "rm", "office_only"]).await;
        w.bring_online("office");
        w.cli_ok("office", &["status", "home"]).await;
        w.cli_ok("office", &["vault", "sync"]).await;
        assert_eq!(w.cli("office", &["vault", "get", "office_only"]).await.code, 109);
        assert_eq!(w.cli_ok("office", &["vault", "get", "shared"]).await["value"], "v-shared");
    },
}
