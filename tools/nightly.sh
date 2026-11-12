#!/usr/bin/env bash
# 每晚运行的慢检查（测试方案中的 L4）：大量种子与变异测试。
set -euo pipefail
cd "$(dirname "$0")/.."
echo "== 每个仿真场景 1000 个种子"
NEXUS_SIM_SEEDS=1000 cargo test --quiet --test scenarios --test terminal -- sim
echo "== 属性测试加大样本"
PROPTEST_CASES=10000 cargo test --quiet -p nexus-core-proto --test properties
PROPTEST_CASES=10000 cargo test --quiet --release -p nexus-term-proto
echo "== 变异测试：身份、名册、路径、授权（要求没有漏网的变异）"
cargo mutants -p nexus-core-proto \
  --file crates/nexus-core-proto/src/authz.rs \
  --file crates/nexus-core-proto/src/path.rs \
  --file crates/nexus-core-proto/src/roster.rs \
  --file crates/nexus-core-proto/src/crypto.rs \
  -j 2
echo "== 变异测试：终端会话状态机"
cargo mutants -p nexus-term-proto --file crates/nexus-term-proto/src/lib.rs -j 2
