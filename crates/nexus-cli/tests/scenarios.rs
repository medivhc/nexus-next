//! 验收场景。每个场景同时生成仿真后端和真实后端两个测试。
//! 需求编号见仓库根目录的 FEATURES.md，功能点见 docs/feature-points/。

use nexus_testkit::{scenario, World};
use serde_json::Value;
use std::time::Duration;

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("输出不是合法的 JSON（{e}）：{text:?}"))
}

fn keys(v: &Value) -> Vec<&str> {
    v.as_object().expect("应当是 JSON 对象").keys().map(String::as_str).collect()
}

/// `nexus ls --json` 里的设备，按名字排序，便于比较。
fn devices(v: &Value) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = v["devices"]
        .as_array()
        .expect("ls 应当输出 devices 数组")
        .iter()
        .map(|d| (d["name"].as_str().unwrap().to_string(), d["online"].as_bool().unwrap()))
        .collect();
    out.sort();
    out
}

fn error_kind(stderr: &str) -> String {
    json(stderr)["error"].as_str().unwrap_or_default().to_string()
}

const STATUS_KEYS: [&str; 11] = [
    "name", "node_id", "online", "path", "role", "roster_version", "rtt_ms", "schema", "started_at_ms", "uptime_ms", "version",
];

scenario! {
    name: node_reports_itself,
    covers: "R00, R19, R20",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        // Given 一个刚启动、尚未加入网络的节点 laptop
        // When 查询本机状态并要求结构化输出
        let out = w.cli("laptop", &["status", "--json"]).await;
        // Then 命令成功，输出是结构固定的 JSON
        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        assert_eq!(out.stderr, "");
        let v = json(&out.stdout);
        assert_eq!(keys(&v), STATUS_KEYS, "--json 的结构发生了变化；如果是有意的，请递增 JSON_SCHEMA");
        assert_eq!(v["schema"], 2);
        assert_eq!(v["name"], "laptop");
        assert_eq!(v["online"], true);
        assert_eq!(v["role"], Value::Null);
        assert_eq!(v["roster_version"], Value::Null);
        let id = v["node_id"].as_str().unwrap();
        assert!(id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()), "标识格式不对：{id}");
        assert!(!v["version"].as_str().unwrap().is_empty());
        assert!(v["started_at_ms"].as_u64().unwrap() > 1_700_000_000_000);

        // And 人类可读的输出也包含名称和状态
        let human = w.cli("laptop", &["status"]).await;
        assert_eq!(human.code, 0);
        assert!(human.stdout.contains("laptop") && human.stdout.contains("尚未加入网络"), "{}", human.stdout);
    },
}

scenario! {
    name: node_identity_is_stable,
    covers: "R00",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        let a = w.cli_ok("laptop", &["status"]).await;
        w.cli_ok("laptop", &["init"]).await;
        let b = w.cli_ok("laptop", &["status"]).await;
        // 标识和启动时间不因查询或建立网络而改变
        assert_eq!(a["node_id"], b["node_id"]);
        assert_eq!(a["started_at_ms"], b["started_at_ms"]);
    },
}

scenario! {
    name: bad_usage_is_a_clear_error,
    covers: "R00",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        let out = w.cli("laptop", &["no-such-command"]).await;
        assert_eq!(out.code, 102);
        assert_eq!(out.stdout, "");
        assert!(!out.stderr.is_empty());
    },
}

scenario! {
    name: init_creates_a_network,
    covers: "R01",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        // When 在第一台设备上建立网络
        let init = w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
        // Then 得到根身份和一次性的恢复码
        assert_eq!(keys(&init), ["name", "node_id", "recovery_code", "root", "schema"]);
        let rc = init["recovery_code"].as_str().unwrap();
        assert!(rc.len() == 64 && rc.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(init["root"], init["node_id"], "根身份和设备身份必须是两把不同的密钥");
        // And 它是管理设备，名册里只有它自己
        let me = w.cli_ok("laptop", &["status"]).await;
        assert_eq!(me["role"], "admin");
        assert_eq!(me["roster_version"], 1);
        assert_eq!(devices(&w.cli_ok("laptop", &["ls"]).await), [("laptop".into(), true)]);
        // And 不能再建一次
        let again = w.cli("laptop", &["init", "--json"]).await;
        assert_eq!(again.code, 106);
        assert_eq!(error_kind(&again.stderr), "invalid_state");
    },
}

scenario! {
    name: approved_device_joins,
    covers: "R02, R03",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        // Given 管理设备 laptop，When home 加入
        w.network("laptop", &["home"]).await;
        // Then 双方都能看到对方在线
        let both = [("home".to_string(), true), ("laptop".to_string(), true)];
        assert_eq!(devices(&w.cli_ok("laptop", &["ls"]).await), both);
        assert_eq!(devices(&w.cli_ok("home", &["ls"]).await), both);
        // And home 是成员，名册版本已前进
        let home = w.cli_ok("home", &["status"]).await;
        assert_eq!(home["role"], "member");
        assert_eq!(home["roster_version"], 2);
        // And 标识与 laptop 看到的一致
        let seen = w.cli_ok("laptop", &["status", "home"]).await;
        assert_eq!(seen["node_id"], home["node_id"]);
    },
}

scenario! {
    name: only_the_latest_join_code_works,
    covers: "R02",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
        // Given home 先后生成了两个加入码
        let a = w.cli_ok("home", &["join"]).await["code"].as_str().unwrap().to_string();
        let b = w.cli_ok("home", &["join"]).await["code"].as_str().unwrap().to_string();
        assert_ne!(a, b);
        // When 用旧的那个批准，Then 被 home 拒绝，名册不变
        let old = w.cli("laptop", &["device", "approve", &a, "--name", "home", "--json"]).await;
        assert_eq!(old.code, 105, "stderr: {}", old.stderr);
        assert_eq!(devices(&w.cli_ok("laptop", &["ls"]).await), [("laptop".into(), true)]);
        assert_eq!(w.cli_ok("home", &["status"]).await["roster_version"], Value::Null);
        // And 格式不对的加入码直接报输入错误
        let junk = w.cli("laptop", &["device", "approve", "nxj1-0000", "--json"]).await;
        assert_eq!(junk.code, 107);
        // When 用新的那个批准，Then 成功
        let ok = w.cli_ok("laptop", &["device", "approve", &b, "--name", "home"]).await;
        assert_eq!(ok["name"], "home");
        assert_eq!(ok["roster_version"], 2);
        // And 同一个加入码不能再用一次
        let reuse = w.cli("laptop", &["device", "approve", &b, "--name", "home2", "--json"]).await;
        assert_eq!(reuse.code, 106);
    },
}

scenario! {
    name: only_the_admin_can_approve,
    covers: "R02",
    nodes: ["laptop", "home", "office"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let code = w.cli_ok("office", &["join"]).await["code"].as_str().unwrap().to_string();
        // When 成员设备 home 尝试批准 office
        let out = w.cli("home", &["device", "approve", &code, "--json"]).await;
        // Then 失败，office 没有加入
        assert_eq!(out.code, 106);
        assert!(json(&out.stderr)["message"].as_str().unwrap().contains("管理设备"));
        assert_eq!(w.cli_ok("office", &["status"]).await["roster_version"], Value::Null);
    },
}

scenario! {
    name: node_sees_peer_status,
    covers: "R03, R22",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let me = w.cli_ok("laptop", &["status"]).await;
        // When 在 laptop 上查询 home 的状态
        let v = w.cli_ok("laptop", &["status", "home"]).await;
        // Then 得到 home 的自述，以及连接的解释：在线、路径类型、往返时间
        assert_eq!(keys(&v), STATUS_KEYS);
        assert_eq!(v["name"], "home");
        assert_eq!(v["online"], true);
        assert_eq!(v["path"], "direct");
        assert!(v["rtt_ms"].is_u64(), "应当报告往返时间：{v}");
        assert_ne!(v["node_id"], me["node_id"]);
        // And 反方向同样成立，也可以用标识代替名字
        let id = me["node_id"].as_str().unwrap();
        let back = w.cli_ok("home", &["status", id]).await;
        assert_eq!(back["name"], "laptop");
        assert_eq!(back["role"], "admin");
        // And 查询名册之外的名字是明确的错误
        let out = w.cli("laptop", &["status", "nosuch", "--json"]).await;
        assert_eq!(out.code, 103);
        assert_eq!(out.stdout, "");
        assert!(json(&out.stderr)["message"].as_str().unwrap().contains("nosuch"));
    },
}

scenario! {
    name: offline_device_shows_as_offline,
    covers: "R03, R22",
    nodes: ["laptop", "home", "office"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home", "office"]).await;
        // Given office 掉线
        w.take_offline("office");
        // When 在 laptop 上列出设备
        let ls = w.cli_ok("laptop", &["ls"]).await;
        // Then office 显示离线，其余正常
        assert_eq!(devices(&ls), [("home".into(), true), ("laptop".into(), true), ("office".into(), false)]);
        let office = w.cli_ok("laptop", &["status", "office"]).await;
        assert_eq!(office["online"], false);
        assert_eq!(office["path"], Value::Null);
        assert_eq!(office["role"], "member");
    },
}

scenario! {
    name: revoked_device_loses_access,
    covers: "R18",
    nodes: ["laptop", "home", "office"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home", "office"]).await;
        // When 管理设备把 office 移出名册
        let r = w.cli_ok("laptop", &["device", "revoke", "office"]).await;
        assert_eq!(r["pushed"], 1, "新名册应当送达 home：{r}");
        assert_eq!(r["unreachable"], serde_json::json!([]));
        // Then office 访问任何设备都被拒绝
        for target in ["home", "laptop"] {
            let out = w.cli("office", &["status", target, "--json"]).await;
            assert_eq!(out.code, 105, "office 访问 {target} 应当被拒绝：{}", out.stderr);
            assert_eq!(error_kind(&out.stderr), "denied");
        }
        // And 其他设备的列表里不再有 office
        let rest = [("home".to_string(), true), ("laptop".to_string(), true)];
        assert_eq!(devices(&w.cli_ok("laptop", &["ls"]).await), rest);
        assert_eq!(devices(&w.cli_ok("home", &["ls"]).await), rest);
        assert_eq!(w.cli("laptop", &["status", "office", "--json"]).await.code, 103);
        // And 管理设备不能移除自己
        assert_eq!(w.cli("laptop", &["device", "revoke", "laptop", "--json"]).await.code, 107);
    },
}

scenario! {
    name: missed_roster_update_catches_up,
    covers: "R02, R18",
    nodes: ["laptop", "home", "office"],
    real: (pending "需要让掉线的节点原样恢复，只有仿真后端能做到"),
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        // Given home 掉线期间，管理设备批准了 office
        w.take_offline("home");
        let code = w.cli_ok("office", &["join"]).await["code"].as_str().unwrap().to_string();
        let r = w.cli_ok("laptop", &["device", "approve", &code, "--name", "office"]).await;
        assert_eq!(r["unreachable"], serde_json::json!(["home"]));
        // When home 重新上线，office 第一次访问它
        w.bring_online("home");
        let v = w.cli_ok("office", &["status", "home"]).await;
        // Then 访问成功，home 也已经知道 office
        assert_eq!(v["online"], true);
        assert_eq!(w.cli_ok("home", &["status"]).await["roster_version"], 3);
        let names: Vec<String> = devices(&w.cli_ok("home", &["ls"]).await).into_iter().map(|d| d.0).collect();
        assert_eq!(names, ["home", "laptop", "office"]);
    },
}

scenario! {
    name: time_passes_without_waiting,
    covers: "R26",
    nodes: ["laptop", "home"],
    real: (pending "只在仿真后端有意义：真实后端需要真的等四小时"),
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let before = w.cli_ok("laptop", &["status", "home"]).await;
        // When 世界真实地运行两秒，然后时钟再向前跳四小时
        w.run_for(Duration::from_secs(2)).await;
        w.advance(Duration::from_secs(4 * 3600)).await;
        // Then 节点仍然可达且没有重启，它自己的时钟显示已经运行了四小时以上
        let after = w.cli_ok("laptop", &["status", "home"]).await;
        assert_eq!(after["online"], true);
        assert_eq!(before["started_at_ms"], after["started_at_ms"]);
        let gained = after["uptime_ms"].as_u64().unwrap() - before["uptime_ms"].as_u64().unwrap();
        assert!(gained >= 4 * 3600 * 1000 + 2000, "时钟只前进了 {gained} 毫秒");
    },
}

// covers: R26
#[test]
fn sim_is_deterministic_per_seed_and_varies_across_seeds() {
    let run = |seed| {
        nexus_testkit::sim_once(&["laptop", "home"], seed, &|w: World| async move {
            w.network("laptop", &["home"]).await;
            w.cli("laptop", &["status", "home", "--json"]).await;
            w.cli("home", &["status", "laptop", "--json"]).await;
        })
    };
    // 同一个种子：轨迹相同
    assert_eq!(run(7), run(7));
    // 不同的种子：密钥、加入码和网络延迟都不同，轨迹也不同
    let distinct: std::collections::HashSet<u64> = (1..=5).map(run).collect();
    assert_eq!(distinct.len(), 5);
}
