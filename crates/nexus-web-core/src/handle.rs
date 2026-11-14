//! 通行密钥的用户标识里存的入口信息：页面在陌生电脑上不需要用户输入任何东西就知道该连谁。
//!
//! 格式：入口设备的节点标识（32 字节），然后可选的中继：1 字节方案（1 = https，2 = http），
//! 接着是中继的主机名（可带端口）。WebAuthn 限制用户标识最多 64 字节，所以主机名最多 31 字节。

use nexus_core_proto::NodeId;

/// 用户标识的上限（WebAuthn 规定）。
pub const MAX_USER_HANDLE: usize = 64;

/// 入口：设备和它所用的中继（没有时由页面决定）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryHandle {
    pub node: NodeId,
    pub relay: Option<String>,
}

impl EntryHandle {
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let mut out = self.node.0.to_vec();
        if let Some(relay) = &self.relay {
            let (scheme, rest) = if let Some(r) = relay.strip_prefix("https://") {
                (1u8, r)
            } else if let Some(r) = relay.strip_prefix("http://") {
                (2u8, r)
            } else {
                return Err(format!("中继地址应当以 https:// 或 http:// 开头：{relay}"));
            };
            let host = rest.trim_end_matches('/');
            if host.is_empty() || host.contains('/') {
                return Err(format!("中继地址只能是 方案://主机名[:端口]：{relay}"));
            }
            out.push(scheme);
            out.extend_from_slice(host.as_bytes());
        }
        if out.len() > MAX_USER_HANDLE {
            return Err(format!("中继地址太长，放不进通行密钥（主机名最多 {} 字节）", MAX_USER_HANDLE - 33));
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 32 || bytes.len() > MAX_USER_HANDLE {
            return None;
        }
        let node = NodeId(bytes[..32].try_into().ok()?);
        let relay = match bytes.get(32) {
            None => None,
            Some(&scheme) => {
                let scheme = match scheme {
                    1 => "https",
                    2 => "http",
                    _ => return None,
                };
                let host = std::str::from_utf8(&bytes[33..]).ok()?;
                if host.is_empty() || host.contains('/') {
                    return None;
                }
                Some(format!("{scheme}://{host}/"))
            }
        };
        Some(Self { node, relay })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_handles_round_trip_within_the_webauthn_limit() {
        let node = NodeId([9; 32]);
        for (relay, back) in [
            (None, None),
            (Some("https://bewith.link"), Some("https://bewith.link/")),
            (Some("https://bewith.link/"), Some("https://bewith.link/")),
            (Some("http://127.0.0.1:3340"), Some("http://127.0.0.1:3340/")),
        ] {
            let h = EntryHandle { node, relay: relay.map(String::from) };
            let bytes = h.encode().unwrap();
            assert!(bytes.len() <= MAX_USER_HANDLE);
            assert_eq!(EntryHandle::decode(&bytes), Some(EntryHandle { node, relay: back.map(String::from) }));
        }
        let long = EntryHandle { node, relay: Some(format!("https://{}", "a".repeat(32))) };
        assert!(long.encode().is_err());
        let ok = EntryHandle { node, relay: Some(format!("https://{}", "a".repeat(31))) };
        assert_eq!(ok.encode().unwrap().len(), 64);
        for bad in ["ftp://x", "https://", "https://a/b"] {
            assert!(EntryHandle { node, relay: Some(bad.into()) }.encode().is_err(), "{bad}");
        }
        assert_eq!(EntryHandle::decode(&[0; 31]), None);
        let mut bad = vec![0u8; 32];
        bad.push(3);
        bad.extend_from_slice(b"x");
        assert_eq!(EntryHandle::decode(&bad), None);
        let mut no_host = vec![0u8; 32];
        no_host.push(1);
        assert_eq!(EntryHandle::decode(&no_host), None);
    }
}
