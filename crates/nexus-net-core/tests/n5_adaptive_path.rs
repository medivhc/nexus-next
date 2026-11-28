// covers: R44, R56, R60, R108
//! N5 自适应路径选择纯逻辑测试用例（对应 docs/test-cases/network-runtime-n5-test-cases.md）。

use nexus_net_core::path_selector::{choose_path, HysteresisConfig, PathDecision, PathSample, TrafficProfile};

fn sample(
    path_id: &str,
    actual_exit: &str,
    is_direct: bool,
    p95_rtt_ms: u32,
    jitter_ms: u32,
    loss_bps: u16,
    goodput_kbps: u32,
) -> PathSample {
    PathSample {
        path_id: path_id.into(),
        actual_exit: actual_exit.into(),
        is_direct,
        p95_rtt_ms,
        jitter_ms,
        loss_bps,
        goodput_kbps,
        sample_count: 20,
        age_secs: 10,
        metered: false,
    }
}

/// TC-N5-01：无明显优势时保持直连，不盲目启用中继
#[test]
fn keeps_direct_when_relay_improvement_below_hysteresis() {
    let candidates = vec![
        sample("direct", "direct", true, 40, 4, 0, 50_000),
        sample("vps-relay", "jp-vps", false, 38, 4, 0, 50_000),
    ];
    let decision = choose_path(TrafficProfile::InteractiveGame, Some("direct"), &candidates, 60, &HysteresisConfig::default());
    assert!(matches!(decision, PathDecision::Keep { ref path_id, .. } if path_id == "direct"), "{decision:?}");
}

/// TC-N5-02：自有 VPS 实测显著改善延迟/抖动/丢包时启用加速
#[test]
fn switches_to_vps_relay_when_game_quality_improves_significantly() {
    let candidates = vec![
        sample("direct", "direct", true, 140, 30, 400, 20_000),
        sample("vps-relay", "jp-vps", false, 45, 5, 0, 50_000),
    ];
    let decision = choose_path(TrafficProfile::InteractiveGame, Some("direct"), &candidates, 60, &HysteresisConfig::default());
    match decision {
        PathDecision::Switch { from, to, improvement_pct, .. } => {
            assert_eq!(from, "direct");
            assert_eq!(to, "vps-relay");
            assert!(improvement_pct >= 15);
        }
        other => panic!("期望切换到 vps-relay：{other:?}"),
    }
}

/// TC-N5-03：未满最短驻留时间时抑制切换防震荡
#[test]
fn respects_min_dwell_time_to_prevent_flapping() {
    let candidates = vec![
        sample("direct", "direct", true, 140, 30, 400, 20_000),
        sample("vps-relay", "jp-vps", false, 45, 5, 0, 50_000),
    ];
    let decision = choose_path(TrafficProfile::InteractiveGame, Some("direct"), &candidates, 10, &HysteresisConfig::default());
    assert!(matches!(decision, PathDecision::Keep { ref path_id, .. } if path_id == "direct"), "{decision:?}");
}

/// TC-N5-04：账号稳定业务允许同出口前段优化，绝不允许更换公网出口身份
#[test]
fn account_stable_allows_same_exit_front_hop_but_forbids_exit_change() {
    let candidates = vec![
        sample("direct->jp-vps", "jp-vps", false, 160, 25, 200, 20_000),
        sample("relay-a->jp-vps", "jp-vps", false, 60, 5, 0, 40_000),
        sample("direct->us-vps", "us-vps", false, 20, 2, 0, 80_000),
    ];
    let decision = choose_path(
        TrafficProfile::AccountStable,
        Some("direct->jp-vps"),
        &candidates,
        60,
        &HysteresisConfig::default(),
    );
    match decision {
        PathDecision::Switch { from, to, .. } => {
            assert_eq!(from, "direct->jp-vps");
            assert_eq!(to, "relay-a->jp-vps");
        }
        other => panic!("应选择同出口前段优化 relay-a->jp-vps，绝不切到 us-vps：{other:?}"),
    }
}

/// TC-N5-05：忽略过期样本与样本量不足的候选
#[test]
fn ignores_stale_or_insufficient_samples() {
    let mut stale_vps = sample("vps-relay", "jp-vps", false, 20, 1, 0, 90_000);
    stale_vps.age_secs = 300;
    let mut few_vps = sample("vps-few", "jp-vps", false, 20, 1, 0, 90_000);
    few_vps.sample_count = 2;

    let candidates = vec![sample("direct", "direct", true, 120, 20, 100, 20_000), stale_vps, few_vps];
    let decision = choose_path(TrafficProfile::InteractiveGame, Some("direct"), &candidates, 60, &HysteresisConfig::default());
    assert!(matches!(decision, PathDecision::Keep { ref path_id, .. } if path_id == "direct"), "{decision:?}");
}

/// TC-N5-06：Bulk 批量传输跳过计费链路
#[test]
fn bulk_profile_skips_metered_paths() {
    let mut fast_metered = sample("5g-relay", "jp-vps", false, 25, 2, 0, 500_000);
    fast_metered.metered = true;
    let candidates = vec![sample("wifi-direct", "direct", true, 40, 5, 0, 100_000), fast_metered];
    let decision = choose_path(TrafficProfile::Bulk, Some("wifi-direct"), &candidates, 60, &HysteresisConfig::default());
    assert!(matches!(decision, PathDecision::Keep { ref path_id, .. } if path_id == "wifi-direct"), "{decision:?}");
}
