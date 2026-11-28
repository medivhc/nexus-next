//! 页面里的终端：连会话、转发输入输出、确认、心跳、断线重连。页面只负责显示和收集按键。

use crate::node::WebNode;
use n0_future::time::{sleep_until, timeout, Duration, Instant};
use nexus_fabric::Transport;
use nexus_wire::{ClientMsg, ErrorCode, Input, SessionFrame, Size, WireError};
use std::future::Future;

/// 多久发一次心跳。和常驻程序之间的会话连接一致。
const HEARTBEAT: Duration = Duration::from_secs(5);
/// 这么久没收到对方的任何东西（对方每 5 秒发一次心跳），就认为连接已死。
const PEER_SILENT: Duration = Duration::from_secs(15);
/// 重连的间隔从 1 秒开始逐次加倍，最长这么久。
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// 页面一侧：显示和输入。
pub trait TerminalUi {
    /// 显示会话的输出。返回之后才向会话确认这段输出。
    fn output(&mut self, bytes: &[u8]);
    /// 连接状态的变化，页面自己决定怎么提示。
    fn notice(&mut self, notice: Notice);
    /// 等待下一个输入事件。必须可以安全地取消（放进 `select!`）。
    fn event(&mut self) -> impl Future<Output = UiEvent>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiEvent {
    /// 用户按下的键（已编码成终端的字节）。
    Keys(Vec<u8>),
    Resize(Size),
    /// 用户离开（关闭终端或页面）：会话继续运行。
    Leave,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notice {
    /// 连接中断，正在重连。
    Reconnecting,
    /// 重新连上了。
    Reconnected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalEnd {
    /// 会话里的 shell 退出了，带着它的退出码。
    Exited(Option<i32>),
    /// 用户离开了。
    Left,
}

enum Pumped {
    End(TerminalEnd),
    Lost,
}

impl<T: Transport> WebNode<T> {
    /// 连上（必要时创建）一台设备上的终端会话并运行，直到会话结束或用户离开。
    ///
    /// 连接中断时自动重连，不需要再用通行密钥；设备拒绝（例如通行密钥被撤销、登录过期）时结束并返回原因。
    /// 第一次就连不上时直接返回错误。
    pub async fn run_terminal(
        &self,
        device: &str,
        session: &str,
        size: Size,
        ui: &mut impl TerminalUi,
    ) -> Result<TerminalEnd, WireError> {
        let mut size = size;
        let mut connected_once = false;
        let mut outage = false;
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.attach(device, session, size).await {
                Ok(mut term) => {
                    connected_once = true;
                    backoff = Duration::from_secs(1);
                    if outage {
                        outage = false;
                        ui.notice(Notice::Reconnected);
                    }
                    match pump(&mut term, &mut size, ui).await {
                        Pumped::End(end) => return Ok(end),
                        Pumped::Lost => {}
                    }
                }
                Err(e) if e.code == ErrorCode::Unreachable && connected_once => {}
                Err(e) => return Err(e),
            }
            if !outage {
                outage = true;
                ui.notice(Notice::Reconnecting);
            }
            // 等一会儿再试；期间用户仍然可以离开或改变尺寸，按键丢弃（没有地方送）。
            let until = Instant::now() + backoff;
            backoff = (backoff * 2).min(MAX_BACKOFF);
            loop {
                tokio::select! {
                    _ = sleep_until(until) => break,
                    ev = ui.event() => match ev {
                        UiEvent::Leave => return Ok(TerminalEnd::Left),
                        UiEvent::Resize(s) => size = s,
                        UiEvent::Keys(_) => {}
                    },
                }
            }
        }
    }
}

/// 限时发送：写入卡住 [`PEER_SILENT`] 也算连接已死。
async fn send<S>(term: &mut crate::WebTerminal<S>, input: Input) -> bool
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    matches!(timeout(PEER_SILENT, term.send(input)).await, Ok(Ok(())))
}

async fn pump<S>(
    term: &mut crate::WebTerminal<S>,
    size: &mut Size,
    ui: &mut impl TerminalUi,
) -> Pumped
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut last_heard = Instant::now();
    let mut beat = Instant::now() + HEARTBEAT;
    loop {
        tokio::select! {
            biased;
            frame = term.next() => {
                last_heard = Instant::now();
                match frame {
                    Some(SessionFrame::Msg(ClientMsg::Exit(code))) => return Pumped::End(TerminalEnd::Exited(code)),
                    Some(SessionFrame::Msg(ClientMsg::Restore(b) | ClientMsg::Output(b))) => {
                        ui.output(&b);
                        if !send(term, Input::Ack(b.len() as u64)).await {
                            return Pumped::Lost;
                        }
                    }
                    Some(SessionFrame::Heartbeat) => {}
                    None => return Pumped::Lost,
                }
            }
            _ = sleep_until(last_heard + PEER_SILENT) => return Pumped::Lost,
            _ = sleep_until(beat) => {
                beat = Instant::now() + HEARTBEAT;
                if !send(term, Input::Ping).await {
                    return Pumped::Lost;
                }
            }
            ev = ui.event() => {
                let input = match ev {
                    UiEvent::Keys(k) if k.is_empty() => continue,
                    UiEvent::Keys(k) => Input::Stdin(k),
                    UiEvent::Resize(s) => {
                        *size = s;
                        Input::Resize(s)
                    }
                    UiEvent::Leave => {
                        if send(term, Input::Detach).await {
                            let _ = timeout(PEER_SILENT, term.close()).await;
                        }
                        return Pumped::End(TerminalEnd::Left);
                    }
                };
                if !send(term, input).await {
                    return Pumped::Lost;
                }
            }
        }
    }
}
