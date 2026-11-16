//! 纯逻辑：本机统一入口的协议解析（SOCKS5、HTTP 代理）与路由判定。
//!
//! 这里只处理字节和规则，不碰网络：读写由运行时负责，所以同一份代码在仿真和真实环境里一样。
#![forbid(unsafe_code)]

use core::net::IpAddr;
use serde::{Deserialize, Serialize};

/// 服务名字的后缀：home 上的 web 是 `web.home.internal`。
pub const SERVICE_SUFFIX: &str = "internal";

/// 连接要去的主机。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Host {
    Name(String),
    Ip(IpAddr),
}

/// 连接要去的地方。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub host: Host,
    pub port: u16,
}

impl core::fmt::Display for Target {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.host {
            Host::Name(n) => write!(f, "{n}:{}", self.port),
            Host::Ip(IpAddr::V6(ip)) => write!(f, "[{ip}]:{}", self.port),
            Host::Ip(ip) => write!(f, "{ip}:{}", self.port),
        }
    }
}

impl Target {
    /// 解析 `主机:端口`（IPv6 写成 `[地址]:端口`）。
    pub fn parse(s: &str) -> Option<Self> {
        let (host, port) = if let Some(rest) = s.strip_prefix('[') {
            let (ip, port) = rest.split_once("]:")?;
            (Host::Ip(IpAddr::V6(ip.parse().ok()?)), port)
        } else {
            let (host, port) = s.rsplit_once(':')?;
            (host_of(host)?, port)
        };
        Some(Target { host, port: port.parse().ok()? })
    }
}

fn host_of(s: &str) -> Option<Host> {
    if s.is_empty() {
        return None;
    }
    Some(match s.parse::<IpAddr>() {
        Ok(ip) => Host::Ip(ip),
        Err(_) => Host::Name(s.to_string()),
    })
}

/// 入口上的协议，由连接的第一个字节决定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    Socks5,
    Http,
}

/// 按第一个字节判断：0x05 是 SOCKS5，大写字母开头的是 HTTP 方法；其他的不接受。
pub fn sniff(first: u8) -> Option<Entry> {
    match first {
        0x05 => Some(Entry::Socks5),
        b'A'..=b'Z' => Some(Entry::Http),
        _ => None,
    }
}

pub mod socks5 {
    //! SOCKS5（RFC 1928）服务端：只支持不认证和 CONNECT。入口只对本机开放，认证由 Nexus 的身份承担。
    use super::{Host, Target};
    use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    /// 回应码。
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Reply {
        Succeeded = 0,
        GeneralFailure = 1,
        /// 规则不允许。
        NotAllowed = 2,
        NetworkUnreachable = 3,
        HostUnreachable = 4,
        ConnectionRefused = 5,
        CommandNotSupported = 7,
        AddressTypeNotSupported = 8,
    }

    /// 接受「不认证」。
    pub const NO_AUTH: [u8; 2] = [5, 0];
    /// 没有可接受的认证方式。
    pub const NO_ACCEPTABLE: [u8; 2] = [5, 0xff];

    /// 问候：版本、方法数、方法。返回（消耗的字节，是否提供了不认证）；不完整时 `None`。
    pub fn parse_greeting(buf: &[u8]) -> Result<Option<(usize, bool)>, Reply> {
        let Some(&ver) = buf.first() else { return Ok(None) };
        if ver != 5 {
            return Err(Reply::GeneralFailure);
        }
        let Some(&n) = buf.get(1) else { return Ok(None) };
        let end = 2 + n as usize;
        if buf.len() < end {
            return Ok(None);
        }
        Ok(Some((end, buf[2..end].contains(&0))))
    }

    /// 请求：只接受 CONNECT。返回（消耗的字节，目标）；不完整时 `None`。
    pub fn parse_request(buf: &[u8]) -> Result<Option<(usize, Target)>, Reply> {
        if buf.len() < 4 {
            return Ok(None);
        }
        if buf[0] != 5 {
            return Err(Reply::GeneralFailure);
        }
        if buf[1] != 1 {
            return Err(Reply::CommandNotSupported);
        }
        let (host, at) = match buf[3] {
            1 => {
                let Some(b) = buf.get(4..8) else { return Ok(None) };
                (Host::Ip(IpAddr::V4(Ipv4Addr::new(b[0], b[1], b[2], b[3]))), 8)
            }
            3 => {
                let Some(&len) = buf.get(4) else { return Ok(None) };
                let end = 5 + len as usize;
                let Some(name) = buf.get(5..end) else { return Ok(None) };
                let name = core::str::from_utf8(name).map_err(|_| Reply::GeneralFailure)?;
                if name.is_empty() {
                    return Err(Reply::GeneralFailure);
                }
                (Host::Name(name.to_string()), end)
            }
            4 => {
                let Some(b) = buf.get(4..20) else { return Ok(None) };
                let octets: [u8; 16] = b.try_into().expect("长度已检查");
                (Host::Ip(IpAddr::V6(Ipv6Addr::from(octets))), 20)
            }
            _ => return Err(Reply::AddressTypeNotSupported),
        };
        let Some(p) = buf.get(at..at + 2) else { return Ok(None) };
        Ok(Some((at + 2, Target { host, port: u16::from_be_bytes([p[0], p[1]]) })))
    }

    /// 回应：绑定地址固定为 0.0.0.0:0（客户端用不上它）。
    pub fn reply(r: Reply) -> [u8; 10] {
        [5, r as u8, 0, 1, 0, 0, 0, 0, 0, 0]
    }
}

pub mod http {
    //! HTTP 代理：CONNECT 隧道，以及绝对形式的普通请求（改写成源站形式后转发）。
    use super::{host_of, Target};

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum ProxyRequest {
        /// `CONNECT 主机:端口`：回应 200 之后就是隧道。
        Connect(Target),
        /// `GET http://主机[:端口]/路径`：`head` 是改写后发给源站的请求头（含结尾的空行）。
        Forward { target: Target, head: Vec<u8> },
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct BadRequest(pub &'static str);

    /// CONNECT 成功的回应。
    pub const CONNECT_OK: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";

    /// 解析完整的请求头（到空行为止）。
    pub fn parse(head: &[u8]) -> Result<ProxyRequest, BadRequest> {
        let end = head.windows(4).position(|w| w == b"\r\n\r\n").ok_or(BadRequest("请求头不完整"))?;
        let text = core::str::from_utf8(&head[..end]).map_err(|_| BadRequest("请求头不是文本"))?;
        let mut lines = text.split("\r\n");
        let line = lines.next().unwrap_or_default();
        let mut parts = line.split(' ');
        let (method, uri, version) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(m), Some(u), Some(v), None) if !m.is_empty() && v.starts_with("HTTP/") => (m, u, v),
            _ => return Err(BadRequest("请求行不合法")),
        };
        if method == "CONNECT" {
            let target = Target::parse(uri).ok_or(BadRequest("CONNECT 的目标应当是 主机:端口"))?;
            return Ok(ProxyRequest::Connect(target));
        }
        let rest = uri.strip_prefix("http://").ok_or(BadRequest("代理请求要用绝对形式的 http:// 地址"))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => match rest.find('?') {
                Some(i) => (&rest[..i], &rest[i..]),
                None => (rest, "/"),
            },
        };
        let path = if path.starts_with('?') { format!("/{path}") } else { path.to_string() };
        let target = if authority.starts_with('[') || authority.rsplit_once(':').is_some_and(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())) {
            Target::parse(authority).ok_or(BadRequest("地址不合法"))?
        } else {
            Target { host: host_of(authority).ok_or(BadRequest("地址不合法"))?, port: 80 }
        };
        let mut out = format!("{method} {path} {version}\r\n");
        for l in lines {
            let name = l.split(':').next().unwrap_or_default().trim().to_ascii_lowercase();
            if name.starts_with("proxy-") {
                continue;
            }
            out.push_str(l);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&head[end + 4..]);
        Ok(ProxyRequest::Forward { target, head: bytes })
    }

    /// 一个简短的错误回应（之后关闭连接）。
    pub fn response(status: u16, reason: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }
}

pub mod route {
    //! 路由判定：`.internal` 名字是网络里的服务；其余按规则，第一条匹配的生效，都不匹配时直连。
    use super::{Host, Target, SERVICE_SUFFIX};
    use core::net::IpAddr;
    use serde::{Deserialize, Serialize};

    /// 规则匹配什么。
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Matcher {
        /// 这个域名和它的所有子域名（小写，不带结尾的点）。
        Domain(String),
        /// 一个地址段。
        Cidr { net: IpAddr, prefix: u8 },
    }

    impl Matcher {
        pub fn parse_domain(s: &str) -> Result<Self, String> {
            let d = s.trim_end_matches('.').to_ascii_lowercase();
            let ok = !d.is_empty()
                && d.split('.').all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
            if ok {
                Ok(Matcher::Domain(d))
            } else {
                Err(format!("域名不合法：{s}"))
            }
        }

        pub fn parse_cidr(s: &str) -> Result<Self, String> {
            let (ip, len) = s.split_once('/').ok_or_else(|| format!("地址段应当写成 地址/前缀长度：{s}"))?;
            let ip: IpAddr = ip.parse().map_err(|_| format!("地址不合法：{s}"))?;
            let max = if ip.is_ipv4() { 32 } else { 128 };
            let prefix: u8 = len.parse().ok().filter(|&p| p <= max).ok_or_else(|| format!("前缀长度不合法：{s}"))?;
            Ok(Matcher::Cidr { net: mask(ip, prefix), prefix })
        }

        pub fn matches(&self, t: &Target) -> bool {
            match (self, &t.host) {
                (Matcher::Domain(d), Host::Name(n)) => {
                    let n = normalize(n);
                    n == *d || n.ends_with(&format!(".{d}"))
                }
                (Matcher::Cidr { net, prefix }, Host::Ip(ip)) => {
                    net.is_ipv4() == ip.is_ipv4() && mask(*ip, *prefix) == *net
                }
                _ => false,
            }
        }
    }

    impl core::fmt::Display for Matcher {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Matcher::Domain(d) => write!(f, "{d}"),
                Matcher::Cidr { net, prefix } => write!(f, "{net}/{prefix}"),
            }
        }
    }

    fn mask(ip: IpAddr, prefix: u8) -> IpAddr {
        match ip {
            IpAddr::V4(v) => {
                let m = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
                IpAddr::V4((u32::from(v) & m).into())
            }
            IpAddr::V6(v) => {
                let m = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix) };
                IpAddr::V6((u128::from(v) & m).into())
            }
        }
    }

    fn normalize(name: &str) -> String {
        name.trim_end_matches('.').to_ascii_lowercase()
    }

    /// 连接从哪里出去。
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Outbound {
        Direct,
        Reject,
    }

    impl core::fmt::Display for Outbound {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Outbound::Direct => write!(f, "直连"),
                Outbound::Reject => write!(f, "拒绝"),
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Rule {
        pub matcher: Matcher,
        pub outbound: Outbound,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Decision {
        /// 网络里发布的服务。
        Service { device: String, service: String },
        /// 以 `.internal` 结尾，但不是「服务.设备.internal」。
        BadInternalName,
        /// 第 `index` 条规则（从 0 开始）。
        Rule { index: usize, outbound: Outbound },
        /// 没有规则匹配。
        Default(Outbound),
    }

    pub fn decide(target: &Target, rules: &[Rule]) -> Decision {
        if let Host::Name(n) = &target.host {
            let n = normalize(n);
            if n == SERVICE_SUFFIX || n.ends_with(&format!(".{SERVICE_SUFFIX}")) {
                let labels: Vec<&str> = n.split('.').collect();
                let ok = |l: &str| !l.is_empty() && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
                return match labels.as_slice() {
                    [service, device, _] if ok(service) && ok(device) => {
                        Decision::Service { device: device.to_string(), service: service.to_string() }
                    }
                    _ => Decision::BadInternalName,
                };
            }
        }
        match rules.iter().position(|r| r.matcher.matches(target)) {
            Some(index) => Decision::Rule { index, outbound: rules[index].outbound.clone() },
            None => Decision::Default(Outbound::Direct),
        }
    }
}

/// 路由规则表（本机保存）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteTable {
    pub rules: Vec<route::Rule>,
}
