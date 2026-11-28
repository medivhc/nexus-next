# Nexus Next N6 测试用例

| 用例编号 | 对应验收标准 | 测试层级 | 测试函数 |
|---|---|---|---|
| TC-N6-01 | FP-N6-01 | L1 纯逻辑 | `crates/nexus-net-core/tests/n6_http_rewrite.rs::map_local_returns_mock_without_touching_unmatched_hosts` |
| TC-N6-02 | FP-N6-02 | L1 纯逻辑 | `crates/nexus-net-core/tests/n6_http_rewrite.rs::redirect_target_and_header_modification_rewrite_correctly` |
| TC-N6-03 | FP-N6-03 | L1 纯逻辑 | `crates/nexus-net-core/tests/n6_http_rewrite.rs::fault_inject_and_body_size_budget_are_enforced` |
| TC-N6-04 | FP-N6-01/02/03 | L2 端到端代理 | `crates/nexus-cli/tests/n6_http_rewrite.rs::http_proxy_map_local_redirect_and_fault_inject` |
| TC-N6-05 | FP-N6-04 | L2 MCP 模拟 | `crates/nexus-cli/tests/n6_http_rewrite.rs::mcp_policy_simulate_does_not_mutate_active_routes` |
