//! 纯逻辑层：Nexus 的核心数据模型。
//!
//! 本 crate 不允许读取系统时间、随机数源、网络、线程或异步运行时。
//! 时间和随机数一律以参数形式传入。该约束由 `tools/check.py` 强制执行。
#![forbid(unsafe_code)]

use core::fmt;
use core::str::FromStr;
use serde::{Deserialize, Serialize};

/// 节点标识。阶段 1 起它等于设备公钥；M0 中是一段随机字节。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub [u8; 32]);

impl NodeId {
    /// 用于展示的短形式（前 8 个十六进制字符）。
    pub fn short(&self) -> String {
        self.to_string()[..8].to_string()
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.short())
    }
}

/// 解析节点标识失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseNodeIdError;

impl fmt::Display for ParseNodeIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("节点标识必须是 64 个十六进制字符")
    }
}

impl FromStr for NodeId {
    type Err = ParseNodeIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = s.as_bytes();
        if bytes.len() != 64 {
            return Err(ParseNodeIdError);
        }
        let mut out = [0u8; 32];
        for (i, chunk) in bytes.chunks(2).enumerate() {
            let hi = (chunk[0] as char).to_digit(16).ok_or(ParseNodeIdError)?;
            let lo = (chunk[1] as char).to_digit(16).ok_or(ParseNodeIdError)?;
            out[i] = (hi * 16 + lo) as u8;
        }
        Ok(NodeId(out))
    }
}

/// 自 Unix 纪元起的毫秒数。由 `System::now` 提供，纯逻辑层从不自行获取。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Timestamp(pub u64);

impl Timestamp {
    /// `self - earlier`，不会下溢。
    pub fn millis_since(&self, earlier: Timestamp) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// 一个节点对自己的描述。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    pub node_id: NodeId,
    pub name: String,
    pub version: String,
    pub started_at: Timestamp,
    /// 该节点生成这份自述时，它自己的时钟。
    pub now: Timestamp,
}

/// 到对端的路径类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathKind {
    Direct,
    Relay,
}

/// 在 M0 中代替名册的「已知节点」表：名字到节点标识。M1.2 起由名册取代。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerBook {
    pub peers: Vec<(String, NodeId)>,
}

impl PeerBook {
    /// 按名字或完整的十六进制标识查找。
    pub fn resolve(&self, name_or_id: &str) -> Option<NodeId> {
        if let Some((_, id)) = self.peers.iter().find(|(n, _)| n == name_or_id) {
            return Some(*id);
        }
        let id: NodeId = name_or_id.parse().ok()?;
        self.peers.iter().any(|(_, p)| *p == id).then_some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_text_round_trip() {
        let mut raw = [0u8; 32];
        for (i, b) in raw.iter_mut().enumerate() {
            *b = (i * 7 + 3) as u8;
        }
        let id = NodeId(raw);
        let text = id.to_string();
        assert_eq!(text.len(), 64);
        assert_eq!(text.parse::<NodeId>(), Ok(id));
    }

    #[test]
    fn node_id_rejects_bad_text() {
        assert!("abc".parse::<NodeId>().is_err());
        assert!("zz".repeat(32).parse::<NodeId>().is_err());
    }

    #[test]
    fn peer_book_resolves_name_and_id_but_not_strangers() {
        let a = NodeId([1; 32]);
        let stranger = NodeId([9; 32]);
        let book = PeerBook { peers: vec![("home".into(), a)] };
        assert_eq!(book.resolve("home"), Some(a));
        assert_eq!(book.resolve(&a.to_string()), Some(a));
        assert_eq!(book.resolve("office"), None);
        assert_eq!(book.resolve(&stranger.to_string()), None);
    }
}
