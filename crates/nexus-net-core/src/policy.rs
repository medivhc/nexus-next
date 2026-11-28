//! 统一策略硬约束选路与可解释淘汰（N3：U03 地域出口、U04 固定稳定出口防漂移）。

use serde::{Deserialize, Serialize};

/// 一个候选出口资源及其当前观测属性。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitCandidate {
    pub device: String,
    /// ISO 地域码（如 `"JP"`、`"US"`），未知时为 `None`。
    pub region: Option<String>,
    /// 是否经实测确认具备稳定公网出口身份。
    pub stable_egress: bool,
    /// 当前出口是否健康可用。
    pub healthy: bool,
    /// 最近观测往返延迟（毫秒），无测量样本时为 `None`。
    pub rtt_ms: Option<u32>,
    pub tags: Vec<String>,
}

/// 策略硬约束：必须全部满足，绝不可因性能优化而违反。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyConstraint {
    pub region: Option<String>,
    pub fixed_exit: Option<String>,
    pub require_stable_egress: bool,
}

impl PolicyConstraint {
    pub fn has_hard_constraint(&self) -> bool {
        self.region.is_some() || self.fixed_exit.is_some() || self.require_stable_egress
    }
}

/// 无可用候选出口时的失败动作。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureAction {
    #[default]
    Block,
    NotifyAndBlock,
    DirectIfAllowed,
}

/// 被淘汰候选出口的解释记录。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedCandidate {
    pub device: String,
    pub reason: String,
}

/// 确定性选路结果与完整解释证据。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PolicyDecision {
    Selected {
        device: String,
        reason: String,
        rejected: Vec<RejectedCandidate>,
    },
    Blocked {
        notify: bool,
        reason: String,
        rejected: Vec<RejectedCandidate>,
    },
    FallbackDirect {
        reason: String,
        rejected: Vec<RejectedCandidate>,
    },
}

/// 先按硬约束过滤，再在可行候选中确定性选优（RTT 优先，其次设备名字典序）。
/// 若存在硬约束且可行集合为空，强制阻断，绝不跨地域漂移或悄悄降级直连。
pub fn select_exit(
    constraint: &PolicyConstraint,
    candidates: &[ExitCandidate],
    on_failure: FailureAction,
) -> PolicyDecision {
    let mut feasible: Vec<&ExitCandidate> = Vec::new();
    let mut rejected: Vec<RejectedCandidate> = Vec::new();

    for cand in candidates {
        if let Some(fixed) = constraint.fixed_exit.as_deref() {
            if cand.device != fixed {
                rejected.push(RejectedCandidate {
                    device: cand.device.clone(),
                    reason: format!("不满足固定出口约束（需要 {fixed}）"),
                });
                continue;
            }
        }
        if let Some(req_region) = constraint.region.as_deref() {
            let matched = cand
                .region
                .as_deref()
                .is_some_and(|r| r.eq_ignore_ascii_case(req_region));
            if !matched {
                let actual = cand.region.as_deref().unwrap_or("unknown");
                rejected.push(RejectedCandidate {
                    device: cand.device.clone(),
                    reason: format!("地域不匹配（需要 {req_region}，实际 {actual}）"),
                });
                continue;
            }
        }
        if constraint.require_stable_egress && !cand.stable_egress {
            rejected.push(RejectedCandidate {
                device: cand.device.clone(),
                reason: "不满足 stable_egress 稳定出口约束".into(),
            });
            continue;
        }
        if !cand.healthy {
            rejected.push(RejectedCandidate {
                device: cand.device.clone(),
                reason: "出口当前不健康或不可达".into(),
            });
            continue;
        }
        feasible.push(cand);
    }

    if feasible.is_empty() {
        let hard = constraint.has_hard_constraint();
        if !hard && on_failure == FailureAction::DirectIfAllowed {
            return PolicyDecision::FallbackDirect {
                reason: "无可用候选出口，且无硬约束，按策略回退直连".into(),
                rejected,
            };
        }
        let notify = on_failure == FailureAction::NotifyAndBlock;
        let reason = if hard {
            "无满足硬约束的健康出口：为防止地域/身份漂移已阻断流量".into()
        } else {
            "无可用健康出口，按失败策略阻断流量".into()
        };
        return PolicyDecision::Blocked {
            notify,
            reason,
            rejected,
        };
    }

    feasible.sort_by(|a, b| {
        let ra = a.rtt_ms.unwrap_or(u32::MAX);
        let rb = b.rtt_ms.unwrap_or(u32::MAX);
        ra.cmp(&rb).then_with(|| a.device.cmp(&b.device))
    });
    let best = feasible[0];
    let reason = match best.rtt_ms {
        Some(rtt) => format!("满足全部硬约束，且在候选集合中延迟最优（{rtt} ms）"),
        None => "满足全部硬约束（尚无延迟样本，按确定性顺序选中）".into(),
    };
    PolicyDecision::Selected {
        device: best.device.clone(),
        reason,
        rejected,
    }
}
