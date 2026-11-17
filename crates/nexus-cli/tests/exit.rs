//! 出口（M3.3）。功能点见 docs/feature-points/M3.3.md。

use nexus_testkit::{scenario, ProxyError, Service, World};

scenario! {
    name: traffic_can_go_out_through_a_device,
    covers: "R42",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        // home 那边「外面」的一个服务：laptop 自己连不上（仿真），或连得上但规则把它送经 home（真实）。
        let (ip, port) = w.serve_public("home", Service::Http("from the outside".into())).await;
        let v = w.cli_ok("laptop", &["route", "add", "--cidr", &format!("{ip}/32"), "--via", "home"]).await;
        assert_eq!(v["index"], 1, "{v}");
        let proxy = w.proxy("laptop");

        // home 还没有开放出口。
        assert_eq!(proxy.socks5_http_get(&ip, port, "/").await, Err(ProxyError::Socks5(2)));
        assert_eq!(proxy.http_get(&format!("http://{ip}:{port}/")).await.map(|r| r.0), Ok(403));

        let v = w.cli_ok("home", &["exit", "on"]).await;
        assert_eq!(v["exit"], true, "{v}");
        assert_eq!(w.cli_ok("home", &["exit", "status"]).await["exit"], true);
        assert_eq!(proxy.socks5_http_get(&ip, port, "/").await.as_deref(), Ok("from the outside"));
        assert_eq!(proxy.http_get(&format!("http://{ip}:{port}/")).await, Ok((200, "from the outside".to_string())));

        let audit = w.cli_ok("home", &["audit"]).await;
        assert!(
            audit["events"].as_array().unwrap().iter().any(|e| e["capability"] == "net.exit" && e["resource"] == "home/exit" && e["outcome"] == "allow"),
            "{audit}"
        );

        let e = w.cli_ok("laptop", &["route", "explain", &format!("{ip}:{port}")]).await;
        assert_eq!((e["outbound"].as_str(), e["via"].as_str(), e["rule"].as_u64()), (Some("exit"), Some("home"), Some(1)), "{e}");
        let text = w.cli("laptop", &["route", "explain", &format!("{ip}:{port}")]).await;
        assert!(text.stdout.contains("经 home 出去"), "{}", text.stdout);
        let rules = w.cli_ok("laptop", &["route", "ls"]).await;
        assert_eq!(rules["rules"], serde_json::json!([{ "index": 1, "match": format!("{ip}/32"), "outbound": "exit", "via": "home" }]));

        // 关掉之后又被拒绝。
        assert_eq!(w.cli_ok("home", &["exit", "off"]).await["exit"], false);
        assert_eq!(proxy.socks5_http_get(&ip, port, "/").await, Err(ProxyError::Socks5(2)));
    },
}

scenario! {
    name: an_exit_does_not_reach_its_own_local_addresses,
    covers: "R42",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let local = w.serve("home", Service::Http("home only".into())).await;
        w.cli_ok("home", &["exit", "on"]).await;
        w.cli_ok("laptop", &["route", "add", "--cidr", "127.0.0.0/8", "--via", "home"]).await;
        let proxy = w.proxy("laptop");
        assert_eq!(proxy.socks5_http_get("127.0.0.1", local, "/").await, Err(ProxyError::Socks5(2)));
        assert_eq!(proxy.http_get(&format!("http://127.0.0.1:{local}/")).await.map(|r| r.0), Ok(403));
    },
}

scenario! {
    name: exits_must_be_other_devices_in_the_roster,
    covers: "R42",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        assert_eq!(w.cli("laptop", &["route", "add", "--domain", "x.test", "--via", "nowhere"]).await.code, 103, "和其他命令一样：没有这台设备");
        assert_eq!(w.cli("laptop", &["route", "add", "--domain", "x.test", "--via", "laptop"]).await.code, 107);
        assert_eq!(w.cli_ok("laptop", &["route", "ls"]).await["rules"], serde_json::json!([]));
    },
}

scenario! {
    name: traffic_can_be_handed_to_an_upstream_socks5_proxy,
    covers: "R42",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        let up = w.serve("laptop", Service::Socks5Upstream).await;
        let upstream = format!("127.0.0.1:{up}");
        w.cli_ok("laptop", &["route", "add", "--domain", "far.test", "--socks5", &upstream]).await;
        let proxy = w.proxy("laptop");

        // 目标原样交给上游（不在本机解析）。
        assert_eq!(proxy.socks5_http_get("www.far.test", 80, "/").await.as_deref(), Ok("via upstream to www.far.test:80"));
        assert_eq!(proxy.http_get("http://far.test/").await, Ok((200, "via upstream to far.test:80".to_string())));
        // 上游拒绝了这个目标。
        assert_eq!(proxy.socks5_http_get("refuse.far.test", 80, "/").await, Err(ProxyError::Socks5(5)));
        assert_eq!(proxy.http_get("http://refuse.far.test/").await.map(|r| r.0), Ok(502));

        let e = w.cli_ok("laptop", &["route", "explain", "www.far.test:80"]).await;
        assert_eq!((e["outbound"].as_str(), e["upstream"].as_str()), (Some("socks5"), Some(upstream.as_str())), "{e}");

        // 上游没有在运行。
        let dead = format!("127.0.0.1:{}", w.unused_port());
        w.cli_ok("laptop", &["route", "add", "--domain", "dead.test", "--socks5", &dead]).await;
        assert_eq!(proxy.socks5_http_get("dead.test", 80, "/").await, Err(ProxyError::Socks5(4)));
        assert_eq!(proxy.http_get("http://dead.test/").await.map(|r| r.0), Ok(502));

        let rules = w.cli_ok("laptop", &["route", "ls"]).await;
        assert_eq!(rules["rules"], serde_json::json!([
            { "index": 1, "match": "far.test", "outbound": "socks5", "upstream": upstream },
            { "index": 2, "match": "dead.test", "outbound": "socks5", "upstream": dead },
        ]));
        assert_eq!(w.cli("laptop", &["route", "add", "--domain", "x.test", "--socks5", "no-port"]).await.code, 107);
    },
}
