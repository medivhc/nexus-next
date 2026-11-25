//! 完整节点：虚拟网卡与名字解析（M3.4）。功能点见 docs/feature-points/M3.4.md。
//! 要求 CAP_NET_ADMIN 和 /dev/net/tun；仿真里没有虚拟网卡。

use nexus_testkit::{scenario, Service, World};
use std::net::IpAddr;

scenario! {
    name: names_resolve_to_local_addresses_that_reach_services,
    covers: "R43",
    nodes: ["laptop", "home"],
    sim: (pending "虚拟网卡是真实的系统设备"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let port = w.serve("home", Service::Http("hello from home".into())).await.to_string();
        w.cli_ok("home", &["publish", "web", "--port", &port, "--proto", "http"]).await;
        let full = w.full_node("laptop").await;

        let ips = full.resolve("web.home.internal", 1).unwrap_or_else(|e| panic!("应当解析成本机临时地址：{e}\n{}\n{}", w.daemon_log("laptop"), w.daemon_log("home")));
        assert_eq!(ips.len(), 1);
        let ip = ips[0];
        assert!(full.contains(ip), "{ip} 应当在 {:?} 里", full.net());
        assert_eq!(full.resolve("WEB.home.internal", 1), Ok(vec![ip]), "名字不分大小写，地址稳定");
        assert_eq!(full.resolve("web.home.internal", 28), Ok(vec![]), "没有 IPv6 地址：NOERROR、没有回答");

        // 不设代理，普通程序按地址连，任何端口都到这个服务。
        assert_eq!(full.http_get(ip, 80, "/").ok().as_deref(), Some("hello from home"));
        assert_eq!(full.http_get(ip, 8080, "/").ok().as_deref(), Some("hello from home"));

        // 没有这台设备、不合法的名字：NXDOMAIN。
        assert_eq!(full.resolve("web.nowhere.internal", 1), Err(3));
        assert_eq!(full.resolve("home.internal", 1), Err(3));
        // 设备在但没有这个服务：名字可以解析，连接被关闭。
        let nope = full.resolve("nope.home.internal", 1).unwrap()[0];
        assert!(full.http_get(nope, 80, "/").map(|b| b.is_empty()).unwrap_or(true));

        let status = w.cli_ok("laptop", &["status"]).await;
        assert_eq!(status["tun"]["dns"].as_str(), Some(full.dns().to_string().as_str()), "{status}");
    },
}

scenario! {
    name: rule_domains_are_captured_by_the_full_node,
    covers: "R43, R41",
    nodes: ["laptop"],
    sim: (pending "虚拟网卡是真实的系统设备"),
    real: yes,
    run: |w: World| async move {
        let up = w.serve("laptop", Service::Socks5Upstream).await;
        w.cli_ok("laptop", &["route", "add", "--domain", "far.test", "--socks5", &format!("127.0.0.1:{up}")]).await;
        w.cli_ok("laptop", &["route", "add", "--domain", "blocked.test", "--reject"]).await;
        let full = w.full_node("laptop").await;

        // 规则里的域名解析成本机临时地址，连接按规则走。
        let far = full.resolve("www.far.test", 1).unwrap()[0];
        assert!(full.contains(far));
        assert_eq!(full.http_get(far, 80, "/").ok().as_deref(), Some("via upstream to www.far.test:80"));
        let blocked = full.resolve("blocked.test", 1).unwrap()[0];
        assert!(full.http_get(blocked, 80, "/").map(|b| b.is_empty()).unwrap_or(true));

        // 其余的名字照常解析。
        assert_eq!(full.resolve("localhost", 1), Ok(vec![IpAddr::from([127, 0, 0, 1])]));
    },
}

scenario! {
    name: the_full_node_needs_a_valid_address_range,
    covers: "R43",
    nodes: ["laptop"],
    sim: (pending "虚拟网卡是真实的系统设备"),
    real: yes,
    run: |w: World| async move {
        for bad in ["1.2.3.4/40", "198.18.0.1/24", "nonsense"] {
            let out = w.cli("laptop", &["daemon", "--tun", "nxbad0", "--tun-net", bad]).await;
            assert_eq!(out.code, 107, "{bad}: {}", out.stderr);
        }
        let out = w.cli("laptop", &["daemon", "--tun-net", "198.18.0.0/24"]).await;
        assert_eq!(out.code, 102, "--tun-net 要和 --tun 一起用：{}", out.stderr);
    },
}

scenario! {
    name: half_closed_connections_still_get_their_reply,
    covers: "R89",
    nodes: ["laptop", "home"],
    sim: (pending "虚拟网卡是真实的系统设备"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let port = w.serve("home", Service::Echo).await.to_string();
        w.cli_ok("home", &["publish", "echo", "--port", &port]).await;
        let full = w.full_node("laptop").await;
        let ip = full.resolve("echo.home.internal", 1).unwrap()[0];

        // 像 `echo hello | nc`：发完就关闭写方向，回应照样回来，连接随后正常结束。
        let got = full.send_then_half_close(ip, 7, b"hello\n");
        assert_eq!(got.as_deref().ok(), Some(&b"hello\n"[..]), "{got:?}\n{}", w.daemon_log("laptop"));

        // 数据比一个包大、跨多个包时也一样。
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let got = full.send_then_half_close(ip, 7, &big).expect("大块数据的回应");
        assert!(got == big, "回来 {} 字节，应当 {}", got.len(), big.len());
    },
}
