//! NEXUS: 本机地址 → 网络接口编号（多路径）。
//!
//! 发送时指定了源地址的包，要从拥有这个地址的接口出去：一台设备有有线和 5G 两个接口时，
//! 同一个套接字上的两条路径才会分别走两个接口（否则系统只按目的地址选路，两条路径都走默认路由）。
//! 查表的结果缓存一秒；地址不在任何接口上时返回 0（由系统选）。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const FRESH: Duration = Duration::from_secs(1);

struct Table {
    at: Instant,
    map: HashMap<IpAddr, u32>,
}

static TABLE: Mutex<Option<Table>> = Mutex::new(None);

pub(crate) fn of(ip: IpAddr) -> u32 {
    if ip.is_unspecified() || ip.is_loopback() {
        return 0;
    }
    let mut guard = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    // 过期就重读；没找到时也重读，但最多每 100 毫秒一次（新出现的接口很快能用上）。
    let refresh = match guard.as_ref() {
        None => true,
        Some(t) => t.at.elapsed() > FRESH || (!t.map.contains_key(&ip) && t.at.elapsed() > Duration::from_millis(100)),
    };
    if refresh {
        *guard = Some(Table { at: Instant::now(), map: read() });
    }
    guard.as_ref().and_then(|t| t.map.get(&ip).copied()).unwrap_or(0)
}

fn read() -> HashMap<IpAddr, u32> {
    let mut map = HashMap::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs 填好链表头；之后只读链表，最后 freeifaddrs 释放。
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return map;
        }
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null() && !ifa.ifa_name.is_null() {
                let family = (*ifa.ifa_addr).sa_family as i32;
                let ip = if family == libc::AF_INET {
                    let sa = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr))))
                } else if family == libc::AF_INET6 {
                    let sa = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    Some(IpAddr::V6(Ipv6Addr::from(sa.sin6_addr.s6_addr)))
                } else {
                    None
                };
                if let Some(ip) = ip {
                    let idx = libc::if_nametoindex(ifa.ifa_name);
                    if idx != 0 {
                        map.insert(ip, idx);
                    }
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(head);
    }
    map
}
