#!/usr/bin/env bash
# Launch isolated laptop + home daemons for verification.
# Usage: launch-daemons.sh <run_root> <nexus_bin> [port=8080]
set -euo pipefail

RUN_ROOT="${1:-}"
NEXUS_BIN="${2:-}"
PORT="${3:-8080}"

if [[ -z "$RUN_ROOT" || -z "$NEXUS_BIN" ]]; then
  echo "Usage: $0 <run_root> <nexus_bin> [port]" >&2
  exit 1
fi

if [[ ! -x "$NEXUS_BIN" ]]; then
  echo "ERROR: nexus binary not found or not executable: $NEXUS_BIN" >&2
  exit 1
fi

if [[ ! -f web/dist/index.html ]]; then
  echo "ERROR: web/dist not built. Run helpers/build-web.sh first." >&2
  exit 1
fi

mkdir -p "$RUN_ROOT/laptop" "$RUN_ROOT/home"

# laptop = anchor (serves web UI + relay)
"$NEXUS_BIN" --data-dir "$RUN_ROOT/laptop/data" daemon --name laptop \
  --anchor-dev "127.0.0.1:$PORT" --page-dir web/dist \
  --no-discovery --agent-socket "$RUN_ROOT/laptop/agent.sock" \
  > "$RUN_ROOT/laptop/daemon.log" 2>&1 &
LAPTOP_PID=$!
echo "$LAPTOP_PID" > "$RUN_ROOT/laptop/daemon.pid"

# home = member (relays through laptop's anchor)
"$NEXUS_BIN" --data-dir "$RUN_ROOT/home/data" daemon --name home \
  --relay "http://127.0.0.1:$PORT" \
  --no-discovery --agent-socket "$RUN_ROOT/home/agent.sock" \
  > "$RUN_ROOT/home/daemon.log" 2>&1 &
HOME_PID=$!
echo "$HOME_PID" > "$RUN_ROOT/home/daemon.pid"

echo "Launched: laptop PID=$LAPTOP_PID, home PID=$HOME_PID"
echo "Run root: $RUN_ROOT"

# Wait for anchor HTTP
echo -n "Waiting for anchor HTTP..."
for i in {1..50}; do
  if curl -sf "http://127.0.0.1:$PORT" >/dev/null 2>&1; then
    echo " OK"
    break
  fi
  sleep 0.2
  if [[ $i -eq 50 ]]; then
    echo " TIMEOUT"
    cat "$RUN_ROOT/laptop/daemon.log"
    exit 1
  fi
done

# Wait for both daemons to report status
echo -n "Waiting for laptop daemon..."
for i in {1..30}; do
  if "$NEXUS_BIN" --data-dir "$RUN_ROOT/laptop/data" status --json 2>/dev/null | jq -e '.code == 0 and .online == true' >/dev/null; then
    echo " OK"
    break
  fi
  sleep 0.2
  if [[ $i -eq 30 ]]; then
    echo " TIMEOUT"
    cat "$RUN_ROOT/laptop/daemon.log"
    exit 1
  fi
done

echo -n "Waiting for home daemon..."
for i in {1..30}; do
  if "$NEXUS_BIN" --data-dir "$RUN_ROOT/home/data" status --json 2>/dev/null | jq -e '.code == 0 and .online == true' >/dev/null; then
    echo " OK"
    break
  fi
  sleep 0.2
  if [[ $i -eq 30 ]]; then
    echo " TIMEOUT"
    cat "$RUN_ROOT/home/daemon.log"
    exit 1
  fi
done

echo "✅ Both daemons ready"