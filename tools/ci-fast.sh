#!/usr/bin/env bash
# 日常迭代的快车道：只跑 L0 门禁 + 纯逻辑包测试（+ 调用者指定的改动相关包）。
#
# 用法：
#   tools/ci-fast.sh                  # 只跑默认的 6 个纯逻辑包
#   tools/ci-fast.sh nexus-runtime    # 再加改动相关的包（可多个）
#
# 改动文件到包的映射（按 crates/<name> 目录名传 -p 参数）：
#   crates/nexus-core-proto, nexus-wire, nexus-term-proto,
#   nexus-net-core, nexus-sys-sim, nexus-web-core  -> 默认已包含
#   crates/nexus-fabric, nexus-fabric-sim, nexus-sys, nexus-testkit,
#   nexus-web, nexus-sys-real      -> 按需追加，例如 tools/ci-fast.sh nexus-fabric-sim
#   crates/nexus-cli, nexus-runtime, nexus-fabric-iroh
#     -> 拉起 iroh / russh 重型依赖树，默认不进快车道；
#        确实改了它们时可以显式追加，但暖缓存下也可能超过 10 分钟目标，
#        以实测为准（见 tasks/ci-slow-001/measurements.md）。
#
# 不包含（仍走 GitHub Actions 全量 tools/ci.sh，PR #2 的 workflow）：
#   全量 cargo clippy --all-targets（本地实测 602.7s）
#   全量 cargo test --no-run（本地实测 1549.5s）
#   L2 仿真场景、L3 本机中继、L3 真实场景
# 全量 tools/ci.sh 的范围不变，本地不强制跑全量。
#
# 目标：暖缓存下在沙箱（1.5 核）上 10 分钟内跑完。
# 计时方式与 tools/ci.sh 一致：每步耗时在该步结束后打印，失败退出非零并显示日志。
# 计时用 awk（不用 bc）：沙箱没有真正的 bc，缺 bc 时旧写法会静默打印 0.0 秒。
# 追加参数里若含默认 6 包会去重，不重复跑。
set -euo pipefail
cd "$(dirname "$0")/.."

# 前置检查：缺 cargo/python3 时明确报错，不要等 check.py 报 FileNotFoundError。
command -v cargo >/dev/null 2>&1 || { echo "错误：找不到 cargo，请把 ~/.cargo/bin 加进 PATH 再跑（例如 export PATH=\"\$HOME/.cargo/bin:\$PATH\"）" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "错误：找不到 python3" >&2; exit 1; }
command -v awk >/dev/null 2>&1 || { echo "错误：找不到 awk（计时需要）" >&2; exit 1; }

PURE_PKGS=(
  -p nexus-core-proto
  -p nexus-wire
  -p nexus-term-proto
  -p nexus-net-core
  -p nexus-sys-sim
  -p nexus-web-core
)

step() {
  local name=$1; shift
  local start=$(date +%s.%N)
  "$@" > /tmp/nexus-ci-fast.log 2>&1 || { cat /tmp/nexus-ci-fast.log; echo "失败：$name"; exit 1; }
  printf '%-28s %6.1f 秒\n' "$name" "$(echo "$(date +%s.%N) $start" | awk '{printf "%.1f", $1 - $2}')"
}

echo "=== nexus-next 快车道 CI（不含全量 clippy / 全量 --no-run / L2 / L3，全量见 tools/ci.sh + GitHub Actions） ==="
step "L0 结构、追溯、许可证"   python3 tools/check.py
step "L0 检查脚本自检"         python3 tools/check.py --self-test
step "快车道 clippy（6 个纯逻辑包）" cargo clippy --quiet "${PURE_PKGS[@]}" --all-targets -- -D warnings
step "L1 纯逻辑"               cargo test --quiet "${PURE_PKGS[@]}"

if [ "$#" -gt 0 ]; then
  extra_args=()
  extra_names=()
  for pkg in "$@"; do
    case "$pkg" in
      nexus-core-proto|nexus-wire|nexus-term-proto|nexus-net-core|nexus-sys-sim|nexus-web-core)
        echo "跳过已包含的默认包：$pkg（默认 6 包里已跑过，不重复跑）" ;;
      *) extra_args+=(-p "$pkg"); extra_names+=("$pkg") ;;
    esac
  done
  if [ "${#extra_args[@]}" -gt 0 ]; then
    step "改动相关包：${extra_names[*]}" cargo test --quiet "${extra_args[@]}"
  fi
fi

echo "全部通过"
