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

/// 本机的 UDP 回声服务：每个数据报原样送回，前面加上 `echo:`。返回端口。
fn udp_echo() -> u16 {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = sock.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            let mut reply = b"echo:".to_vec();
            reply.extend_from_slice(&buf[..n]);
            let _ = sock.send_to(&reply, from);
        }
    });
    port
}

/// 从一个新的 UDP 套接字发 `data` 给 `ip:port`，等回应（2 秒一次，最多五次：UDP 可能丢）。
fn udp_ask(sock: &std::net::UdpSocket, ip: IpAddr, port: u16, data: &[u8]) -> Option<Vec<u8>> {
    sock.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
    let mut buf = [0u8; 65536];
    (0..5).find_map(|_| {
        sock.send_to(data, (ip, port)).ok()?;
        let n = sock.recv(&mut buf).ok()?;
        Some(buf[..n].to_vec())
    })
}

scenario! {
    name: udp_services_are_reachable_through_the_full_node,
    covers: "R91",
    nodes: ["laptop", "home"],
    sim: (pending "虚拟网卡是真实的系统设备"),
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        let port = udp_echo().to_string();
        w.cli_ok("home", &["publish", "game", "--port", &port, "--proto", "udp"]).await;
        let tcp = w.serve("home", Service::Echo).await.to_string();
        w.cli_ok("home", &["publish", "echo", "--port", &tcp]).await;
        let full = w.full_node("laptop").await;
        let ip = full.resolve("game.home.internal", 1).unwrap()[0];

        // 发往服务的临时地址（任何端口）的数据报到了 home 上的 UDP 服务，回应原路送回。
        let a = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let got = udp_ask(&a, ip, 9000, b"hi");
        assert_eq!(got.as_deref(), Some(&b"echo:hi"[..]), "{}\n{}", w.daemon_log("laptop"), w.daemon_log("home"));
        let big = vec![b'x'; 1200];
        let mut want = b"echo:".to_vec();
        want.extend_from_slice(&big);
        assert_eq!(udp_ask(&a, ip, 9000, &big), Some(want), "1200 字节的数据报");
        // 另一个程序（另一个源端口）各走各的。
        let b = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        assert_eq!(udp_ask(&b, ip, 53, b"two").as_deref(), Some(&b"echo:two"[..]));
        assert_eq!(udp_ask(&a, ip, 9000, b"one").as_deref(), Some(&b"echo:one"[..]));

        // nexus connect 只转发 TCP：说明 UDP 服务要经完整节点访问。
        let out = w.cli("laptop", &["connect", "home/game"]).await;
        assert_ne!(out.code, 0);
        assert!(out.stderr.contains("UDP") && out.stderr.contains("game.home.internal"), "{}", out.stderr);

        // TCP 服务不收 UDP：没有回应。
        let echo = full.resolve("echo.home.internal", 1).unwrap()[0];
        let c = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
        c.send_to(b"x", (echo, 7)).unwrap();
        assert!(c.recv(&mut [0u8; 16]).is_err());
    },
}
