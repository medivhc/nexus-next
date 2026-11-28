//! 一键体检命令 `nexus doctor` 的验收场景（R94）。功能点见 docs/feature-points/M11.5.md。

use nexus_testkit::{scenario, World};
use serde_json::Value;

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("输出不是合法的 JSON（{e}）：{text:?}"))
}

fn find_check<'a>(checks: &'a [Value], name: &str) -> &'a Value {
    checks.iter().find(|c| c["name"] == name).unwrap_or_else(|| panic!("缺少检查项 {name}：{checks:?}"))
}

scenario! {
    name: doctor_reports_ok_when_network_is_healthy,
    covers: "R94, R19",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;

        let out = w.cli("laptop", &["doctor", "--json"]).await;
        assert_eq!(out.code, 0, "stderr: {}\nstdout: {}", out.stderr, out.stdout);
        assert_eq!(out.stderr, "");

        let v = json(&out.stdout);
        assert_eq!(v["schema"], 2, "{v}");
        assert_eq!(v["overall"], "ok", "{v}");

        let checks = v["checks"].as_array().expect("checks 应当是数组");
        for name in ["daemon", "device:home", "path:home", "version", "name_resolution"] {
            let item = find_check(checks, name);
            assert_eq!(item["status"], "ok", "{item}");
            assert!(!item["detail"].as_str().unwrap_or("").is_empty(), "{item}");
            assert!(item["suggestion"].is_null(), "{item}");
        }

        let human = w.cli("laptop", &["doctor"]).await;
        assert_eq!(human.code, 0, "stderr: {}\nstdout: {}", human.stderr, human.stdout);
        assert_eq!(human.stderr, "");
        for needle in ["daemon", "device:home", "path:home", "version", "name_resolution", "汇总"] {
            assert!(human.stdout.contains(needle), "人读输出缺少 {needle}：\n{}", human.stdout);
        }
    },
}

scenario! {
    name: doctor_reports_fail_when_peer_is_offline,
    covers: "R94, R19",
    nodes: ["laptop", "home", "office"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home", "office"]).await;
        w.take_offline("office");

        let out = w.cli("laptop", &["doctor", "--json"]).await;
        assert_eq!(out.code, 1, "存在失败项时退出码应当非 0，stdout: {}\nstderr: {}", out.stdout, out.stderr);
        assert_eq!(out.stderr, "");

        let v = json(&out.stdout);
        assert_eq!(v["schema"], 2, "{v}");
        assert_eq!(v["overall"], "fail", "{v}");

        let checks = v["checks"].as_array().expect("checks 应当是数组");
        for item in checks {
            let obj = item.as_object().expect("每项检查应当是对象");
            for key in ["name", "status", "detail", "suggestion"] {
                assert!(obj.contains_key(key), "检查项缺少字段 {key}：{item}");
            }
        }

        let office = find_check(checks, "device:office");
        assert_eq!(office["status"], "fail", "{office}");
        assert!(!office["detail"].as_str().unwrap_or("").is_empty(), "{office}");
        assert!(!office["suggestion"].as_str().unwrap_or("").is_empty(), "失败项应当给出建议：{office}");

        assert_eq!(find_check(checks, "device:home")["status"], "ok");
        assert_eq!(find_check(checks, "path:home")["status"], "ok");
        assert!(checks.iter().all(|c| c["name"] != "path:office"), "离线设备不应生成连接路径检查：{checks:?}");

        let human = w.cli("laptop", &["doctor"]).await;
        assert_eq!(human.code, 1, "人读模式退出码应当非 0，stdout: {}\nstderr: {}", human.stdout, human.stderr);
        assert!(human.stdout.contains("device:office"), "{}", human.stdout);
        assert!(human.stdout.contains("FAIL") || human.stdout.contains("fail"), "{}", human.stdout);
        assert!(human.stdout.contains("建议"), "{}", human.stdout);
        assert!(human.stdout.contains("汇总"), "{}", human.stdout);
    },
}
