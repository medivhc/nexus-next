// covers: R40, R41, R109
//! N6 显式 HTTP 代理变换与 MCP 隔离策略模拟端到端测试（仿真 + 真实后端）。

use nexus_testkit::{scenario, Service, World};

// TC-N6-04：显式 HTTP 代理 Map Local、测试服务重定向与 503 故障注入
scenario! {
    name: http_proxy_map_local_redirect_and_fault_inject,
    covers: "R40, R109",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
        let test_port = w.serve("laptop", Service::Http("from local staging".into())).await;

        w.cli_ok(
            "laptop",
            &[
                "http", "add", "--id", "mock-v1", "--domain-exact", "api.dev.test",
                "--path", "/v1/mock", "--mock-json", r#"{"mock":true}"#,
            ],
        )
        .await;
        w.cli_ok(
            "laptop",
            &[
                "http", "add", "--id", "redir-v1", "--domain-exact", "api.dev.test",
                "--path", "/v1/staging", "--redirect", &format!("127.0.0.1:{test_port}"),
            ],
        )
        .await;
        w.cli_ok(
            "laptop",
            &[
                "http", "add", "--id", "fault-503", "--domain", "flaky.dev.test",
                "--fault", "503",
            ],
        )
        .await;

        let list = w.cli_ok("laptop", &["http", "ls"]).await;
        assert_eq!(list["rules"].as_array().unwrap().len(), 3, "{list}");

        let proxy = w.proxy("laptop");
        assert_eq!(
            proxy.http_get("http://api.dev.test/v1/mock").await,
            Ok((200, r#"{"mock":true}"#.to_string()))
        );
        assert_eq!(
            proxy.http_get("http://api.dev.test/v1/staging").await,
            Ok((200, "from local staging".to_string()))
        );
        assert_eq!(
            proxy.http_get("http://flaky.dev.test/ping").await.map(|r| r.0),
            Ok(503)
        );

        // 验证本地脱敏流事件记录了 http_mock 与 http_rewrite
        let flows = w.cli_ok("laptop", &["flow", "ls", "--limit", "10"]).await;
        let events = flows["events"].as_array().unwrap();
        assert!(events.iter().any(|e| e["decision"] == "http_mock:mock-v1"), "{flows}");
        assert!(events.iter().any(|e| e["decision"] == "http_rewrite:redir-v1"), "{flows}");

        w.cli_ok("laptop", &["http", "rm", "mock-v1"]).await;
        assert_eq!(w.cli_ok("laptop", &["http", "ls"]).await["rules"].as_array().unwrap().len(), 2);
    },
}
