# Nexus Next 网络运行时设计文档（N3：统一策略、DNS 改写过滤与服务诊断）

- **文档版本**：v1.0
- **日期**：2026-11-27
- **前置分支**：`feat/n0-n1-network-runtime`（基线 `64a0bca`）
- **上游依据**：`nexus-next-network-runtime-design-v0.1.md` 第 5、6、8、12 节（覆盖 U03、U04、U07、U09）

## 1. 设计目标

在 N1/N2 已落地的单 VPN、通用公网 UDP 出口、Clash 导入/LKG 回滚和省电模式基础上，N3 交付三大核心能力：

1. **统一策略硬约束与确定性选路（U03 / U04）**：支持按地域（如 `JP`）、固定出口（如 `jp-vps`）及 `stable_egress` 硬约束筛选候选出口，输出完整淘汰原因；无可用候选时按失败动作阻断/告警，**绝不破坏硬约束悄悄漂移或降级直连**。
2. **独立 DNS 改写与广告/追踪过滤管线（U07）**：支持 `Allow`（白名单优先局部解除）、`BlockNxdomain`、`BlockNodata` 与 `Override`（自定义 A/AAAA 及 TTL），并保证 `.internal` 私有命名空间永不向公网泄露。
3. **服务级健康分类与本地脱敏观测（U09 基础）**：区分 `NodeHealth` 与 `ServiceHealth`，规范 `FailureClass`（绝不把所有 HTTP 403 臆断为 IP 封禁），通过本地环形缓冲区 `FlowRing` 记录脱敏连接/DNS 证据，供 CLI 与 Agent MCP 查询。

## 2. 模块边界

| 模块 | 落点 | 职责 |
|---|---|---|
| `policy` | `crates/nexus-net-core/src/policy.rs` | `ExitCandidate`、`PolicyConstraint`、`FailureAction`、`select_exit`（纯逻辑、无 I/O） |
| `dns_policy` | `crates/nexus-net-core/src/dns_policy.rs` | `DnsRule`、`DnsAction`、`DnsDecision`、`evaluate_dns`（纯逻辑） |
| `diag` | `crates/nexus-net-core/src/diag.rs` | `ServiceStatus`、`FailureClass`、`classify_http_status`、`FlowEvent`、`FlowRing`（纯逻辑） |
| 控制协议 | `crates/nexus-wire/src/lib.rs` | `CtlRequest::DnsRuleAdd/Rm/List/Explain`、`CtlRequest::FlowRecent` |
| 运行时 | `crates/nexus-runtime/src/lib.rs` | `dns_rules` 持久化、`answer_dns` 管线集成、`FlowRing` 脱敏事件采集 |
| CLI | `crates/nexus-cli/src/lib.rs` | `nexus dns add/rm/ls/explain`、`nexus flow ls` |
