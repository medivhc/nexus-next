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

// ---------------------------------------------------------------- M1.3 结构化执行

const EXEC_KEYS: [&str; 10] = [
    "duration_ms", "exit_code", "schema", "signal", "stderr", "stderr_base64", "stdout", "stdout_base64", "timed_out", "truncated",
];

scenario! {
    name: exec_returns_output_and_exit_code,
    covers: "R09, R19",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let script = "echo out; echo err >&2; exit 3";
        // When 直接执行
        let out = w.cli("laptop", &["exec", "home", "--", "sh", "-c", script]).await;
        // Then 输出分开到达，退出码透传
        assert_eq!(out.code, 3, "stderr: {}", out.stderr);
        assert_eq!(out.stdout, "out\n");
        assert_eq!(out.stderr, "err\n");
        // When 要求 JSON
        let out = w.cli("laptop", &["exec", "home", "--json", "--", "sh", "-c", script]).await;
        assert_eq!(out.code, 3);
        let v = json(&out.stdout);
        assert_eq!(keys(&v), EXEC_KEYS);
        assert_eq!(v["exit_code"], 3);
        assert_eq!(v["stdout"], "out\n");
        assert_eq!(v["stderr"], "err\n");
        assert_eq!(v["timed_out"], false);
        assert_eq!(v["truncated"], false);
        assert_eq!(v["stdout_base64"], Value::Null);
        assert!(v["duration_ms"].is_u64());
        // And 成功的命令退出码为 0
        let ok = w.cli("laptop", &["exec", "home", "--", "echo", "hello", "world"]).await;
        assert_eq!((ok.code, ok.stdout.as_str()), (0, "hello world\n"));
    },
}

scenario! {
    name: exec_streams_stdin,
    covers: "R09",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        // 足够大，必然被拆成很多块传输
        let input: Vec<u8> = (0..300_000u32).map(|i| b'a' + (i % 26) as u8).chain(*b"\nend\n").collect();
        let out = w.cli_with_stdin("laptop", &["exec", "home", "--", "cat"], &input).await;
        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        assert_eq!(out.stdout_raw.len(), input.len());
        assert!(out.stdout_raw == input, "输出与输入不一致");
    },
}

scenario! {
    name: exec_passes_arbitrary_bytes_and_large_output,
    covers: "R09",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        // 不是合法 UTF-8 的字节
        let out = w.cli("laptop", &["exec", "home", "--", "printf", "\\377\\000x"]).await;
        assert_eq!(out.stdout_raw, [0xff, 0x00, b'x']);
        let v = json(&w.cli("laptop", &["exec", "home", "--json", "--", "printf", "\\377\\000x"]).await.stdout);
        assert_eq!(v["stdout"], Value::Null);
        assert_eq!(v["stdout_base64"], "/wB4");
        // 几 MB 的输出完整到达
        let big = w.cli("laptop", &["exec", "home", "--", "head", "-c", "3000000", "/dev/zero"]).await;
        assert_eq!(big.code, 0, "stderr: {}", big.stderr);
        assert_eq!(big.stdout_raw.len(), 3_000_000);
        assert!(big.stdout_raw.iter().all(|b| *b == 0));
    },
}

scenario! {
    name: exec_honours_cwd_and_env,
    covers: "R09",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let out = w.cli("laptop", &["exec", "home", "--cwd", "/tmp", "--env", "FOO=bar", "--", "sh", "-c", "pwd; printenv FOO"]).await;
        assert_eq!(out.code, 0, "stderr: {}", out.stderr);
        assert_eq!(out.stdout, "/tmp\nbar\n");
        let bad = w.cli("laptop", &["exec", "home", "--env", "NOEQUALS", "--", "true"]).await;
        assert_eq!(bad.code, 102);
    },
}

scenario! {
    name: exec_timeout_is_reported,
    covers: "R10",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let out = w.cli("laptop", &["exec", "home", "--timeout", "1", "--json", "--", "sleep", "30"]).await;
        assert_eq!(out.code, 124, "stderr: {}", out.stderr);
        let v = json(&out.stdout);
        assert_eq!(v["timed_out"], true);
        assert_eq!(v["truncated"], false);
        assert_eq!(v["exit_code"], Value::Null);
        let ms = v["duration_ms"].as_u64().unwrap();
        assert!((1000..5000).contains(&ms), "应当在 1 秒左右被终止，实际 {ms} 毫秒");
        // 直接输出时，标准错误说明原因
        let human = w.cli("laptop", &["exec", "home", "--timeout", "0.5", "--", "sleep", "30"]).await;
        assert_eq!(human.code, 124);
        assert_eq!(error_kind(&human.stderr), "timed_out");
    },
}

scenario! {
    name: exec_output_limit_is_reported,
    covers: "R10",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let out = w.cli("laptop", &["exec", "home", "--max-output", "1000", "--", "head", "-c", "100000", "/dev/zero"]).await;
        assert_eq!(out.code, 125, "stderr: {}", out.stderr);
        assert_eq!(out.stdout_raw.len(), 1000, "应当恰好收到上限那么多");
        assert_eq!(error_kind(&out.stderr), "truncated");
        let v = json(&w.cli("laptop", &["exec", "home", "--max-output", "1000", "--json", "--", "head", "-c", "100000", "/dev/zero"]).await.stdout);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["timed_out"], false);
        assert_eq!(v["stdout"].as_str().unwrap().len(), 1000);
    },
}

scenario! {
    name: exec_errors_are_clear,
    covers: "R10",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        // 尚未加入网络
        let out = w.cli("laptop", &["exec", "home", "--", "true"]).await;
        assert_eq!(out.code, 106);
        w.network("laptop", &["home"]).await;
        // 命令不存在
        let out = w.cli("laptop", &["exec", "home", "--", "no-such-program-nexus"]).await;
        assert_eq!(out.code, 127, "stderr: {}", out.stderr);
        assert_eq!(out.stdout, "");
        assert_eq!(error_kind(&out.stderr), "spawn_failed");
        // 节点不在名册里
        assert_eq!(w.cli("laptop", &["exec", "nosuch", "--", "true"]).await.code, 103);
        // 没有给出命令
        assert_eq!(w.cli("laptop", &["exec", "home"]).await.code, 102);
    },
}

scenario! {
    name: exec_on_the_local_node,
    covers: "R09",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
        let out = w.cli("laptop", &["exec", "laptop", "--", "echo", "hi"]).await;
        assert_eq!((out.code, out.stdout.as_str()), (0, "hi\n"), "stderr: {}", out.stderr);
    },
}

scenario! {
    name: exec_stops_when_the_caller_leaves,
    covers: "R10",
    nodes: ["laptop", "home"],
    sim: (pending "要中断一个真实的命令行进程，只在真实后端运行"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let marker = "nexus-caller-left-7f3a";
        // 查找用的模式本身不含 marker 原文，否则会匹配到发出查找的那条命令行。
        let pattern = "nexus-caller-lef[t]-7f3a";
        let running = w.spawn_cli("laptop", &["exec", "home", "--", "sh", "-c", &format!("sleep 60; : {marker}")]);
        // 等命令确实在 home 上跑起来。
        let mut started = false;
        for _ in 0..100 {
            let out = w.cli("home", &["exec", "home", "--", "pgrep", "-f", pattern]).await;
            if out.code == 0 {
                started = true;
                break;
            }
            w.run_for(Duration::from_millis(50)).await;
        }
        assert!(started, "命令没有在 home 上运行起来");

        running.interrupt();
        // 调用方离开后，远端命令连同它派生的进程一起被终止。
        let mut gone = false;
        for _ in 0..100 {
            let out = w.cli("home", &["exec", "home", "--", "pgrep", "-f", pattern]).await;
            if out.code == 1 {
                gone = true;
                break;
            }
            w.run_for(Duration::from_millis(50)).await;
        }
        assert!(gone, "调用方离开 5 秒后命令仍在运行");
    },
}
