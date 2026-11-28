//! 出口（M3.3，FP-3.3-04 的纯逻辑部分）：作为 SOCKS5 客户端连上游代理。

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use nexus_net_core::route::{decide, Decision, Matcher, Outbound, Rule};
use nexus_net_core::{socks5, Host, Target};
use proptest::prelude::*;

#[test]
fn requests_keep_the_target_as_given() {
    let t = Target { host: Host::Name("www.far.test".into()), port: 80 };
    let mut want = vec![5, 1, 0, 3, 12];
    want.extend_from_slice(b"www.far.test");
    want.extend_from_slice(&[0, 80]);
    assert_eq!(socks5::request(&t), want);
    let t = Target { host: Host::Ip(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))), port: 443 };
    assert_eq!(socks5::request(&t), vec![5, 1, 0, 1, 10, 1, 2, 3, 1, 187]);
    let t = Target { host: Host::Ip(IpAddr::V6(Ipv6Addr::LOCALHOST)), port: 1 };
    assert_eq!(socks5::request(&t).len(), 4 + 16 + 2);
}

#[test]
fn replies_are_read_whole() {
    // 不完整
    assert_eq!(socks5::parse_reply(&[5, 0, 0, 1, 0, 0]), Ok(None));
    // IPv4、IPv6、域名三种绑定地址
    assert_eq!(socks5::parse_reply(&socks5::reply(socks5::Reply::Succeeded)), Ok(Some((10, 0))));
    let mut v6 = vec![5, 5, 0, 4];
    v6.extend_from_slice(&[0; 18]);
    v6.push(42); // 之后的数据不属于回应
    assert_eq!(socks5::parse_reply(&v6), Ok(Some((22, 5))));
    let mut dom = vec![5, 0, 0, 3, 3];
    dom.extend_from_slice(b"abc\0\x50");
    assert_eq!(socks5::parse_reply(&dom), Ok(Some((10, 0))));
    // 版本不对、地址类型不认识
    assert!(socks5::parse_reply(&[4, 0, 0, 1, 0, 0, 0, 0, 0, 0]).is_err());
    assert!(socks5::parse_reply(&[5, 0, 0, 9, 0, 0, 0, 0, 0, 0]).is_err());
}

#[test]
fn rules_can_send_traffic_out_through_a_device_or_an_upstream() {
    let rules = vec![
        Rule { matcher: Matcher::parse_domain("far.test").unwrap(), outbound: Outbound::Socks5("127.0.0.1:7890".into()) },
        Rule { matcher: Matcher::parse_cidr("198.51.100.0/24").unwrap(), outbound: Outbound::Exit("home".into()) },
    ];
    let t = Target::parse("www.far.test:80").unwrap();
    assert_eq!(decide(&t, &rules), Decision::Rule { index: 0, outbound: Outbound::Socks5("127.0.0.1:7890".into()) });
    let t = Target::parse("198.51.100.7:8080").unwrap();
    assert_eq!(decide(&t, &rules), Decision::Rule { index: 1, outbound: Outbound::Exit("home".into()) });
    // 服务的名字总在规则之前。
    let t = Target::parse("web.home.internal:80").unwrap();
    assert!(matches!(decide(&t, &rules), Decision::Service { .. }));
    assert_eq!(Outbound::Exit("home".into()).to_string(), "经 home 出去");
    assert_eq!(Outbound::Socks5("127.0.0.1:7890".into()).to_string(), "经上游代理 127.0.0.1:7890");
}

proptest! {
    /// 自己编出的请求，自己的解析器读回来是同一个目标。
    #[test]
    fn requests_round_trip(name in "[a-z0-9]{1,20}(\\.[a-z0-9]{1,10}){0,3}", port in 1u16..) {
        let t = Target { host: Host::Name(name), port };
        let bytes = socks5::request(&t);
        prop_assert_eq!(socks5::parse_request(&bytes), Ok(Some((bytes.len(), t))));
    }
}
