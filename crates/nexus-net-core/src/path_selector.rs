//! 自适应路径选择与滞回防震荡（N5：U05 直连 vs 自有 VPS 对比、U04 同出口前段优化）。

use serde::{Deserialize, Serialize};

/// 业务流量画像（对应 RFC 第 7.3 节）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrafficProfile {
    InteractiveGame,
    Bulk,
    AccountStable,
    PrivateService,
    MobileIdle,
}

/// 一条候选路径在目标上的观测样本。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathSample {
    pub path_id: String,
    /// 实际公网出口身份标识（例如 `"direct"`、`"jp-vps"`）。
    pub actual_exit: String,
    pub is_direct: bool,
    pub p95_rtt_ms: u32,
    pub jitter_ms: u32,
    /// 丢包率（万分比 bps，例如 100 = 1.00%）。
    pub loss_bps: u16,
    /// 有效吞吐（kbps）。
    pub goodput_kbps: u32,
    pub sample_count: u32,
    pub age_secs: u64,
    pub metered: bool,
}

/// 滞回与样本新鲜度门禁配置。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HysteresisConfig {
    /// 相对当前路径的最小改善百分比（例如 15 表示至少改善 15% 才切换）。
    pub min_improvement_pct: u32,
    /// 当前路径最短驻留时间（秒），未满时不因普通优化切换。
    pub min_dwell_secs: u64,
    /// 样本最大有效年龄（秒），超过则视为过期不采信。
    pub max_sample_age_secs: u64,
    /// 触发加速切换所需的最小样本量。
    pub min_samples: u32,
}

impl Default for HysteresisConfig {
    fn default() -> Self {
        Self {
            min_improvement_pct: 15,
            min_dwell_secs: 30,
            max_sample_age_secs: 120,
            min_samples: 5,
        }
    }
}

/// 路径选择结果与可解释原因。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathDecision {
    Initial {
        path_id: String,
        reason: String,
    },
    Keep {
        path_id: String,
        reason: String,
    },
    Switch {
        from: String,
        to: String,
        improvement_pct: u32,
        reason: String,
    },
    Unavailable {
        reason: String,
    },
}

/// 计算路径代价分（越低越好）。
fn cost_score(profile: TrafficProfile, s: &PathSample) -> u64 {
    match profile {
        TrafficProfile::InteractiveGame | TrafficProfile::AccountStable | TrafficProfile::PrivateService => {
            u64::from(s.p95_rtt_ms) + u64::from(s.jitter_ms) * 2 + u64::from(s.loss_bps) / 2
        }
        TrafficProfile::Bulk => {
            let bw = u64::from(s.goodput_kbps.max(1));
            1_000_000_000 / bw + u64::from(s.loss_bps) * 10
        }
        TrafficProfile::MobileIdle => {
            let relay_penalty = if s.is_direct { 0 } else { 500 };
            let meter_penalty = if s.metered { 1000 } else { 0 };
            relay_penalty + meter_penalty + u64::from(s.p95_rtt_ms)
        }
    }
}

/// 依据业务画像、新鲜样本与滞回门禁，在候选路径中做确定性选路。
pub fn choose_path(
    profile: TrafficProfile,
    current_path: Option<&str>,
    candidates: &[PathSample],
    dwell_secs: u64,
    cfg: &HysteresisConfig,
) -> PathDecision {
    let current = current_path.and_then(|id| candidates.iter().find(|c| c.path_id == id));
    let required_exit = if profile == TrafficProfile::AccountStable {
        current.map(|c| c.actual_exit.as_str())
    } else {
        None
    };

    let fresh: Vec<&PathSample> = candidates
        .iter()
        .filter(|c| c.age_secs <= cfg.max_sample_age_secs && c.sample_count >= cfg.min_samples)
        .filter(|c| !(profile == TrafficProfile::Bulk && c.metered))
        .filter(|c| required_exit.is_none_or(|ex| c.actual_exit == ex))
        .collect();

    if fresh.is_empty() {
        if let Some(cur) = current {
            return PathDecision::Keep {
                path_id: cur.path_id.clone(),
                reason: "无新鲜且样本量充足的替代路径，保守保持当前路径".into(),
            };
        }
        if let Some(direct) = candidates.iter().find(|c| c.is_direct) {
            return PathDecision::Initial {
                path_id: direct.path_id.clone(),
                reason: "冷启动尚无足够测量样本，保守默认直连".into(),
            };
        }
        return PathDecision::Unavailable {
            reason: "没有可用候选路径".into(),
        };
    }

    let best = fresh
        .iter()
        .copied()
        .min_by(|a, b| cost_score(profile, a).cmp(&cost_score(profile, b)).then_with(|| a.path_id.cmp(&b.path_id)))
        .expect("fresh 非空");

    let Some(cur) = current else {
        return PathDecision::Initial {
            path_id: best.path_id.clone(),
            reason: format!("初始选中最优候选路径 {}", best.path_id),
        };
    };

    if best.path_id == cur.path_id {
        return PathDecision::Keep {
            path_id: cur.path_id.clone(),
            reason: "当前路径已是最优".into(),
        };
    }

    if dwell_secs < cfg.min_dwell_secs {
        return PathDecision::Keep {
            path_id: cur.path_id.clone(),
            reason: format!("当前路径驻留 {dwell_secs}s 未达最短驻留 {}s，抑制震荡", cfg.min_dwell_secs),
        };
    }

    let cur_cost = cost_score(profile, cur).max(1);
    let best_cost = cost_score(profile, best);
    if best_cost >= cur_cost {
        return PathDecision::Keep {
            path_id: cur.path_id.clone(),
            reason: "候选路径相对当前路径无优势，保持当前路径".into(),
        };
    }

    let improvement_pct = (((cur_cost - best_cost) * 100) / cur_cost) as u32;
    if improvement_pct < cfg.min_improvement_pct {
        return PathDecision::Keep {
            path_id: cur.path_id.clone(),
            reason: format!("候选路径仅改善 {improvement_pct}%，低于滞回阈值 {}%，不切换", cfg.min_improvement_pct),
        };
    }

    PathDecision::Switch {
        from: cur.path_id.clone(),
        to: best.path_id.clone(),
        improvement_pct,
        reason: format!("实测综合质量改善 {improvement_pct}%（≥ {}%），切换至 {}", cfg.min_improvement_pct, best.path_id),
    }
}
