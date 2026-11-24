# Nexus A/B 实验 B 组

固定基线为 `experiment/ab-baseline-20261124` 上的 `a54db991393645db5e9f13888fcc79b4a2d5cf3f`。本目录保存修复前定义的[行为契约](contract.md)、原始[可执行模型与反例搜索程序](model.py)以及[执行日志](logs/execution.log)。契约和程序的 SHA-256 在基线和修复后的日志中一致。

## 原始结果

| 验证 | 基线 | 修复后 |
| --- | --- | --- |
| 独立模型，1,197 个场景 | 32 个反例，退出码 1 | 0 个反例，退出码 0 |
| 单独修改 `crates/nexus-drive-core/src/lib.rs` | `web=false` | `web=true` |
| 单独修改 `crates/nexus-drive-core/Cargo.toml` | `web=false` | `web=true` |
| 将 `crates/nexus-web-core/src/drive.rs` 重命名到 `crates/nexus-testkit/src/renamed_drive.rs` | 仅传入新路径，`web=false` | 传入两个路径，`web=true` |
| 门禁模拟浏览器任务被跳过 | 通过 | 拒绝放行 |

定向回归共 21 项，全部通过，耗时 0.206 秒。`git diff --check` 通过。实验从 2026-11-24 13:48:28 UTC 到 13:56:30 UTC，实际耗时 482 秒，包含环境启动和建模。生产修复迭代 1 次。Token 消耗未知。验证范围为计划器和必需门禁，未执行完整 CI 或浏览器 E2E。

## 独立复现

从检出本 PR 提交的仓库根目录执行。需要 Git、Python 3.11 或更高版本以及 `cargo`。模型仅调用离线的 `cargo metadata --no-deps` 来读取依赖声明。若 Cargo 不在 `PATH`，给模型加上 `--cargo /path/to/cargo`。

```bash
result_tree=$(git rev-parse --show-toplevel)
base=a54db991393645db5e9f13888fcc79b4a2d5cf3f
base_tree=$(mktemp -d)
git worktree add --detach "$base_tree" "$base"
model="$result_tree/docs/experiments/ab-result-b/model.py"

# 基线预期退出码 1，并报告 1,197 个场景中的 32 个反例。
python3 "$model" --repo "$base_tree"

# 修复版预期退出码 0，并报告同样 1,197 个场景中的 0 个反例。
python3 "$model" --repo "$result_tree"
python3 -m unittest discover -s tools -p 'test_ci_plan.py' -v
```

使用两个计划器回放同一组文件变更。这里在临时基线工作树里创建本地夹具提交，复现输出中的提交 SHA 可以不同。

```bash
(
  cd "$base_tree" || exit 1
  fixture_commit() {
    git -c user.name='CI experiment fixture' \
      -c user.email='ci-experiment@example.invalid' \
      -c commit.gpgsign=false -c core.hooksPath=/dev/null commit "$@"
  }
  replay_git_diff() {
    head=$(git rev-parse HEAD)
    git diff --name-status "$base...$head"
    python3 tools/ci_plan.py --event pull_request --base "$base" --head "$head"
    python3 "$result_tree/tools/ci_plan.py" --event pull_request --base "$base" --head "$head"
  }

  printf '\n// CI selection experiment fixture.\n' >> crates/nexus-drive-core/src/lib.rs
  fixture_commit -am 'Fixture: drive-core source-only change'
  replay_git_diff

  git switch --detach "$base"
  git mv crates/nexus-web-core/src/drive.rs crates/nexus-testkit/src/renamed_drive.rs
  fixture_commit -m 'Fixture: move a browser source file out of its crate'
  replay_git_diff

  paths='["crates/nexus-drive-core/Cargo.toml"]'
  python3 tools/ci_plan.py --event pull_request --paths-json "$paths"
  python3 "$result_tree/tools/ci_plan.py" --event pull_request --paths-json "$paths"
)
```

每组输出依次应为 `web=false` 和 `web=true`。原始门禁模拟及全部输出见[综合日志](logs/execution.log)。
