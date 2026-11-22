#!/usr/bin/env bash
# Read-only health check for verification daemons.
# Usage: doctor.sh <run_root> <nexus_bin> [port=8080]
set -euo pipefail

RUN_ROOT="${1:-}"
NEXUS_BIN="${2:-}"
PORT="${3:-8080}"

if [[ -z "$RUN_ROOT" || -z "$NEXUS_BIN" ]]; then
  echo "Usage: $0 <run_root> <nexus_bin> [port]" >&2
  exit 1
fi

# Anchor HTTP
if ! curl -sf "http://127.0.0.1:$PORT" >/dev/null; then
  echo "FAIL: anchor HTTP not responding on port $PORT" >&2
  exit 1
fi

# Laptop status
if ! "$NEXUS_BIN" --data-dir "$RUN_ROOT/laptop/data" status --json 2>/dev/null | jq -e '.code == 0 and .online == true' >/dev/null; then
  echo "FAIL: laptop daemon not healthy" >&2
  exit 1
fi

# Home status
if ! "$NEXUS_BIN" --data-dir "$RUN_ROOT/home/data" status --json 2>/dev/null | jq -e '.code == 0 and .online == true' >/dev/null; then
  echo "FAIL: home daemon not healthy" >&2
  exit 1
fi

# Optional: verify PIDs still alive
for node in laptop home; do
  pid_file="$RUN_ROOT/$node/daemon.pid"
  if [[ -f "$pid_file" ]]; then
    pid=$(cat "$pid_file")
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "FAIL: $node daemon PID $pid not alive" >&2
      exit 1
    fi
  fi
done

echo "OK: all checks passed"
exit 0