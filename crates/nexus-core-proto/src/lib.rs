//! 纯逻辑层：Nexus 的核心数据模型。
//!
//! 本 crate 不允许读取系统时间、随机数源、网络、线程或异步运行时。
//! 时间和随机数一律以参数形式传入。该约束由 `tools/check.py` 强制执行。
#![forbid(unsafe_code)]

use core::fmt;
use core::str::FromStr;
use serde::{Deserialize, Serialize};

pub mod authz;
pub mod crypto;
pub mod path;
pub mod roster;

pub use crypto::{PublicKey, SecretKey, Signature};

/// 节点标识，就是设备的 Ed25519 公钥。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub [u8; 32]);

impl NodeId {
    /// 作为公钥使用。
    pub fn public_key(&self) -> PublicKey {
        PublicKey(self.0)
    }

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
    /// 该节点持有的名册版本；尚未加入任何网络时为 `None`。
    pub roster_version: Option<u64>,
}

/// 到对端的路径类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathKind {
    Direct,
    Relay,
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
}
