//! 统一入口的协议解析与路由判定（M3.2，FP-3.2-01、02、03 的纯逻辑部分）。

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use nexus_net_core::route::{decide, Decision, Matcher, Outbound, Rule};
use nexus_net_core::{http, sniff, socks5, Entry, Host, Target};
use proptest::prelude::*;

fn name(h: &str, port: u16) -> Target {
    Target { host: Host::Name(h.into()), port }
}

#[test]
fn the_first_byte_tells_socks5_from_http() {
    assert_eq!(sniff(0x05), Some(Entry::Socks5));
    for b in *b"GCPHODT" {
        assert_eq!(sniff(b), Some(Entry::Http), "{}", b as char);
    }
    for b in [0x04, 0x16, b'g', b' ', 0] {
        assert_eq!(sniff(b), None, "{b:#x}");
    }
}

#[test]
fn socks5_greeting_and_connect() {
    assert_eq!(socks5::parse_greeting(&[5]), Ok(None));
    assert_eq!(socks5::parse_greeting(&[5, 2, 2]), Ok(None));
    assert_eq!(socks5::parse_greeting(&[5, 2, 2, 0, 9]), Ok(Some((4, true))));
    assert_eq!(socks5::parse_greeting(&[5, 1, 2]), Ok(Some((3, false))), "只提供用户名密码时不接受");
    assert!(socks5::parse_greeting(&[4, 1, 0]).is_err());

    // 域名
    let mut req = vec![5, 1, 0, 3, 17];
    req.extend_from_slice(b"web.home.internal");
    req.extend_from_slice(&80u16.to_be_bytes());
    assert_eq!(socks5::parse_request(&req[..req.len() - 1]), Ok(None));
    assert_eq!(socks5::parse_request(&req), Ok(Some((req.len(), name("web.home.internal", 80)))));
    // IPv4 与 IPv6
    let v4 = [5, 1, 0, 1, 127, 0, 0, 1, 0x1f, 0x90];
    assert_eq!(socks5::parse_request(&v4), Ok(Some((10, Target { host: Host::Ip(IpAddr::V4(Ipv4Addr::LOCALHOST)), port: 8080 }))));
    let mut v6 = vec![5, 1, 0, 4];
    v6.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
    v6.extend_from_slice(&443u16.to_be_bytes());
    assert_eq!(socks5::parse_request(&v6), Ok(Some((22, Target { host: Host::Ip(IpAddr::V6(Ipv6Addr::LOCALHOST)), port: 443 }))));
    // 只支持 CONNECT
    assert_eq!(socks5::parse_request(&[5, 3, 0, 1, 1, 2, 3, 4, 0, 1]), Err(socks5::Reply::CommandNotSupported));
    assert_eq!(socks5::parse_request(&[5, 1, 0, 9, 1, 2, 3, 4, 0, 1]), Err(socks5::Reply::AddressTypeNotSupported));
    // 回应：版本、结果、保留、IPv4 0.0.0.0:0
    assert_eq!(socks5::reply(socks5::Reply::Succeeded), [5, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    assert_eq!(socks5::reply(socks5::Reply::HostUnreachable)[1], 4);
    assert_eq!(socks5::reply(socks5::Reply::NotAllowed)[1], 2);
}

#[test]
fn http_proxy_requests() {
    let head = b"CONNECT web.home.internal:443 HTTP/1.1\r\nHost: web.home.internal:443\r\n\r\n";
    assert_eq!(http::parse(head), Ok(http::ProxyRequest::Connect(name("web.home.internal", 443))));

    let head = b"GET http://web.home.internal/v1/models?x=1 HTTP/1.1\r\nHost: web.home.internal\r\nProxy-Connection: keep-alive\r\nAccept: */*\r\n\r\n";
    let http::ProxyRequest::Forward { target, head } = http::parse(head).unwrap() else { panic!("应当是转发") };
    assert_eq!(target, name("web.home.internal", 80));
    let text = String::from_utf8(head).unwrap();
    assert!(text.starts_with("GET /v1/models?x=1 HTTP/1.1\r\n"), "{text}");
    assert!(!text.to_ascii_lowercase().contains("proxy-connection"), "{text}");
    assert!(text.contains("Accept: */*\r\n") && text.ends_with("\r\n\r\n"), "{text}");

    let head = b"POST http://127.0.0.1:8080 HTTP/1.1\r\nHost: 127.0.0.1:8080\r\n\r\n";
    let http::ProxyRequest::Forward { target, head } = http::parse(head).unwrap() else { panic!() };
    assert_eq!(target, Target { host: Host::Ip(IpAddr::V4(Ipv4Addr::LOCALHOST)), port: 8080 });
    assert!(head.starts_with(b"POST / HTTP/1.1\r\n"));
    let head = b"GET http://[::1]:9000/a HTTP/1.1\r\n\r\n";
    let http::ProxyRequest::Forward { target, .. } = http::parse(head).unwrap() else { panic!() };
    assert_eq!(target, Target { host: Host::Ip(IpAddr::V6(Ipv6Addr::LOCALHOST)), port: 9000 });

    for bad in [&b"GET /relative HTTP/1.1\r\n\r\n"[..], b"GET https://x/ HTTP/1.1\r\n\r\n", b"CONNECT nohost HTTP/1.1\r\n\r\n", b"garbage\r\n\r\n", b"CONNECT a:99999 HTTP/1.1\r\n\r\n"] {
        assert!(http::parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
    }
    assert!(http::response(502, "Bad Gateway", "连不上").starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
    assert_eq!(http::CONNECT_OK, b"HTTP/1.1 200 Connection Established\r\n\r\n");
}

#[test]
fn internal_names_go_to_services_and_rules_decide_the_rest() {
    let rules = vec![
        Rule { matcher: Matcher::parse_domain("example.test").unwrap(), outbound: Outbound::Reject },
        Rule { matcher: Matcher::parse_cidr("10.0.0.0/8").unwrap(), outbound: Outbound::Reject },
    ];
    assert_eq!(decide(&name("web.home.internal", 80), &rules), Decision::Service { device: "home".into(), service: "web".into() });
    assert_eq!(decide(&name("WEB.Home.Internal.", 80), &rules), Decision::Service { device: "home".into(), service: "web".into() });
    for bad in ["home.internal", "a.b.c.internal", "internal", "we b.home.internal"] {
        assert_eq!(decide(&name(bad, 80), &rules), Decision::BadInternalName, "{bad}");
    }
    assert_eq!(decide(&name("example.test", 443), &rules), Decision::Rule { index: 0, outbound: Outbound::Reject });
    assert_eq!(decide(&name("www.example.test", 443), &rules), Decision::Rule { index: 0, outbound: Outbound::Reject });
    assert_eq!(decide(&name("notexample.test", 443), &rules), Decision::Default(Outbound::Direct));
    let ip = |a, b, c, d| Target { host: Host::Ip(IpAddr::V4(Ipv4Addr::new(a, b, c, d))), port: 1 };
    assert_eq!(decide(&ip(10, 1, 2, 3), &rules), Decision::Rule { index: 1, outbound: Outbound::Reject });
    assert_eq!(decide(&ip(11, 1, 2, 3), &rules), Decision::Default(Outbound::Direct));

    assert!(Matcher::parse_domain("bad domain").is_err());
    assert!(Matcher::parse_cidr("10.0.0.0/33").is_err());
    assert!(Matcher::parse_cidr("fd00::/8").is_ok());
    assert_eq!(Matcher::parse_domain("Example.TEST.").unwrap().to_string(), "example.test");
    assert_eq!(Matcher::parse_cidr("10.1.2.3/8").unwrap().to_string(), "10.0.0.0/8");
}

proptest! {
    // 任意输入都不会让解析崩溃；解析成功时消耗的字节不超过输入。
    #[test]
    fn parsers_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
        if let Ok(Some((n, _))) = socks5::parse_greeting(&bytes) { prop_assert!(n <= bytes.len()); }
        if let Ok(Some((n, _))) = socks5::parse_request(&bytes) { prop_assert!(n <= bytes.len()); }
        let _ = http::parse(&bytes);
    }

    // CIDR 的匹配和逐位比较一致。
    #[test]
    fn cidr_matches_by_prefix(a in any::<u32>(), b in any::<u32>(), len in 0u8..=32) {
        let net = Ipv4Addr::from(a);
        let m = Matcher::parse_cidr(&format!("{net}/{len}")).unwrap();
        let t = Target { host: Host::Ip(IpAddr::V4(Ipv4Addr::from(b))), port: 1 };
        let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
        prop_assert_eq!(m.matches(&t), a & mask == b & mask);
    }
}
