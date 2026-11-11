//! 纯逻辑层：线上消息与帧格式。
//!
//! 帧 = 4 字节小端长度 + postcard 编码的消息。本 crate 只做编解码，不做 IO。
#![forbid(unsafe_code)]

use nexus_core_proto::{NodeInfo, PathKind};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// 节点间协议版本。
pub const WIRE_VERSION: u16 = 1;
/// 单帧上限。超过即视为协议错误。
pub const MAX_FRAME: usize = 1 << 20;

/// 节点间请求。每条双向流承载一次调用。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestHeader {
    pub version: u16,
    pub body: Request,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    NodeInfo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    NodeInfo(NodeInfo),
    Error(WireError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    UnsupportedVersion,
    UnknownNode,
    Unreachable,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub code: ErrorCode,
    pub message: String,
}

/// 本机控制通道：命令行发给常驻程序的请求。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CtlRequest {
    /// 本机节点的状态。
    Status,
    /// 另一个节点的状态，按名字或标识指定。
    RemoteStatus { node: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CtlResponse {
    Status(StatusReport),
    Error(WireError),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    /// 在线时是对端的自述；离线时只有本机已知的名字和标识。
    pub name: String,
    pub node_id: nexus_core_proto::NodeId,
    pub info: Option<NodeInfo>,
    pub online: bool,
    pub rtt_ms: Option<u64>,
    pub path: Option<PathKind>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    TooLarge(usize),
    Malformed,
}

impl core::fmt::Display for FrameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FrameError::TooLarge(n) => write!(f, "帧长度 {n} 超过上限 {MAX_FRAME}"),
            FrameError::Malformed => f.write_str("帧内容无法解码"),
        }
    }
}

impl std::error::Error for FrameError {}

/// 把一条消息编码成一帧。
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let body = postcard::to_allocvec(msg).map_err(|_| FrameError::Malformed)?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// 解析帧头，返回帧体长度。
pub fn body_len(header: [u8; 4]) -> Result<usize, FrameError> {
    let n = u32::from_le_bytes(header) as usize;
    if n > MAX_FRAME {
        return Err(FrameError::TooLarge(n));
    }
    Ok(n)
}

/// 解码帧体。
pub fn decode_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, FrameError> {
    postcard::from_bytes(body).map_err(|_| FrameError::Malformed)
}

/// 增量解码器：可以分多次喂入字节。
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// 缓冲里有完整的一帧时取出并解码，否则返回 `None`。
    pub fn next_frame<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FrameError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let n = body_len([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])?;
        if self.buf.len() < 4 + n {
            return Ok(None);
        }
        let msg = decode_body(&self.buf[4..4 + n])?;
        self.buf.drain(..4 + n);
        Ok(Some(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core_proto::{NodeId, Timestamp};

    fn sample() -> Response {
        Response::NodeInfo(NodeInfo {
            node_id: NodeId([7; 32]),
            name: "home".into(),
            version: "0.0.1".into(),
            started_at: Timestamp(1_700_000_000_000),
            now: Timestamp(1_700_000_000_500),
        })
    }

    #[test]
    fn frame_round_trip() {
        let frame = encode(&sample()).unwrap();
        let mut dec = FrameDecoder::default();
        dec.push(&frame);
        assert_eq!(dec.next_frame::<Response>().unwrap(), Some(sample()));
        assert_eq!(dec.next_frame::<Response>().unwrap(), None);
    }

    #[test]
    fn decoder_handles_byte_at_a_time_and_back_to_back_frames() {
        let mut bytes = encode(&sample()).unwrap();
        bytes.extend(encode(&RequestHeader { version: WIRE_VERSION, body: Request::NodeInfo }).unwrap());
        let mut dec = FrameDecoder::default();
        let mut got_first = false;
        let mut got_second = false;
        for b in bytes {
            dec.push(&[b]);
            if !got_first {
                got_first = dec.next_frame::<Response>().unwrap().is_some();
            } else if !got_second {
                got_second = dec.next_frame::<RequestHeader>().unwrap().is_some();
            }
        }
        assert!(got_first && got_second);
    }

    #[test]
    fn oversized_and_garbage_frames_are_errors_not_panics() {
        let mut dec = FrameDecoder::default();
        dec.push(&u32::MAX.to_le_bytes());
        assert!(matches!(dec.next_frame::<Response>(), Err(FrameError::TooLarge(_))));

        let mut dec = FrameDecoder::default();
        dec.push(&3u32.to_le_bytes());
        dec.push(&[0xff, 0xff, 0xff]);
        assert_eq!(dec.next_frame::<Response>(), Err(FrameError::Malformed));
    }
}
