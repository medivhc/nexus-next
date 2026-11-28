# Nexus Next N5 测试用例（自适应选路与资源协同）

| 用例编号 | 对应验收标准 | 测试层级 | 测试函数 |
|---|---|---|---|
| TC-N5-01 | FP-N5-01 | L1 纯逻辑 | `crates/nexus-net-core/tests/n5_adaptive_path.rs::keeps_direct_when_relay_improvement_below_hysteresis` |
| TC-N5-02 | FP-N5-02 | L1 纯逻辑 | `crates/nexus-net-core/tests/n5_adaptive_path.rs::switches_to_vps_relay_when_game_quality_improves_significantly` |
| TC-N5-03 | FP-N5-03 | L1 纯逻辑 | `crates/nexus-net-core/tests/n5_adaptive_path.rs::respects_min_dwell_time_to_prevent_flapping` |
| TC-N5-04 | FP-N5-04 | L1 纯逻辑 | `crates/nexus-net-core/tests/n5_adaptive_path.rs::account_stable_allows_same_exit_front_hop_but_forbids_exit_change` |
| TC-N5-05 | FP-N5-05 | L1 纯逻辑 | `crates/nexus-net-core/tests/n5_adaptive_path.rs::ignores_stale_or_insufficient_samples` |
| TC-N5-06 | FP-N5-05 | L1 纯逻辑 | `crates/nexus-net-core/tests/n5_adaptive_path.rs::bulk_profile_skips_metered_paths` |
