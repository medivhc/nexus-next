//! 临时分享的访客客户端（M9.1）：凭分享链接 `#share=<token>&node=<node_id>&hint=<hint>`
//! 直接在浏览器里只读查看文件/目录、使用已发布的网页服务或只读观看终端，不需要通行密钥。

use crate::http::{exchange, HttpResponse, WebHttpRequest};
use crate::terminal::{Notice, TerminalEnd, TerminalUi, UiEvent};
use crate::{call, unreachable};
use nexus_core_proto::authz::Principal;
use nexus_core_proto::NodeId;
use nexus_fabric::{read_response, write_msg, Connection, FrameReader, Proto, Transport};
use nexus_wire::{
    hex_decode, ClientMsg, ErrorCode, Input, Request, RequestHeader, Response, SessionFrame,
    ShareDriveEntry, ShareInfo, Size, WireError,
};
use tokio::io::AsyncWriteExt;

/// 分享链接里的信息：`…#share=<16字节hex>&node=<设备标识>&hint=<地址提示>`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareLink {
    pub token: [u8; 16],
    pub node: NodeId,
    pub hint: String,
}

impl ShareLink {
    pub fn parse(link: &str) -> Result<Self, String> {
        let fragment = link.rsplit_once('#').map_or(link, |(_, f)| f);
        let field = |key: &str| {
            fragment
                .split('&')
                .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
                .ok_or_else(|| format!("分享链接里缺少 {key}"))
        };
        let token: [u8; 16] = hex_decode(field("share")?)
            .and_then(|b| b.try_into().ok())
            .ok_or("分享码格式不对")?;
        let node: NodeId = field("node")?.parse().map_err(|_| "设备标识格式不对")?;
        Ok(Self {
            token,
            node,
            hint: field("hint")?.to_string(),
        })
    }
}

/// 一个已打开的访客分享会话（全内存，关闭页面即消失，遵守 R32）。
pub struct GuestShare<T> {
    transport: T,
    node: NodeId,
    token: [u8; 16],
    info: ShareInfo,
}

impl<T: Transport> GuestShare<T> {
    /// 打开分享链接，校验并扣减一次可用次数，返回分享会话。
    pub async fn open(transport: T, link: &ShareLink) -> Result<Self, WireError> {
        transport.add_hint(link.node, &link.hint);
        let body = Request::ShareOpen { token: link.token };
        match call(&transport, link.node, Proto::Web, Principal::Device, body).await? {
            Response::ShareOpened(info) => Ok(Self {
                transport,
                node: link.node,
                token: link.token,
                info,
            }),
            Response::Error(e) => Err(e),
            other => Err(WireError::new(
                ErrorCode::Internal,
                format!("分享设备的回应无法理解：{other:?}"),
            )),
        }
    }

    pub fn info(&self) -> &ShareInfo {
        &self.info
    }

    /// 列出分享目录下的子路径（相对分享根路径）。
    pub async fn drive_list(&self, sub_path: &str) -> Result<Vec<ShareDriveEntry>, WireError> {
        let body = Request::ShareDriveList {
            token: self.token,
            path: sub_path.to_string(),
        };
        match call(
            &self.transport,
            self.node,
            Proto::Web,
            Principal::Device,
            body,
        )
        .await?
        {
            Response::ShareDriveEntries(entries) => Ok(entries),
            Response::Error(e) => Err(e),
            other => Err(WireError::new(
                ErrorCode::Internal,
                format!("回应无法理解：{other:?}"),
            )),
        }
    }

    /// 读取分享文件（或分享目录下的相对子路径）最多 `max` 字节。
    pub async fn drive_read(
        &self,
        sub_path: &str,
        max: u64,
    ) -> Result<(u64, Vec<u8>, bool), WireError> {
        let body = Request::ShareDriveRead {
            token: self.token,
            path: sub_path.to_string(),
            max,
        };
        match call(
            &self.transport,
            self.node,
            Proto::Web,
            Principal::Device,
            body,
        )
        .await?
        {
            Response::ShareDriveFile {
                size,
                bytes,
                truncated,
            } => Ok((size, bytes, truncated)),
            Response::Error(e) => Err(e),
            other => Err(WireError::new(
                ErrorCode::Internal,
                format!("回应无法理解：{other:?}"),
            )),
        }
    }

    /// 通过分享令牌向被分享的 HTTP 服务发送一条 HTTP/1.1 请求。
    pub async fn http_fetch(&self, req: &WebHttpRequest) -> Result<HttpResponse, WireError> {
        let conn = self
            .transport
            .connect(self.node, Proto::Web)
            .await
            .map_err(unreachable)?;
        let mut stream = conn.open_bi().await.map_err(unreachable)?;
        let body = Request::ShareServiceConnect { token: self.token };
        let header = RequestHeader::new(0, Principal::Device, &body);
        write_msg(&mut stream, &header).await.map_err(unreachable)?;
        match read_response(&mut stream).await.map_err(unreachable)? {
            Response::Ok => {}
            Response::Error(e) => return Err(e),
            other => {
                return Err(WireError::new(
                    ErrorCode::Internal,
                    format!("回应无法理解：{other:?}"),
                ))
            }
        }
        exchange(&mut stream, req, &format!("分享 {}", self.info.id)).await
    }

    /// 只读观看被分享的终端会话：输出喂给 `ui`，忽略本地按键输入。
    pub async fn watch_terminal<U: TerminalUi>(
        &self,
        size: Size,
        ui: &mut U,
    ) -> Result<TerminalEnd, WireError> {
        let conn = self
            .transport
            .connect(self.node, Proto::Web)
            .await
            .map_err(unreachable)?;
        let mut stream = conn.open_bi().await.map_err(unreachable)?;
        let body = Request::ShareSessionWatch {
            token: self.token,
            size,
        };
        let header = RequestHeader::new(0, Principal::Device, &body);
        write_msg(&mut stream, &header).await.map_err(unreachable)?;
        match read_response(&mut stream).await.map_err(unreachable)? {
            Response::Ok => {}
            Response::Error(e) => return Err(e),
            other => {
                return Err(WireError::new(
                    ErrorCode::Internal,
                    format!("回应无法理解：{other:?}"),
                ))
            }
        }
        let (rd, mut writer) = tokio::io::split(stream);
        let mut reader = FrameReader::new(rd);
        let _ = Notice::Reconnected;
        loop {
            tokio::select! {
                biased;
                frame = reader.next::<SessionFrame>() => match frame {
                    Ok(Some(SessionFrame::Heartbeat)) => {
                        let _ = write_msg(&mut writer, &Input::Ping).await;
                    }
                    Ok(Some(SessionFrame::Msg(ClientMsg::Restore(b) | ClientMsg::Output(b)))) => {
                        ui.output(&b);
                        let _ = write_msg(&mut writer, &Input::Ack(b.len() as u64)).await;
                    }
                    Ok(Some(SessionFrame::Msg(ClientMsg::Exit(code)))) => {
                        let _ = writer.shutdown().await;
                        return Ok(TerminalEnd::Exited(code));
                    }
                    Ok(None) | Err(_) => {
                        return Ok(TerminalEnd::Exited(None));
                    }
                },
                ev = ui.event() => match ev {
                    UiEvent::Leave => {
                        let _ = write_msg(&mut writer, &Input::Detach).await;
                        let _ = writer.shutdown().await;
                        return Ok(TerminalEnd::Left);
                    }
                    // 只读观看：按键和调整大小在客户端与服务端双重忽略。
                    UiEvent::Keys(_) | UiEvent::Resize(_) => {}
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_links_parse() {
        let node = NodeId([9; 32]);
        let link = format!(
            "https://bewith.link/#share={}&node={node}&hint=127.0.0.1:1999",
            "a1".repeat(16)
        );
        let parsed = ShareLink::parse(&link).unwrap();
        assert_eq!(
            parsed,
            ShareLink {
                token: [0xa1; 16],
                node,
                hint: "127.0.0.1:1999".into()
            }
        );
        assert!(ShareLink::parse("https://bewith.link/#enroll=00").is_err());
    }
}
