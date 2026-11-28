# Nexus Next 网络运行时设计文档（N5：网络资源协同与自适应选路）

- **文档版本**：v1.0
- **日期**：2026-11-27
- **前置分支**：`feat/n3-policy-dns-diag`
- **上游依据**：`nexus-next-network-runtime-design-v0.1.md` 第 7.3、12 节（覆盖 U05、U04 同出口前段优化）

## 1. 核心原则

1. **无优势不加速（U05）**：对比直连基线与自有 VPS / Mesh 中继候选路径，只有实测样本新鲜、样本量充足且综合质量改善超过滞回阈值（默认 ≥ 15%）时才启用中继加速；否则保持直连。
2. **滞回与最短驻留防震荡**：当前路径驻留时间未达到 `min_dwell_secs` 时不触发普通性能优化切换（严重断连除外）。
3. **同出口前段优化（U04）**：对 `AccountStable` 业务，允许在 `client → relay-A → exit-JP` 与 `client → exit-JP` 之间优化前段路径，但**强制要求 `actual_exit` 相同**，绝不更换公网出口身份。
4. **计费与业务差异化评分**：
   - `InteractiveGame`：综合 `p95_rtt_ms` + `jitter_ms * 2` + `loss_bps / 2`（低分更优）
   - `Bulk`：按 `goodput_kbps` 优先且排除计费路径（`metered == true`）
   - `MobileIdle`：优先不计费且直连路径，避免无谓中继唤醒
