# Nexus Next N3 测试用例（统一策略、DNS 改写过滤与服务诊断）

| 用例编号 | 对应验收标准 | 测试层级 | 测试函数 |
|---|---|---|---|
| TC-N3-01 | FP-N3-01 | L1 纯逻辑 | `crates/nexus-net-core/tests/n3_policy_dns_diag.rs::region_constraint_selects_lowest_rtt_and_explains_rejections` |
| TC-N3-02 | FP-N3-01 | L1 纯逻辑 | `crates/nexus-net-core/tests/n3_policy_dns_diag.rs::region_constraint_blocks_when_no_healthy_candidate` |
| TC-N3-03 | FP-N3-02 | L1 纯逻辑 | `crates/nexus-net-core/tests/n3_policy_dns_diag.rs::stable_fixed_exit_notifies_and_blocks_without_drift` |
| TC-N3-04 | FP-N3-03/04 | L1 纯逻辑 | `crates/nexus-net-core/tests/n3_policy_dns_diag.rs::dns_policy_allowlist_overrides_block_and_protects_internal` |
| TC-N3-05 | FP-N3-05 | L1 纯逻辑 | `crates/nexus-net-core/tests/n3_policy_dns_diag.rs::service_health_does_not_misclassify_403_as_ip_ban` |
| TC-N3-06 | FP-N3-06 | L1 纯逻辑 | `crates/nexus-net-core/tests/n3_policy_dns_diag.rs::flow_ring_evicts_oldest_and_preserves_order` |
| TC-N3-07 | FP-N3-03/04 | L2 运行时仿真 | `crates/nexus-testkit/tests/n3_dns_runtime.rs::tun_dns_applies_override_block_nodata_and_allowlist` |
| TC-N3-08 | FP-N3-06 | L2 运行时仿真 | `crates/nexus-testkit/tests/n3_dns_runtime.rs::tun_dns_records_redacted_flow_events` |
| TC-N3-09 | FP-N3-03/04 | L2 CLI 仿真+真实 | `crates/nexus-cli/tests/n3_dns_diag.rs::dns_rules_add_explain_list_and_rm` |
| TC-N3-10 | FP-N3-06 | L2 CLI 仿真+真实 | `crates/nexus-cli/tests/n3_dns_diag.rs::flow_ls_returns_recent_events` |
