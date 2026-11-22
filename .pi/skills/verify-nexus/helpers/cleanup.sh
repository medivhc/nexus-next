#!/usr/bin/env bash
# Tear down verification daemons, preserve evidence.
# Usage: cleanup.sh <run_root>
set -euo pipefail

RUN_ROOT="${1:-}"

if [[ -z "$RUN_ROOT" ]]; then
  echo "Usage: $0 <run_root>" >&2
  exit 1
fi

echo "Cleaning up $RUN_ROOT..."

# Kill daemon processes by PID file (not by name!)
for node in laptop home; do
  pid_file="$RUN_ROOT/$node/daemon.pid"
  if [[ -f "$pid_file" ]]; then
    pid=$(cat "$pid_file")
    if kill -0 "$pid" 2>/dev/null; then
      echo "  Killing $node daemon (PID $pid)..."
      kill "$pid" 2>/dev/null || true
      # Wait for graceful shutdown
      for i in {1..10}; do
        if ! kill -0 "$pid" 2>/dev/null; then break; fi
        sleep 0.2
      done
      # Force if needed
      if kill -0 "$pid" 2>/dev/null; then
        kill -9 "$pid" 2>/dev/null || true
      fi
    fi
  fi
done

# Unmount if any FUSE mount left (best effort)
fusermount3 -u /mnt/nexus-home 2>/dev/null || true

# Remove run directory contents EXCEPT evidence/
if [[ -d "$RUN_ROOT/evidence" ]]; then
  echo "  Preserving evidence at $RUN_ROOT/evidence"
  # Move evidence aside temporarily
  EVIDENCE_TMP=$(mktemp -d -t nexus-evidence-XXXXXX)
  mv "$RUN_ROOT/evidence" "$EVIDENCE_TMP/"
  rm -rf "$RUN_ROOT"
  mv "$EVIDENCE_TMP/evidence" "$RUN_ROOT"
  rmdir "$EVIDENCE_TMP"
else
  rm -rf "$RUN_ROOT"
fi

echo "✅ Cleanup complete. Evidence at $RUN_ROOT/evidence"