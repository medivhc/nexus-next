//! 网页节点发布硬件能力（M8.3）：摄像头、麦克风、位置。
//!
//! 每项能力默认关闭，必须由浏览器用户在页面上显式同意后才会对网络内受信任设备开放；
//! 每次远端访问都会通知页面「谁在用」，关闭页面或撤回同意后立即停止。

use crate::WebNode;
use nexus_core_proto::NodeId;
use nexus_fabric::{read_msg, write_response, Connection, Proto, Transport};
use nexus_wire::{
    ErrorCode, HardwareCapture, HardwareKind, Request, RequestHeader, Response, WireError,
};
use std::collections::BTreeSet;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;

/// 一次硬件访问的状态记录（用于在页面展示「谁在用」与最近使用记录）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HardwareUsageEvent {
    pub caller: String,
    pub kind: HardwareKind,
    pub active: bool,
}

/// 浏览器侧提供的硬件采集实现（纯异步 trait，不耦合浏览器 API，便于仿真与真实测试复用）。
pub trait HardwareProvider {
    fn capture(
        &self,
        kind: HardwareKind,
    ) -> impl Future<Output = Result<HardwareCapture, WireError>>;
}

/// 网页节点硬件授权与使用状态（全内存，不落任何浏览器存储，遵守 R32）。
#[derive(Clone, Default)]
pub struct HardwareState {
    inner: Arc<Mutex<HardwareStateInner>>,
}

#[derive(Default)]
struct HardwareStateInner {
    enabled: BTreeSet<HardwareKind>,
    active: Vec<(String, HardwareKind)>,
    history: Vec<HardwareUsageEvent>,
}

impl HardwareState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 显式开启某项硬件能力（需经过用户在页面点击确认）。
    pub fn enable(&self, kind: HardwareKind) {
        self.inner.lock().unwrap().enabled.insert(kind);
    }

    /// 关闭某项硬件能力（立即生效）。
    pub fn disable(&self, kind: HardwareKind) {
        self.inner.lock().unwrap().enabled.remove(&kind);
    }

    /// 清空全部授权与活跃状态（离开或关闭页面时调用）。
    pub fn clear(&self) {
        let mut g = self.inner.lock().unwrap();
        g.enabled.clear();
        g.active.clear();
    }

    /// 当前已显式同意的能力列表（按固定顺序）。
    pub fn enabled_list(&self) -> Vec<HardwareKind> {
        self.inner.lock().unwrap().enabled.iter().copied().collect()
    }

    /// 某项能力当前是否已获同意。
    pub fn is_enabled(&self, kind: HardwareKind) -> bool {
        self.inner.lock().unwrap().enabled.contains(&kind)
    }

    /// 当前正在使用硬件的调用方列表。
    pub fn active_users(&self) -> Vec<(String, HardwareKind)> {
        self.inner.lock().unwrap().active.clone()
    }

    /// 历史使用事件记录（按发生顺序）。
    pub fn history(&self) -> Vec<HardwareUsageEvent> {
        self.inner.lock().unwrap().history.clone()
    }

    fn begin_use(&self, caller: &str, kind: HardwareKind) {
        let mut g = self.inner.lock().unwrap();
        g.active.push((caller.to_string(), kind));
        g.history.push(HardwareUsageEvent { caller: caller.to_string(), kind, active: true });
    }

    fn end_use(&self, caller: &str, kind: HardwareKind) {
        let mut g = self.inner.lock().unwrap();
        if let Some(pos) = g.active.iter().position(|(c, k)| c == caller && *k == kind) {
            g.active.remove(pos);
        }
        g.history.push(HardwareUsageEvent { caller: caller.to_string(), kind, active: false });
    }
}

impl<T: Transport> WebNode<T> {
    /// 校验入站连接对端是否属于当前会话受信任的设备，并返回设备名。
    pub(crate) fn trusted_caller_name(&self, peer: NodeId) -> Option<String> {
        self.devices().iter().find(|d| d.node_id == peer).map(|d| d.name.clone())
    }

    /// 处理单条来自远端设备的硬件能力请求。
    pub async fn handle_hardware_request<P: HardwareProvider>(
        &self,
        peer: NodeId,
        req: Request,
        state: &HardwareState,
        provider: &P,
        on_usage: &mut impl FnMut(HardwareUsageEvent),
    ) -> Response {
        let Some(caller) = self.trusted_caller_name(peer) else {
            return Response::Error(WireError::new(
                ErrorCode::Denied,
                "未受信任的设备不能访问网页节点的硬件能力",
            ));
        };
        match req {
            Request::HardwareList => Response::HardwareList(state.enabled_list()),
            Request::HardwareCapture { kind } => {
                if !state.is_enabled(kind) {
                    return Response::Error(WireError::new(
                        ErrorCode::Denied,
                        format!("浏览器未同意开放硬件能力：{}", kind.as_str()),
                    ));
                }
                state.begin_use(&caller, kind);
                on_usage(HardwareUsageEvent { caller: caller.clone(), kind, active: true });
                let result = provider.capture(kind).await;
                state.end_use(&caller, kind);
                on_usage(HardwareUsageEvent { caller, kind, active: false });
                match result {
                    Ok(cap) => Response::HardwareCapture(cap),
                    Err(e) => Response::Error(e),
                }
            }
            _ => Response::Error(WireError::new(ErrorCode::Denied, "网页节点只接受硬件能力请求")),
        }
    }

    /// 接受入站 `Proto::Rpc` 连接并在每条复用连接上持续处理多条硬件能力请求流，直到 `stop` 收到信号。
    pub async fn serve_hardware<P: HardwareProvider>(
        &self,
        state: HardwareState,
        provider: P,
        mut stop: tokio::sync::oneshot::Receiver<()>,
        on_usage: impl FnMut(HardwareUsageEvent),
    ) {
        use std::cell::RefCell;
        use std::pin::Pin;
        use std::task::Poll;

        let on_usage = RefCell::new(on_usage);
        let mut tasks: Vec<Pin<Box<dyn Future<Output = ()> + '_>>> = Vec::new();
        let mut accept = Box::pin(self.transport().accept());
        let state_ref = &state;
        let provider_ref = &provider;
        let on_usage_ref = &on_usage;

        loop {
            let has_tasks = !tasks.is_empty();
            let mut poll_tasks = std::future::poll_fn(|cx| {
                let mut progressed = false;
                let mut i = 0;
                while i < tasks.len() {
                    if let Poll::Ready(()) = tasks[i].as_mut().poll(cx) {
                        drop(tasks.swap_remove(i));
                        progressed = true;
                    } else {
                        i += 1;
                    }
                }
                if progressed { Poll::Ready(()) } else { Poll::Pending }
            });
            tokio::select! {
                biased;
                _ = &mut stop => break,
                () = &mut poll_tasks, if has_tasks => {}
                res = &mut accept => {
                    let Ok((conn, proto)) = res else { break };
                    accept = Box::pin(self.transport().accept());
                    if proto != Proto::Rpc {
                        continue;
                    }
                    tasks.push(Box::pin(async move {
                        let peer = conn.remote();
                        while let Ok(Some(mut stream)) = conn.accept_bi().await {
                            let resp = match read_msg::<_, RequestHeader>(&mut stream).await {
                                Ok(hdr) => match hdr.request(self.label()) {
                                    Ok(req) => {
                                        let mut cb = |ev| (on_usage_ref.borrow_mut())(ev);
                                        self.handle_hardware_request(peer, req, state_ref, provider_ref, &mut cb).await
                                    }
                                    Err(e) => Response::Error(e),
                                },
                                Err(e) => Response::Error(WireError::new(ErrorCode::Internal, format!("读取请求失败：{e}"))),
                            };
                            let _ = write_response(&mut stream, &resp).await;
                            let _ = stream.shutdown().await;
                        }
                    }));
                }
            }
        }
        drop(tasks);
        state.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_state_defaults_empty_and_tracks_usage() {
        let st = HardwareState::new();
        assert!(st.enabled_list().is_empty());
        assert!(!st.is_enabled(HardwareKind::Camera));

        st.enable(HardwareKind::Location);
        assert_eq!(st.enabled_list(), vec![HardwareKind::Location]);
        assert!(st.is_enabled(HardwareKind::Location));
        assert!(!st.is_enabled(HardwareKind::Microphone));

        st.begin_use("laptop", HardwareKind::Location);
        assert_eq!(st.active_users(), vec![("laptop".to_string(), HardwareKind::Location)]);
        st.end_use("laptop", HardwareKind::Location);
        assert!(st.active_users().is_empty());
        assert_eq!(st.history().len(), 2);
        assert!(st.history()[0].active);
        assert!(!st.history()[1].active);

        st.clear();
        assert!(st.enabled_list().is_empty());
    }
}
