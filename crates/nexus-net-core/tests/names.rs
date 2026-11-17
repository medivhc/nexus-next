//! 完整节点的名字解析（M3.4 的纯逻辑部分）：DNS 报文与本机临时地址。

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use nexus_net_core::dns::{self, Query};
use nexus_net_core::fakeip::FakePool;
use proptest::prelude::*;

/// 手写的查询：id 0x1234，要求递归，一个问题。
fn query(name: &str, qtype: u16) -> Vec<u8> {
    let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in name.split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, (qtype >> 8) as u8, qtype as u8, 0, 1]);
    q
}

#[test]
fn queries_are_read() {
    let q = dns::parse_query(&query("Web.Home.internal", dns::A)).unwrap();
    assert_eq!((q.id, q.name.as_str(), q.qtype), (0x1234, "web.home.internal", dns::A));
    // 回应、多个问题、截断的报文都不是我们要回答的查询。
    let mut resp = query("a.b", dns::A);
    resp[2] |= 0x80;
    assert_eq!(dns::parse_query(&resp), None);
    let mut two = query("a.b", dns::A);
    two[5] = 2;
    assert_eq!(dns::parse_query(&two), None);
    assert_eq!(dns::parse_query(&query("a.b", dns::A)[..15]), None);
    assert_eq!(dns::parse_query(&[]), None);
}

#[test]
fn answers_carry_only_records_of_the_asked_type() {
    let raw = query("web.home.internal", dns::A);
    let q: Query = dns::parse_query(&raw).unwrap();
    let ips = [IpAddr::V4(Ipv4Addr::new(198, 18, 0, 3)), IpAddr::V6(Ipv6Addr::LOCALHOST)];
    let out = dns::answer(&q, &ips, 60);
    assert_eq!(&out[..2], &[0x12, 0x34], "同一个 id");
    assert_eq!(out[2] & 0x80, 0x80, "是回应");
    assert_eq!(out[2] & 0x01, 0x01, "保留「要求递归」");
    assert_eq!(out[3] & 0x0f, 0, "NOERROR");
    assert_eq!(&out[4..12], &[0, 1, 0, 1, 0, 0, 0, 0], "一个问题、一条回答");
    assert_eq!(&out[12..raw.len()], &raw[12..], "问题原样带回");
    let rr = &out[raw.len()..];
    assert_eq!(rr, &[0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 198, 18, 0, 3]);

    let raw6 = query("web.home.internal", dns::AAAA);
    let out = dns::answer(&dns::parse_query(&raw6).unwrap(), &ips, 60);
    assert_eq!(&out[6..8], &[0, 1]);
    assert_eq!(out.len(), raw6.len() + 12 + 16);
    // 没有这种地址：NOERROR 但没有回答。
    let out = dns::answer(&dns::parse_query(&raw6).unwrap(), &ips[..1], 60);
    assert_eq!((out[3] & 0x0f, &out[6..8]), (0, &[0u8, 0][..]));
}

#[test]
fn errors_keep_the_question() {
    let raw = query("nowhere.internal", dns::A);
    let out = dns::error(&dns::parse_query(&raw).unwrap(), dns::NXDOMAIN);
    assert_eq!(out.len(), raw.len());
    assert_eq!(out[3] & 0x0f, 3);
    assert_eq!(&out[6..8], &[0, 0]);
}

#[test]
fn local_addresses_are_stable_and_reversible() {
    let mut pool = FakePool::new(Ipv4Addr::new(198, 18, 0, 0), 24).unwrap();
    assert_eq!(pool.gateway(), Ipv4Addr::new(198, 18, 0, 1));
    assert_eq!(pool.dns(), Ipv4Addr::new(198, 18, 0, 2));
    let a = pool.assign("web.home.internal");
    let b = pool.assign("echo.home.internal");
    assert_eq!(a, Ipv4Addr::new(198, 18, 0, 3));
    assert_ne!(a, b);
    assert_eq!(pool.assign("web.home.internal"), a, "同一个名字总是同一个地址");
    assert_eq!(pool.lookup(a), Some("web.home.internal"));
    assert_eq!(pool.lookup(Ipv4Addr::new(198, 18, 0, 200)), None);
    assert!(pool.contains(a) && !pool.contains(Ipv4Addr::new(10, 0, 0, 1)));
    // 不合法的网段
    assert!(FakePool::new(Ipv4Addr::new(198, 18, 0, 0), 31).is_none());
    assert!(FakePool::new(Ipv4Addr::new(198, 18, 0, 1), 24).is_none(), "不是网段的起点");
}

#[test]
fn a_full_pool_reuses_the_oldest_address() {
    // /29：8 个地址，去掉网段、网关、名字服务、广播，剩 4 个。
    let mut pool = FakePool::new(Ipv4Addr::new(10, 9, 0, 0), 29).unwrap();
    let first: Vec<_> = (0..4).map(|i| pool.assign(&format!("n{i}.internal"))).collect();
    let next = pool.assign("n4.internal");
    assert_eq!(next, first[0]);
    assert_eq!(pool.lookup(next), Some("n4.internal"));
    assert_eq!(pool.assign("n1.internal"), first[1], "其余的不受影响");
    assert!(!first.contains(&Ipv4Addr::new(10, 9, 0, 7)), "不用广播地址");
}

proptest! {
    #[test]
    fn any_bytes_are_safe(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
        if let Some(q) = dns::parse_query(&bytes) {
            let _ = dns::answer(&q, &[IpAddr::V4(Ipv4Addr::LOCALHOST)], 1);
            let _ = dns::error(&q, dns::SERVFAIL);
        }
    }

    #[test]
    fn assigned_addresses_stay_in_the_pool(names in proptest::collection::vec("[a-z]{1,8}", 1..50)) {
        let mut pool = FakePool::new(Ipv4Addr::new(198, 18, 4, 0), 28).unwrap();
        for n in &names {
            let ip = pool.assign(n);
            prop_assert!(pool.contains(ip));
            prop_assert!(ip != pool.gateway() && ip != pool.dns());
            prop_assert_eq!(pool.lookup(ip), Some(n.as_str()));
        }
    }
}
