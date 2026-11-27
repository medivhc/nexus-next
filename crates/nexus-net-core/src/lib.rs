//! 纯逻辑：本机统一入口的协议解析（SOCKS5、HTTP 代理）与路由判定。
//!
//! 这里只处理字节和规则，不碰网络：读写由运行时负责，所以同一份代码在仿真和真实环境里一样。
#![forbid(unsafe_code)]

pub mod app_policy;
pub mod diag;
pub mod dns_policy;
pub mod import;
pub mod policy;
pub mod power;

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

    /// 作为客户端连上游代理时的请求（CONNECT）。名字原样交给上游，不在本机解析。
    pub fn request(target: &Target) -> Vec<u8> {
        let mut out = vec![5, 1, 0];
        match &target.host {
            Host::Ip(IpAddr::V4(ip)) => {
                out.push(1);
                out.extend_from_slice(&ip.octets());
            }
            Host::Ip(IpAddr::V6(ip)) => {
                out.push(4);
                out.extend_from_slice(&ip.octets());
            }
            Host::Name(name) => {
                out.push(3);
                out.push(name.len() as u8);
                out.extend_from_slice(name.as_bytes());
            }
        }
        out.extend_from_slice(&target.port.to_be_bytes());
        out
    }

    /// 上游代理的回应。返回（消耗的字节，回应码）；不完整时 `None`；无法理解时 `Err`。
    /// 上游代理的回应无法理解。
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct BadReply;

    pub fn parse_reply(buf: &[u8]) -> Result<Option<(usize, u8)>, BadReply> {
        if buf.len() < 5 {
            return Ok(None);
        }
        if buf[0] != 5 {
            return Err(BadReply);
        }
        let len = match buf[3] {
            1 => 4 + 4 + 2,
            4 => 4 + 16 + 2,
            3 => 4 + 1 + buf[4] as usize + 2,
            _ => return Err(BadReply),
        };
        Ok((buf.len() >= len).then_some((len, buf[1])))
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
        /// 仅精确匹配该域名本身，不匹配子域名（小写，不带结尾的点）。
        DomainExact(String),
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

        pub fn parse_domain_exact(s: &str) -> Result<Self, String> {
            match Self::parse_domain(s)? {
                Matcher::Domain(d) => Ok(Matcher::DomainExact(d)),
                other => Ok(other),
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
                (Matcher::DomainExact(d), Host::Name(n)) => normalize(n) == *d,
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
                Matcher::DomainExact(d) => write!(f, "exact:{d}"),
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
        /// 经网络里的这台设备出去。
        Exit(String),
        /// 交给这个上游 SOCKS5 代理（主机:端口）。
        Socks5(String),
    }

    impl core::fmt::Display for Outbound {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Outbound::Direct => write!(f, "直连"),
                Outbound::Reject => write!(f, "拒绝"),
                Outbound::Exit(device) => write!(f, "经 {device} 出去"),
                Outbound::Socks5(upstream) => write!(f, "经上游代理 {upstream}"),
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

    /// 静态规则审计发现的问题类型。
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum AuditKind {
        /// 与前序规则匹配器完全重复，永不可达。
        Duplicate,
        /// 被更宽的前序规则遮蔽，永不可达。
        Shadowed,
        /// 出口或上游代理配置无效。
        InvalidUpstream,
    }

    /// 单条审计问题（规则编号为 1-based，与 `nexus route ls` 一致）。
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct AuditIssue {
        pub rule_index: usize,
        pub kind: AuditKind,
        pub shadowed_by: Option<usize>,
        pub message: String,
    }

    /// 规则表的静态编译审计报告。
    #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct RuleAudit {
        pub issues: Vec<AuditIssue>,
    }

    impl RuleAudit {
        pub fn is_clean(&self) -> bool {
            self.issues.is_empty()
        }
    }

    /// 检查规则表中的重复规则、被前序宽规则遮蔽的不可达规则以及无效上游地址。
    pub fn audit_rules(rules: &[Rule]) -> RuleAudit {
        let mut issues = Vec::new();
        for (i, rule) in rules.iter().enumerate() {
            let idx = i + 1;
            match &rule.outbound {
                Outbound::Exit(dev) if dev.trim().is_empty() => {
                    issues.push(AuditIssue {
                        rule_index: idx,
                        kind: AuditKind::InvalidUpstream,
                        shadowed_by: None,
                        message: format!("第 {idx} 条规则的出口设备名为空"),
                    });
                }
                Outbound::Socks5(addr) if Target::parse(addr).is_none() => {
                    issues.push(AuditIssue {
                        rule_index: idx,
                        kind: AuditKind::InvalidUpstream,
                        shadowed_by: None,
                        message: format!("第 {idx} 条规则的上游 SOCKS5 地址不合法：{addr}"),
                    });
                }
                _ => {}
            }
            for (j, prev) in rules[..i].iter().enumerate() {
                let prev_idx = j + 1;
                if prev.matcher == rule.matcher {
                    issues.push(AuditIssue {
                        rule_index: idx,
                        kind: AuditKind::Duplicate,
                        shadowed_by: Some(prev_idx),
                        message: format!("第 {idx} 条规则（{}）与第 {prev_idx} 条完全重复，永不生效", rule.matcher),
                    });
                    break;
                }
                if matcher_covers(&prev.matcher, &rule.matcher) {
                    issues.push(AuditIssue {
                        rule_index: idx,
                        kind: AuditKind::Shadowed,
                        shadowed_by: Some(prev_idx),
                        message: format!("第 {idx} 条规则（{}）被第 {prev_idx} 条（{}）完全遮蔽，永不可达", rule.matcher, prev.matcher),
                    });
                    break;
                }
            }
        }
        RuleAudit { issues }
    }

    fn matcher_covers(broad: &Matcher, narrow: &Matcher) -> bool {
        match (broad, narrow) {
            (Matcher::Domain(b), Matcher::Domain(n)) | (Matcher::Domain(b), Matcher::DomainExact(n)) => {
                n == b || n.ends_with(&format!(".{b}"))
            }
            (
                Matcher::Cidr { net: b_net, prefix: b_pref },
                Matcher::Cidr { net: n_net, prefix: n_pref },
            ) => b_net.is_ipv4() == n_net.is_ipv4() && b_pref <= n_pref && mask(*n_net, *b_pref) == *b_net,
            _ => false,
        }
    }
}

/// 路由规则表（本机保存）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteTable {
    pub rules: Vec<route::Rule>,
}

/// 完整节点的名字服务用到的 DNS 报文：只回答一个问题的普通查询，回答里只有地址记录。
pub mod dns {
    use core::net::IpAddr;

    pub const A: u16 = 1;
    pub const AAAA: u16 = 28;
    pub const SERVFAIL: u8 = 2;
    pub const NXDOMAIN: u8 = 3;
    pub const NOTIMP: u8 = 4;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Query {
        pub id: u16,
        /// 小写，没有末尾的点。
        pub name: String,
        pub qtype: u16,
        /// 查询方是否要求递归（回应里原样带回）。
        recursion: bool,
        /// 问题段的原始字节（回应里原样带回）。
        question: Vec<u8>,
    }

    /// 一个查询；回应、多个问题、截断或不认识的报文返回 `None`。
    pub fn parse_query(buf: &[u8]) -> Option<Query> {
        if buf.len() < 12 {
            return None;
        }
        let flags = u16::from_be_bytes([buf[2], buf[3]]);
        let opcode = (flags >> 11) & 0xf;
        if flags & 0x8000 != 0 || opcode != 0 || buf[4..6] != [0, 1] {
            return None;
        }
        let mut pos = 12;
        let mut labels: Vec<String> = Vec::new();
        loop {
            let len = *buf.get(pos)? as usize;
            pos += 1;
            if len == 0 {
                break;
            }
            if len > 63 {
                return None; // 问题里不应当有压缩指针
            }
            let label = buf.get(pos..pos + len)?;
            labels.push(core::str::from_utf8(label).ok()?.to_ascii_lowercase());
            pos += len;
            if pos > 12 + 255 {
                return None;
            }
        }
        let tail = buf.get(pos..pos + 4)?;
        Some(Query {
            id: u16::from_be_bytes([buf[0], buf[1]]),
            name: labels.join("."),
            qtype: u16::from_be_bytes([tail[0], tail[1]]),
            recursion: flags & 0x0100 != 0,
            question: buf[12..pos + 4].to_vec(),
        })
    }

    fn header(q: &Query, rcode: u8, answers: u16) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + q.question.len());
        out.extend_from_slice(&q.id.to_be_bytes());
        // 回应、权威、可以递归；保留查询方的「要求递归」。
        out.push(0x84 | u8::from(q.recursion));
        out.push(0x80 | (rcode & 0x0f));
        out.extend_from_slice(&[0, 1]);
        out.extend_from_slice(&answers.to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&q.question);
        out
    }

    /// 回答：只带和问题类型一致的地址；没有时是没有回答的 NOERROR。
    pub fn answer(q: &Query, ips: &[IpAddr], ttl: u32) -> Vec<u8> {
        let records: Vec<&IpAddr> = ips
            .iter()
            .filter(|ip| matches!((ip, q.qtype), (IpAddr::V4(_), A) | (IpAddr::V6(_), AAAA)))
            .take(16)
            .collect();
        let mut out = header(q, 0, records.len() as u16);
        for ip in records {
            out.extend_from_slice(&[0xc0, 12]);
            out.extend_from_slice(&q.qtype.to_be_bytes());
            out.extend_from_slice(&[0, 1]);
            out.extend_from_slice(&ttl.to_be_bytes());
            match ip {
                IpAddr::V4(v4) => {
                    out.extend_from_slice(&[0, 4]);
                    out.extend_from_slice(&v4.octets());
                }
                IpAddr::V6(v6) => {
                    out.extend_from_slice(&[0, 16]);
                    out.extend_from_slice(&v6.octets());
                }
            }
        }
        out
    }

    /// 出错的回应（例如 NXDOMAIN）。
    pub fn error(q: &Query, rcode: u8) -> Vec<u8> {
        header(q, rcode, 0)
    }
}

/// 完整节点给名字分配的本机临时地址：同一个名字总是同一个地址，地址可以反查名字；
/// 用完时重新用最早分配的地址。网段的第一个地址留给网段本身，第二个给本机一侧，第三个给名字服务，最后一个是广播。
pub mod fakeip {
    use std::collections::BTreeMap;
    use core::net::Ipv4Addr;

    #[derive(Clone, Debug)]
    pub struct FakePool {
        base: u32,
        prefix: u8,
        /// 可分配地址的个数（从 base + 3 开始）。
        size: u32,
        next: u32,
        by_name: BTreeMap<String, u32>,
        by_ip: BTreeMap<u32, String>,
    }

    impl FakePool {
        /// 网段要从起点给出，前缀在 8 到 29 之间。
        pub fn new(net: Ipv4Addr, prefix: u8) -> Option<Self> {
            if !(8..=29).contains(&prefix) {
                return None;
            }
            let base = u32::from(net);
            let mask = u32::MAX << (32 - prefix);
            if base & !mask != 0 {
                return None;
            }
            let size = (1u32 << (32 - prefix)) - 4;
            Some(Self { base, prefix, size, next: 0, by_name: BTreeMap::new(), by_ip: BTreeMap::new() })
        }

        pub fn network(&self) -> (Ipv4Addr, u8) {
            (Ipv4Addr::from(self.base), self.prefix)
        }

        /// 本机一侧（虚拟网卡）的地址。
        pub fn gateway(&self) -> Ipv4Addr {
            Ipv4Addr::from(self.base + 1)
        }

        /// 名字服务的地址。
        pub fn dns(&self) -> Ipv4Addr {
            Ipv4Addr::from(self.base + 2)
        }

        pub fn contains(&self, ip: Ipv4Addr) -> bool {
            let ip = u32::from(ip);
            ip >= self.base + 3 && ip < self.base + 3 + self.size
        }

        pub fn assign(&mut self, name: &str) -> Ipv4Addr {
            if let Some(&ip) = self.by_name.get(name) {
                return Ipv4Addr::from(ip);
            }
            let ip = self.base + 3 + self.next;
            self.next = (self.next + 1) % self.size;
            if let Some(old) = self.by_ip.remove(&ip) {
                self.by_name.remove(&old);
            }
            self.by_name.insert(name.into(), ip);
            self.by_ip.insert(ip, name.into());
            Ipv4Addr::from(ip)
        }

        pub fn lookup(&self, ip: Ipv4Addr) -> Option<&str> {
            self.by_ip.get(&u32::from(ip)).map(String::as_str)
        }
    }
}
