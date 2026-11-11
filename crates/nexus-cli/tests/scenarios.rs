//! 验收场景。每个场景同时生成仿真后端和真实后端两个测试。
//! 需求编号见仓库根目录的 FEATURES.md。

use nexus_testkit::{scenario, World};
use serde_json::Value;
use std::time::Duration;

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("输出不是合法的 JSON（{e}）：{text:?}"))
}

fn keys(v: &Value) -> Vec<&str> {
    v.as_object().expect("应当是 JSON 对象").keys().map(String::as_str).collect()
}

scenario! {
    name: node_reports_itself,
    covers: "R00, R19, R20",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        // Given 一个刚启动的节点 laptop
        // When 在它上面查询本机状态并要求结构化输出
        let out = w.cli("laptop", &["status", "--json"]).await;
        // Then 命令成功，输出是结构固定的 JSON，包含名称、标识、版本和在线状态
        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        assert_eq!(out.stderr, "");
        let v = json(&out.stdout);
        assert_eq!(
            keys(&v),
            ["name", "node_id", "online", "path", "rtt_ms", "schema", "started_at_ms", "uptime_ms", "version"],
            "--json 的结构发生了变化；如果是有意的，请递增 JSON_SCHEMA"
        );
        assert_eq!(v["schema"], 1);
        assert_eq!(v["name"], "laptop");
        assert_eq!(v["online"], true);
        let id = v["node_id"].as_str().unwrap();
        assert!(id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()), "标识格式不对：{id}");
        assert!(!v["version"].as_str().unwrap().is_empty());
        assert!(v["started_at_ms"].as_u64().unwrap() > 1_700_000_000_000);

        // And 人类可读的输出也包含名称
        let human = w.cli("laptop", &["status"]).await;
        assert_eq!(human.code, 0);
        assert!(human.stdout.contains("laptop") && human.stdout.contains("在线"), "{}", human.stdout);
    },
}

scenario! {
    name: node_identity_is_stable,
    covers: "R00",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        // Given 一个运行中的节点
        // When 连续两次查询本机状态
        let a = json(&w.cli("laptop", &["status", "--json"]).await.stdout);
        let b = json(&w.cli("laptop", &["status", "--json"]).await.stdout);
        // Then 标识和启动时间保持不变
        assert_eq!(a["node_id"], b["node_id"]);
        assert_eq!(a["started_at_ms"], b["started_at_ms"]);
    },
}

scenario! {
    name: unknown_node_is_a_clear_error,
    covers: "R00",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        // Given 节点 laptop 不认识任何叫 nosuch 的节点
        // When 查询 nosuch 的状态
        let out = w.cli("laptop", &["status", "nosuch", "--json"]).await;
        // Then 以固定的退出码失败，标准错误是一行结构化的原因，标准输出为空
        assert_eq!(out.code, 103);
        assert_eq!(out.stdout, "");
        let err = json(&out.stderr);
        assert_eq!(err["error"], "unknown_node");
        assert!(err["message"].as_str().unwrap().contains("nosuch"));
    },
}

scenario! {
    name: bad_usage_is_a_clear_error,
    covers: "R00",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        // When 使用不存在的子命令
        let out = w.cli("laptop", &["no-such-command"]).await;
        // Then 以用法错误的退出码失败，并且没有标准输出
        assert_eq!(out.code, 102);
        assert_eq!(out.stdout, "");
        assert!(!out.stderr.is_empty());
    },
}

scenario! {
    name: node_sees_peer_status,
    covers: "R03, R22",
    nodes: ["laptop", "home"],
    real: (pending "M1.2：真实后端还没有网络传输"),
    run: |w: World| async move {
        // Given 两个互相认识的节点 laptop 和 home
        let me = json(&w.cli("laptop", &["status", "--json"]).await.stdout);
        // When 在 laptop 上查询 home 的状态
        let out = w.cli("laptop", &["status", "home", "--json"]).await;
        // Then 得到 home 的自述，以及连接的解释：在线、路径类型、往返时间
        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        let v = json(&out.stdout);
        assert_eq!(v["name"], "home");
        assert_eq!(v["online"], true);
        assert_eq!(v["path"], "direct");
        assert!(v["rtt_ms"].is_u64(), "应当报告往返时间：{v}");
        assert_ne!(v["node_id"], me["node_id"]);
        assert!(!v["version"].as_str().unwrap().is_empty());

        // And 反方向同样成立
        let back = json(&w.cli("home", &["status", "laptop", "--json"]).await.stdout);
        assert_eq!(back["name"], "laptop");
        assert_eq!(back["node_id"], me["node_id"]);
    },
}

scenario! {
    name: time_passes_without_waiting,
    covers: "R26",
    nodes: ["laptop", "home"],
    real: (pending "只在仿真后端有意义：真实后端需要真的等四小时"),
    run: |w: World| async move {
        // Given 两个节点
        let before = json(&w.cli("laptop", &["status", "home", "--json"]).await.stdout);
        // When 世界真实地运行两秒，然后时钟再向前跳四小时
        w.run_for(Duration::from_secs(2)).await;
        w.advance(Duration::from_secs(4 * 3600)).await;
        // Then 节点仍然可达且没有重启，它自己的时钟显示已经运行了四小时以上
        let after = json(&w.cli("laptop", &["status", "home", "--json"]).await.stdout);
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
            w.cli("laptop", &["status", "home", "--json"]).await;
            w.cli("home", &["status", "laptop", "--json"]).await;
        })
    };
    // 同一个种子：轨迹相同
    assert_eq!(run(7), run(7));
    // 不同的种子：节点标识和网络延迟都不同，轨迹也不同
    let distinct: std::collections::HashSet<u64> = (1..=5).map(run).collect();
    assert_eq!(distinct.len(), 5);
}
