//! 发布服务与按路径连接（M3.1）。功能点见 docs/feature-points/M3.1.md。

use nexus_testkit::{scenario, Service, World};
use std::time::Duration;

/// 在管理设备上签发一份 agent 凭据。
async fn issue(w: &World, label: &str, grants: &[&str]) -> String {
    let mut args = vec!["agent", "issue", "--label", label, "--ttl", "4h"];
    for g in grants {
        args.extend(["--grant", g]);
    }
    w.cli_ok("laptop", &args).await["credential"].as_str().expect("应当输出凭据").to_string()
}

async fn services(w: &World, at: &str, node: &str) -> Vec<serde_json::Value> {
    let v = w.cli_ok(at, &["ls", node]).await;
    assert_eq!(v["node"], node, "{v}");
    v["services"].as_array().expect("应当有 services 数组").clone()
}

scenario! {
    name: services_can_be_published_listed_and_withdrawn,
    covers: "R37, R39",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let port = w.serve("home", Service::Echo).await.to_string();
        let v = w.cli_ok("home", &["publish", "web", "--port", &port, "--proto", "http", "--env", "BASE_URL=http://{addr}/v1"]).await;
        assert_eq!(v["path"], "home/web", "{v}");

        let list = services(&w, "laptop", "home").await;
        assert_eq!(list.len(), 1, "{list:?}");
        let web = &list[0];
        assert_eq!(web["name"], "web");
        assert_eq!(web["path"], "home/web");
        assert_eq!(web["dns"], "web.home.internal");
        assert_eq!(web["proto"], "http");
        assert_eq!(web["env"], serde_json::json!({ "BASE_URL": "http://{addr}/v1" }));
        assert!(!web.to_string().contains(&port), "不能暴露发布方的本机端口：{web}");
        let text = w.cli("laptop", &["ls", "home"]).await;
        assert!(text.stdout.contains("web") && text.stdout.contains("http"), "{}", text.stdout);

        // 名字重复、不合法、环境变量写法不对。
        for args in [
            &["publish", "web", "--port", &port][..],
            &["publish", "my web", "--port", &port],
            &["publish", "db", "--port", &port, "--env", "NOEQUALS"],
            &["publish", "db", "--port", &port, "--proto", "gopher"],
        ] {
            let out = w.cli("home", args).await;
            assert_eq!(out.code, 107, "{args:?}: {}", out.stderr);
        }

        w.cli_ok("home", &["unpublish", "web"]).await;
        assert!(services(&w, "laptop", "home").await.is_empty());
        assert_eq!(w.cli("home", &["unpublish", "web"]).await.code, 109);
    },
}

scenario! {
    name: a_published_service_is_reached_by_path,
    covers: "R38",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let port = w.serve("home", Service::Echo).await.to_string();
        w.cli_ok("home", &["publish", "echo", "--port", &port]).await;

        let out = w.cli_with_stdin("laptop", &["connect", "home/echo"], b"hello through nexus").await;
        assert_eq!((out.code, out.stdout.as_str()), (0, "hello through nexus"), "{}", out.stderr);
        // 大一点的数据也完整往返。
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let out = w.cli_with_stdin("laptop", &["connect", "home/echo"], &big).await;
        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(out.stdout_raw == big, "回声的数据不一致（{} 字节）", out.stdout_raw.len());

        // 不存在的服务；发布了但本机服务没有在运行。
        assert_eq!(w.cli("laptop", &["connect", "home/nope"]).await.code, 109);
        let dead = w.unused_port().to_string();
        w.cli_ok("home", &["publish", "dead", "--port", &dead]).await;
        let out = w.cli("laptop", &["connect", "home/dead"]).await;
        assert_eq!(out.code, 108, "{}", out.stderr);
        // 不合法的路径。
        assert_eq!(w.cli("laptop", &["connect", "home"]).await.code, 107);
    },
}

scenario! {
    name: connect_can_listen_on_a_local_port,
    covers: "R38",
    nodes: ["laptop", "home"],
    sim: (pending "本机端口转发要在真实的本机网络上验证"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let port = w.serve("home", Service::Http("hello from home".into())).await.to_string();
        w.cli_ok("home", &["publish", "web", "--port", &port, "--proto", "http"]).await;

        let local = w.unused_port();
        let listen = format!("127.0.0.1:{local}");
        let _fwd = w.spawn_cli("laptop", &["connect", "home/web", "--listen", &listen]);
        let mut body = None;
        for _ in 0..100 {
            if let Ok(b) = nexus_testkit::http_get(local, "/") {
                body = Some(b);
                break;
            }
            w.run_for(Duration::from_millis(50)).await;
        }
        assert_eq!(body.as_deref(), Some("hello from home"));
        // 每个本机连接各自一条，互不影响。
        for _ in 0..3 {
            assert_eq!(nexus_testkit::http_get(local, "/").as_deref().ok(), Some("hello from home"));
        }
    },
}

scenario! {
    name: services_need_a_grant_for_agents_and_are_audited,
    covers: "R37, R38, R39",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let port = w.serve("home", Service::Echo).await.to_string();
        w.cli_ok("home", &["publish", "echo", "--port", &port]).await;
        w.cli_ok("home", &["publish", "other", "--port", &port]).await;

        let viewer = issue(&w, "viewer", &["node.info@**"]).await;
        let out = w.cli_with_stdin("laptop", &["--agent", &viewer, "connect", "home/echo"], b"x").await;
        assert_eq!(out.code, 105, "{}", out.stderr);
        let v = w.cli("laptop", &["--agent", &viewer, "ls", "home", "--json"]).await;
        let v: serde_json::Value = serde_json::from_str(&v.stdout).expect("JSON");
        assert_eq!(v["services"], serde_json::json!([]), "{v}");

        let user = issue(&w, "user", &["node.info@**", "svc.connect@home/svc/echo"]).await;
        let out = w.cli_with_stdin("laptop", &["--agent", &user, "connect", "home/echo"], b"granted").await;
        assert_eq!((out.code, out.stdout.as_str()), (0, "granted"), "{}", out.stderr);
        let v = w.cli("laptop", &["--agent", &user, "ls", "home", "--json"]).await;
        let v: serde_json::Value = serde_json::from_str(&v.stdout).expect("JSON");
        let names: Vec<_> = v["services"].as_array().unwrap().iter().map(|s| s["name"].clone()).collect();
        assert_eq!(names, [serde_json::json!("echo")], "{v}");
        assert_eq!(w.cli_with_stdin("laptop", &["--agent", &user, "connect", "home/other"], b"x").await.code, 105);

        let audit = w.cli_ok("home", &["audit"]).await;
        let events = audit["events"].as_array().unwrap();
        assert!(
            events.iter().any(|e| e["capability"] == "svc.connect" && e["resource"] == "home/svc/echo" && e["outcome"] == "allow"
                && e["principal"]["label"] == "user"),
            "{audit}"
        );
    },
}
