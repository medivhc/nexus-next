#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
step() {
  local name=$1; shift
  local start=$(date +%s.%N)
  "$@" > /tmp/nexus-ci.log 2>&1 || { cat /tmp/nexus-ci.log; echo "失败：$name"; exit 1; }
  printf '%-28s %6.1f 秒\n' "$name" "$(awk -v a="$(date +%s.%N)" -v b="$start" 'BEGIN { print a - b }')"
}
# PR checks run L0 in the mandatory independent sanity job.
# Standalone, nightly and main-branch runs retain the entire L0-L3 suite.
if [ "${NEXUS_CI_PR_MODE:-0}" = "1" ]; then
  echo "PR mode: L0 static checks are covered by the mandatory sanity job"
else
  step "L0 结构、追溯、许可证" python3 tools/check.py
  step "L0 检查脚本自检"       python3 tools/check.py --self-test
fi
if [ "${NEXUS_CI_SKIP_CLIPPY:-0}" = "1" ]; then
  echo "CI mode: L0 clippy is covered by the parallel clippy job"
else
  step "L0 clippy"               cargo clippy --all-targets --quiet -- -D warnings
fi
# cargo test --no-run also compiles tests not executed by L1-L3.
# Keep compile-only coverage nightly/on main; PR compiles the actually executed suites.
if [ "${NEXUS_CI_PR_MODE:-0}" = "1" ]; then
  echo "PR mode: skip compile-only targets (nightly/main still compiles all)"
else
  step "编译测试"              cargo test --no-run --quiet
fi
# Use default workspace-member invocations (avoiding narrow `-p` subsets) so Cargo
# keeps a single unified feature graph across L1, L2, L3, relay, and local_relay.
if [ "${NEXUS_CI_PR_MODE:-0}" = "1" ]; then
  echo "PR mode: L1 pure-logic crates are covered by the parallel smoke job"
  step "L1 命令行与运行时单元"   cargo test --quiet --lib --bins
else
  step "L1 纯逻辑"               cargo test --quiet -p nexus-core-proto -p nexus-wire -p nexus-term-proto -p nexus-net-core -p nexus-drive-core -p nexus-sys-sim -p nexus-web-core
  step "L1 命令行与运行时单元"   cargo test --quiet --lib --bins
fi
step "L2 仿真场景"             cargo test --quiet --test scenarios --test terminal --test agent --test web --test anchor --test services --test proxy --test exit --test fullnode --test drive --test drive_sync --test drive_mount --test drive_prefetch --test join_short --test recover --test root_rotate --test mcp --test versions --test session_restart --test upgrade --test open --test web_drive --test vault --test share --test web_service --test web_hardware -- sim
# 完整节点的场景要建虚拟网卡（CAP_NET_ADMIN）：不是 root 但能免密 sudo 时（例如 GitHub 的 runner），
# 给测试用的二进制加上这项能力；都不行时跳过这些场景并说明。
FULLNODE="--test fullnode"
SUDO=""
if [ "$(id -u)" != 0 ]; then
  if sudo -n true 2>/dev/null; then
    SUDO="sudo -n"
    sudo -n setcap cap_net_admin+ep target/debug/nexus
  else
    echo "跳过完整节点的真实场景：不是 root，也不能免密 sudo（需要 CAP_NET_ADMIN 建虚拟网卡）"
    FULLNODE=""
  fi
fi
step "L3 本机中继"             cargo test --quiet --test relay
step "L3 真实场景"             cargo test --quiet --test scenarios --test terminal --test agent --test ssh --test web --test anchor --test services --test proxy --test exit $FULLNODE --test drive --test drive_sync --test drive_mount --test drive_prefetch --test join_short --test recover --test root_rotate --test mcp --test versions --test session_restart --test upgrade --test open --test web_drive --test vault --test share --test web_service --test web_hardware -- real
step "L3 用户手册"             python3 tools/manual_check.py
step "L3 新旧混跑（自检）"     python3 tools/compat_check.py --old-bin target/debug/nexus
# 需要 root、ip、tc；不满足时跳过（脚本自己说明原因）。
step "L3 满载测量"             $SUDO python3 tools/sched_bench.py --only-scheduled --attempts 3
step "编译测试用中继"           cargo build --quiet --example local_relay
if [ "${NEXUS_CI_PR_MODE:-0}" = "1" ]; then
  # PR mode runs the full multipath suite once under 20ms RTT (strictly harder than 0ms);
  # main/nightly/local runs still execute both 0ms and 20ms baselines.
  step "L3 多路径（延迟）"     $SUDO python3 tools/multipath_check.py --delay-ms 20
else
  step "L3 多路径"             $SUDO python3 tools/multipath_check.py
  # 有延迟的链路（需要 netem，GitHub 的 runner 上有；没有时跳过）。
  step "L3 多路径（延迟）"     $SUDO python3 tools/multipath_check.py --delay-ms 20 --only failover,redundancy,relay,bulk
fi
echo "全部通过"
