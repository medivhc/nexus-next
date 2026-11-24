# Nexus Next CI 影响分析与分层门禁

## 目的与基线

2026-11-23 PR #9，workflow run 45178939398（ubuntu-24.04）：

| Full CI 内步骤 | 时间（秒） |
| --- | ---: |
| L0 结构检查 + 自检 | 11.1 |
| Clippy all-targets | 350.2 |
| cargo test --no-run | 1266.3 |
| L1 两组单测 | 233.5 |
| L2 仿真 | 79.7 |
| L3 本机中继 | 264.1 |
| L3 真实场景 | 203.2 |
| L3 手册、兼容自检、满载与多路径 | 125.0 |

Clippy + compile-only 共 1616.5 秒，占脚本步骤总耗时 2533.1 秒的 63.8%。Full CI Job 墙钟约 44 分 39 秒；历史基线为 28–46 分钟。编译缓存 Rust 命中率仅 0.19%，不能据此断言 sccache 在暖缓存下无效，要观察多个后续提交。

## 执行策略

工作流 .github/workflows/ci.yml 保留五类原有检查，并新增 Test impact plan 和 **Required CI gate**。每次 workflow 都运行 L0 sanity 和 gate；其余任务由 tools/ci_plan.py 按 PR 改动路径保守决定：

| 修改范围 | Smoke | Full L0–L3 | 真实跨版本 | 浏览器 |
| --- | --- | --- | --- | --- |
| 仅文档（Markdown、docs/tasks 下常见文档与图片；`docs/manual.md` 除外） | – | – | – | – |
| `docs/manual.md`（L3 用户手册照着执行） | ✓ | ✓ | – | – |
| Web UI、web E2E、nexus-web | ✓ | – | – | ✓ |
| nexus-web-core（L2/L3 的浏览器场景直接跑它） | ✓ | ✓ | – | ✓ |
| nexus-drive-core 与其他非协议浏览器构建依赖 | ✓ | ✓ | – | ✓ |
| 其他非协议 native crate | ✓ | ✓ | – | – |
| Wire、协议、CLI、Runtime、Fabric 与核心网络 | ✓ | ✓ | ✓ | ✓ |
| Cargo.lock / vendor / CI 配置与计划 | ✓ | ✓ | ✓ | ✓ |
| 新目录、未知类型 | ✓ | ✓ | ✓ | ✓ |
| push 到 main、每日 schedule、workflow_dispatch | ✓ | ✓ | ✓ | ✓ |

浏览器任务按 Cargo 清单跟踪 `nexus-web`（Wasm 页面）和 `nexus-cli`（端到端测试启动的原生程序）的仓库内传递构建依赖。普通依赖和构建依赖会纳入检查，包含目标专属和可选依赖。重命名按旧路径和新路径计算影响。

PR 的 Full CI 仍执行 Clippy、L1、L2、L3，但跳过独立的 cargo test --no-run：实际执行的测试会自行编译；未执行的 test targets 的编译覆盖由 **main / nightly / manual** 的完整运行保留。L0 检查在 PR 中由强制 sanity Job 完成，避免重复。**这不是删除测试能力**，而是改变运行时机。

兼容性 Job 只有找到相同 wire major 的已发布 tag 时才能执行真正的新旧版本混跑；如果找不到，原 Job 会 success + notice（这是原有行为，不能视为验证了实际兼容）。发布前要人工检查 tag 发现结果，不能仅检查绿色状态。

## 门禁与风险

- tools/ci_plan.py 若无法计算 merge-base diff，计划 Job 失败；未知改动默认跑全部。
- tools/ci_gate.py 验证每个应执行的 Job 为 success；不应执行的 Job 必须明确 skipped；任何失败、取消、未按计划执行、非法布尔输出均失败。
- **必须在 GitHub 的 main 分支保护 / ruleset 中将 Required CI gate 设为必需检查**，建议同时保留 Fast sanity (no compilation)。不要只依赖按需执行的 Full CI (tools/ci.sh)：被条件跳过的 Job 不能替代 Gate 对覆盖范围的校验。
- 合并规则更新前，不应以“跳过的 Full CI 是绿色”作为放行理由。
- concurrency.cancel-in-progress 只会取消同一 ref 的旧 run；gate 不可能替已取消的 run 放行。

## 本地验证与复核

无需 Rust 的 policy 单测：

    python3 -m unittest discover -s tools -p 'test_ci_plan.py' -v

不访问 GitHub 的策略回放：

    python3 tools/ci_plan.py --event pull_request --paths-json '["web/index.html"]'
    python3 tools/ci_plan.py --event pull_request --paths-json '["crates/nexus-wire/src/lib.rs"]'

完整验证：bash tools/ci.sh（默认模式没有任何跳过）。PR 模式：NEXUS_CI_PR_MODE=1 bash tools/ci.sh（只能与独立 sanity + gate 组合用于 CI）。

## 观测与后续收敛

每次记录 plan 输出、各 Job 排队/执行时间、PR 反馈时间、CI 失败类别，以及 Full CI 的每个阶段耗时。至少对 5–10 次包含 Rust 改动的 PR 比较暖/冷缓存，并针对一次相同输入（非并行）做 PR 模式与完整模式对照。目标是普通文档/Web PR 不再等 44 分钟；网络/协议变更仍保留高风险测试。**在没有数据证明覆盖和耗时收益前，不应继续删除 L2/L3、兼容性、多路径与浏览器回归。**
