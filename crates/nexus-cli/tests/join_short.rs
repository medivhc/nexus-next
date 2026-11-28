//! 短加入码的验收场景（M6.1，R62、R63）。功能点见 docs/feature-points/M6.1.md。
//!
//! 仿真里碰头服务是一台虚拟主机；真实后端由测试工具在本机起一个碰头服务，所有常驻程序用 `--rendezvous` 指向它。

use nexus_testkit::{scenario, World};
use serde_json::Value;
use std::time::Duration;

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("输出不是合法的 JSON（{e}）：{text:?}"))
}

fn names(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

async fn short_code(w: &World, node: &str) -> String {
    let v = w.cli_ok(node, &["join"]).await;
    let code = v["short_code"]
        .as_str()
        .unwrap_or_else(|| panic!("join 应当给出短码：{v}"))
        .to_string();
    assert!(
        v["code"].as_str().unwrap().starts_with("nxj2-"),
        "长码：{v}"
    );
    code
}

scenario! {
    name: approve_with_short_code,
    covers: "R62",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
        // FP-6.1-01：short_code 是两组 5 个 Crockford 字符
        let code = short_code(&w, "home").await;
        let (a, b) = code.split_once('-').expect("两组");
        assert_eq!((a.len(), b.len()), (5, 5), "{code}");
        assert!(code.replace('-', "").bytes().all(|c| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&c)), "{code}");
        // FP-6.1-02：小写、去掉分隔符也行
        let typed = code.to_lowercase().replace('-', " ");
        let ok = w.cli_ok("laptop", &["device", "approve", &typed, "--name", "home"]).await;
        assert_eq!(ok["name"], "home");
        assert_eq!(names(&w.cli_ok("laptop", &["ls"]).await), ["home", "laptop"]);
        assert_eq!(w.cli_ok("home", &["status"]).await["roster_version"], 2);
        // FP-6.1-03：用过的短码不能再用
        let again = w.cli("laptop", &["device", "approve", &code, "--name", "home2", "--json"]).await;
        assert_eq!(again.code, 109, "stderr: {}", again.stderr);
        assert!(json(&again.stderr)["message"].as_str().unwrap().contains("短加入码"), "{}", again.stderr);
    },
}

scenario! {
    name: only_the_latest_short_code_works,
    covers: "R62",
    nodes: ["laptop", "office"],
    real: yes,
    run: |w: World| async move {
        w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
        // FP-6.1-03：重新 join 之后旧短码失效
        let first = short_code(&w, "office").await;
        let second = short_code(&w, "office").await;
        assert_ne!(first, second);
        let old = w.cli("laptop", &["device", "approve", &first, "--json"]).await;
        assert_eq!(old.code, 109, "stderr: {}", old.stderr);
        // FP-6.1-02：格式不对
        let junk = w.cli("laptop", &["device", "approve", "7KQ4M-9XZ2", "--json"]).await;
        assert_eq!(junk.code, 107);
        // FP-6.1-03：格式对、但从没有人用过的短码（输错了）：找不到，不是连不上
        let typo = w.cli("laptop", &["device", "approve", "7KQ4M-9XZ21", "--json"]).await;
        assert_eq!(typo.code, 109, "stderr: {}", typo.stderr);
        let ok = w.cli_ok("laptop", &["device", "approve", &second]).await;
        assert_eq!(ok["name"], "office");
    },
}

scenario! {
    name: short_code_expires,
    covers: "R62",
    nodes: ["laptop", "home"],
    real: (pending "要推进时间：只在仿真里测"),
    run: |w: World| async move {
        w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
        let code = short_code(&w, "home").await;
        // FP-6.1-03：过了 10 分钟，新设备拒绝
        w.advance(Duration::from_secs(11 * 60)).await;
        let late = w.cli("laptop", &["device", "approve", &code, "--json"]).await;
        assert_eq!(late.code, 106, "stderr: {}", late.stderr);
        assert!(json(&late.stderr)["message"].as_str().unwrap().contains("过期"), "{}", late.stderr);
    },
}

scenario! {
    name: long_code_without_rendezvous,
    covers: "R62, R63",
    nodes: ["laptop", "home"],
    sim: (pending "仿真里碰头服务总在：在真实后端用 --rendezvous disabled 测"),
    real: yes,
    run: |w: World| async move {
        w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
        // FP-6.1-05：没有碰头服务
        w.restart("home", &["--local-only", "--rendezvous", "disabled"]).await;
        let v = w.cli_ok("home", &["join"]).await;
        assert_eq!(v["short_code"], Value::Null, "{v}");
        assert!(v["short_code_error"].as_str().unwrap().contains("碰头服务"), "{v}");
        let text = w.cli("home", &["join"]).await;
        assert!(text.stdout.contains("只能用长加入码"), "{}", text.stdout);
        let code = w.cli_ok("home", &["join"]).await["code"].as_str().unwrap().to_string();
        // FP-6.1-06：长码只有小写字母和数字
        assert!(code.starts_with("nxj2-") && code[5..].bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()), "{code}");
        let ok = w.cli_ok("laptop", &["device", "approve", &code, "--name", "home"]).await;
        assert_eq!(ok["name"], "home");
    },
}

scenario! {
    name: anchor_hosts_the_rendezvous,
    covers: "R62",
    nodes: ["laptop", "anchor", "home"],
    sim: (pending "锚点是真实的 HTTP 服务，只在真实后端运行"),
    real: yes,
    run: |w: World| async move {
        // FP-6.1-07：自建网络里，新设备和管理设备都用锚点上的碰头服务
        let page = w.home_dir("anchor").join("page");
        std::fs::create_dir_all(&page).unwrap();
        std::fs::write(page.join("index.html"), "<!doctype html>").unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let listen = format!("127.0.0.1:{port}");
        w.restart("anchor", &["--local-only", "--anchor-dev", &listen, "--page-dir", page.to_str().unwrap(), "--rendezvous", "disabled"]).await;
        let url = format!("http://localhost:{port}/pkarr");
        w.restart("laptop", &["--local-only", "--rendezvous", &url]).await;
        w.restart("home", &["--local-only", "--rendezvous", &url]).await;
        w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
        let code = short_code(&w, "home").await;
        let ok = w.cli_ok("laptop", &["device", "approve", &code]).await;
        assert_eq!(ok["name"], "home");
        // 锚点上的记录用过之后清掉了
        let again = w.cli("laptop", &["device", "approve", &code, "--json"]).await;
        assert_eq!(again.code, 109, "stderr: {}", again.stderr);
    },
}
