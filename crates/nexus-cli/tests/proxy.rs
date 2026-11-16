//! 本机统一入口与路由规则（M3.2）。功能点见 docs/feature-points/M3.2.md。

use nexus_testkit::{scenario, ProxyError, Service, World};

scenario! {
    name: services_are_reached_by_name_through_the_local_entry,
    covers: "R40",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let port = w.serve("home", Service::Http("hello from home".into())).await.to_string();
        w.cli_ok("home", &["publish", "web", "--port", &port, "--proto", "http"]).await;
        let echo = w.serve("home", Service::Echo).await.to_string();
        w.cli_ok("home", &["publish", "echo", "--port", &echo]).await;
        let proxy = w.proxy("laptop");

        // SOCKS5
        assert_eq!(proxy.socks5_http_get("web.home.internal", 80, "/").await.as_deref(), Ok("hello from home"));
        // HTTP 代理的普通请求
        assert_eq!(proxy.http_get("http://web.home.internal/").await, Ok((200, "hello from home".to_string())));
        // HTTP 代理的 CONNECT 隧道
        assert_eq!(proxy.connect_tunnel("echo.home.internal:443", b"through the tunnel").await.as_deref(), Ok(&b"through the tunnel"[..]));

        // 不存在的服务、不存在的设备、不合法的名字。
        assert_eq!(proxy.socks5_http_get("nope.home.internal", 80, "/").await, Err(ProxyError::Socks5(4)));
        assert_eq!(proxy.socks5_http_get("web.nowhere.internal", 80, "/").await, Err(ProxyError::Socks5(4)));
        assert_eq!(proxy.http_get("http://nope.home.internal/").await.map(|r| r.0), Ok(502));
        assert_eq!(proxy.http_get("http://home.internal/").await.map(|r| r.0), Ok(502));
    },
}

scenario! {
    name: other_traffic_follows_the_rules,
    covers: "R40, R41",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        // laptop 本机上的一个普通服务：没有任何规则时直连。
        let local = w.serve("laptop", Service::Http("plain local service".into())).await;
        let proxy = w.proxy("laptop");
        assert_eq!(proxy.socks5_http_get("127.0.0.1", local, "/").await.as_deref(), Ok("plain local service"));
        assert_eq!(proxy.http_get(&format!("http://127.0.0.1:{local}/")).await, Ok((200, "plain local service".to_string())));

        // 拒绝一个域名和它的子域名；拒绝一个地址段。
        let v = w.cli_ok("laptop", &["route", "add", "--domain", "example.test", "--reject"]).await;
        assert_eq!(v["index"], 1, "{v}");
        w.cli_ok("laptop", &["route", "add", "--cidr", "127.0.0.0/8", "--reject"]).await;
        assert_eq!(proxy.socks5_http_get("www.example.test", 443, "/").await, Err(ProxyError::Socks5(2)));
        assert_eq!(proxy.http_get("http://example.test/").await.map(|r| r.0), Ok(403));
        assert_eq!(proxy.socks5_http_get("127.0.0.1", local, "/").await, Err(ProxyError::Socks5(2)));

        let rules = w.cli_ok("laptop", &["route", "ls"]).await;
        assert_eq!(rules["rules"], serde_json::json!([
            { "index": 1, "match": "example.test", "outbound": "reject" },
            { "index": 2, "match": "127.0.0.0/8", "outbound": "reject" },
        ]));

        let e = w.cli_ok("laptop", &["route", "explain", "www.example.test:443"]).await;
        assert_eq!((e["outbound"].as_str(), e["rule"].as_u64()), (Some("reject"), Some(1)), "{e}");
        let e = w.cli_ok("laptop", &["route", "explain", "web.home.internal:80"]).await;
        assert_eq!((e["outbound"].as_str(), e["device"].as_str(), e["service"].as_str()), (Some("nexus"), Some("home"), Some("web")), "{e}");
        let e = w.cli_ok("laptop", &["route", "explain", "other.test:80"]).await;
        assert_eq!((e["outbound"].as_str(), e["rule"].is_null()), (Some("direct"), true), "{e}");
        let text = w.cli("laptop", &["route", "explain", "www.example.test:443"]).await;
        assert!(text.stdout.contains("example.test") && text.stdout.contains("拒绝"), "{}", text.stdout);

        // 删除规则后恢复直连。
        w.cli_ok("laptop", &["route", "rm", "2"]).await;
        assert_eq!(proxy.socks5_http_get("127.0.0.1", local, "/").await.as_deref(), Ok("plain local service"));
        assert_eq!(w.cli("laptop", &["route", "rm", "9"]).await.code, 109);
        for bad in [&["route", "add", "--domain", "bad domain", "--reject"][..], &["route", "add", "--cidr", "1.2.3.4/40", "--reject"]] {
            assert_eq!(w.cli("laptop", bad).await.code, 107, "{bad:?}");
        }
        assert_eq!(w.cli("laptop", &["route", "explain", "no-port"]).await.code, 107);
    },
}

scenario! {
    name: the_local_entry_rejects_garbage,
    covers: "R40",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        let proxy = w.proxy("laptop");
        assert!(proxy.raw(b"\x16\x03\x01garbage").await.is_empty(), "不认识的协议应当直接关闭");
    },
}

scenario! {
    name: the_local_entry_only_listens_locally,
    covers: "R40",
    nodes: ["laptop"],
    sim: (pending "监听地址是真实的本机网络"),
    real: yes,
    run: |w: World| async move {
        let out = w.cli("laptop", &["daemon", "--proxy", "0.0.0.0:0"]).await;
        assert_eq!(out.code, 107, "{}", out.stderr);
    },
}

scenario! {
    name: nexus_run_injects_service_environment,
    covers: "R45",
    nodes: ["laptop", "home"],
    sim: (pending "nexus run 启动真实的进程"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let port = w.serve("home", Service::Http("hello from home".into())).await.to_string();
        w.cli_ok("home", &["publish", "web", "--port", &port, "--proto", "http", "--env", "BASE_URL=http://{addr}/v1"]).await;

        let out = w.cli("laptop", &["run", "--use", "home/web", "--", "sh", "-c", "echo $BASE_URL"]).await;
        assert_eq!(out.code, 0, "{}", out.stderr);
        let url = out.stdout.trim().to_string();
        assert!(url.starts_with("http://127.0.0.1:") && url.ends_with("/v1"), "{url}");

        // 程序真的能用这个地址访问 home 上的服务。
        let out = w.cli("laptop", &["run", "--use", "home/web", "--", "sh", "-c", "curl -s --noproxy '*' ${BASE_URL%/v1}/ ; exit 7"]).await;
        assert_eq!(out.code, 7, "{}", out.stderr);
        assert_eq!(out.stdout, "hello from home");

        // 服务不存在：程序不启动。
        let out = w.cli("laptop", &["run", "--use", "home/nope", "--", "sh", "-c", "echo started"]).await;
        assert_eq!(out.code, 109, "{}", out.stderr);
        assert!(!out.stdout.contains("started"));
    },
}
