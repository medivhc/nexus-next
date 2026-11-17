#!/usr/bin/env bash
# 每次提交都要通过的全部检查（测试方案中的 L0 到 L3），并报告各自的耗时。
set -euo pipefail
cd "$(dirname "$0")/.."
step() {
  local name=$1; shift
  local start=$(date +%s.%N)
  "$@" > /tmp/nexus-ci.log 2>&1 || { cat /tmp/nexus-ci.log; echo "失败：$name"; exit 1; }
  printf '%-28s %6.1f 秒\n' "$name" "$(awk -v a="$(date +%s.%N)" -v b="$start" 'BEGIN { print a - b }')"
}
step "L0 结构、追溯、许可证"   python3 tools/check.py
step "L0 检查脚本自检"         python3 tools/check.py --self-test
step "L0 clippy"               cargo clippy --all-targets --quiet -- -D warnings
step "编译测试"                cargo test --no-run --quiet
step "L1 纯逻辑"               cargo test --quiet -p nexus-core-proto -p nexus-wire -p nexus-term-proto -p nexus-net-core -p nexus-sys-sim -p nexus-web-core
step "L1 命令行与运行时单元"   cargo test --quiet -p nexus-cli -p nexus-runtime --lib --bins
step "L2 仿真场景"             cargo test --quiet --test scenarios --test terminal --test agent --test web --test anchor --test services --test proxy --test exit --test fullnode -- sim
step "L3 本机中继"             cargo test --quiet -p nexus-fabric-iroh --test relay
step "L3 真实场景"             cargo test --quiet --test scenarios --test terminal --test agent --test ssh --test web --test anchor --test services --test proxy --test exit --test fullnode -- real
# 需要 root、ip、tc；不满足时跳过（脚本自己说明原因）。
step "L3 满载测量"             python3 tools/sched_bench.py --only-scheduled
echo "全部通过"
