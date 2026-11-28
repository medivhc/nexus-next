//! 服务级健康分类与本地脱敏流事件环形缓冲（N3：U09 可核实诊断）。

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// 服务级健康状态（独立于节点级在线状态 `NodeHealth`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceStatus {
    Unknown,
    Healthy,
    Degraded,
    Unavailable,
    PolicyRestricted,
}

/// 可核实的故障原因分类（证据不足时必须标为 `Unknown`，不臆断）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Dns,
    Connect,
    Tls,
    ProxyAuth,
    ExitRegion,
    HttpRateLimit,
    AppError,
    Unknown,
}

/// 按 HTTP 状态码给出保守的服务级健康分类：
/// 严格遵循 RFC 第 8 节——绝不把所有 403 臆断为 IP 封禁。
pub fn classify_http_status(status: u16) -> (ServiceStatus, Option<FailureClass>) {
    match status {
        200..=399 => (ServiceStatus::Healthy, None),
        401 | 407 => (ServiceStatus::Unavailable, Some(FailureClass::ProxyAuth)),
        403 => (ServiceStatus::PolicyRestricted, Some(FailureClass::Unknown)),
        429 => (ServiceStatus::Degraded, Some(FailureClass::HttpRateLimit)),
        500..=599 => (ServiceStatus::Unavailable, Some(FailureClass::AppError)),
        _ => (ServiceStatus::Unknown, Some(FailureClass::Unknown)),
    }
}

/// 单条本地脱敏观测事件（仅元数据，默认绝不记录请求正文、Cookie 或凭据）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowEvent {
    pub flow_id: u64,
    pub ts_ms: u64,
    pub ingress: String,
    pub target: String,
    pub decision: String,
    pub exit_id: Option<String>,
    pub result: String,
    pub error_class: Option<FailureClass>,
}

/// 固定容量环形事件缓冲区，防止长期运行内存无限增长。
#[derive(Clone, Debug)]
pub struct FlowRing {
    cap: usize,
    next_id: u64,
    items: VecDeque<FlowEvent>,
}

impl FlowRing {
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            cap,
            next_id: 1,
            items: VecDeque::with_capacity(cap),
        }
    }

    pub fn record(&mut self, mut event: FlowEvent) -> u64 {
        let flow_id = self.next_id;
        self.next_id += 1;
        event.flow_id = flow_id;
        if self.items.len() == self.cap {
            self.items.pop_front();
        }
        self.items.push_back(event);
        flow_id
    }

    /// 返回最近 `limit` 条事件（按时间倒序，最新在前）。
    pub fn recent(&self, limit: usize) -> Vec<FlowEvent> {
        let take = if limit == 0 { self.items.len() } else { limit.min(self.items.len()) };
        self.items.iter().rev().take(take).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}
