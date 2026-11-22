#!/usr/bin/env bash
# Run canonical CLI verification flows against isolated daemons.
# Usage: cli-drive.sh <run_root> <nexus_bin> <evidence_dir>
set -euo pipefail

RUN_ROOT="${1:-}"
NEXUS_BIN="${2:-}"
EVIDENCE_DIR="${3:-}"

if [[ -z "$RUN_ROOT" || -z "$NEXUS_BIN" || -z "$EVIDENCE_DIR" ]]; then
  echo "Usage: $0 <run_root> <nexus_bin> <evidence_dir>" >&2
  exit 1
fi

mkdir -p "$EVIDENCE_DIR"

# Helper: run command on a node, capture JSON output to evidence file
cli() {
  local node="$1"; shift
  local outfile="$1"; shift
  "$NEXUS_BIN" --data-dir "$RUN_ROOT/$node/data" "$@" --json > "$EVIDENCE_DIR/$outfile"
  local code=$?
  if [[ $code -ne 0 ]]; then
    echo "FAIL: cli $node $* exited with $code" >&2
    cat "$EVIDENCE_DIR/$outfile" >&2
    exit $code
  fi
}

echo "=== network-init-join ==="
# laptop: init
cli laptop init-laptop.json init --name laptop
if ! jq -e '.name == "laptop" and .recovery_code != null' "$EVIDENCE_DIR/init-laptop.json" >/dev/null; then
  echo "FAIL: init-laptop.json missing expected fields" >&2
  exit 1
fi

# home: join (join code starts with nxj1-)
cli home join-home.json join
JOIN_CODE=$(jq -r .code "$EVIDENCE_DIR/join-home.json")
if [[ ! "$JOIN_CODE" =~ ^nxj1- ]]; then
  echo "FAIL: join code does not start with nxj1-: $JOIN_CODE" >&2
  exit 1
fi

# laptop: approve home
cli laptop approve-home.json device approve "$JOIN_CODE" --name home

# both: ls (both show online: true)
cli laptop ls-laptop.json ls
cli home ls-home.json ls

# both: status (both show roster_version >= 2)
cli laptop status-laptop.json status
cli home status-home.json status

echo "=== persistent-terminal ==="
# exec home --json (uses exit_code, not code)
"$NEXUS_BIN" --data-dir "$RUN_ROOT/laptop/data" exec --json home -- sh -c 'echo hello; echo world' > "$EVIDENCE_DIR/exec-home.json"
if ! jq -e '.exit_code == 0 and (.stdout | contains("hello")) and (.stdout | contains("world"))' "$EVIDENCE_DIR/exec-home.json" >/dev/null; then
  echo "FAIL: exec-home.json missing expected exit_code or output" >&2
  cat "$EVIDENCE_DIR/exec-home.json" >&2
  exit 1
fi

# Helper: run a command as the agent, through the daemon's agent socket.
# 语法：nexus exec --json <NODE> -- <COMMAND>
agent_cli() {
  local outfile="$1"; shift
  local node="$1"; shift
  NEXUS_AGENT="$AGENT_CRED" NEXUS_AGENT_SOCKET="$RUN_ROOT/laptop/agent.sock" \
    "$NEXUS_BIN" --data-dir "$RUN_ROOT/laptop/data" exec --json "$node" -- "$@" > "$EVIDENCE_DIR/$outfile"
}

# agent issue + exec (credential starts with nxa1-)
cli laptop agent-issue.json agent issue --label builder --grant term.exec@home --ttl 4h
AGENT_CRED=$(jq -r .credential "$EVIDENCE_DIR/agent-issue.json")
if [[ ! "$AGENT_CRED" =~ ^nxa1- ]]; then
  echo "FAIL: agent credential does not start with nxa1-" >&2
  exit 1
fi

agent_cli agent-exec.json home echo "agent ok"
if ! jq -e '.exit_code == 0 and (.stdout | contains("agent ok"))' "$EVIDENCE_DIR/agent-exec.json" >/dev/null; then
  echo "FAIL: agent exec failed（凭据走 agent 入口，需要 NEXUS_AGENT_SOCKET）" >&2
  cat "$EVIDENCE_DIR/agent-exec.json" >&2
  exit 1
fi

# 越权必须被拒：只有 term.exec@home 的凭据不能列会话
# 用 agent socket 直接调用 session ls（走 control channel）
if NEXUS_AGENT="$AGENT_CRED" NEXUS_AGENT_SOCKET="$RUN_ROOT/laptop/agent.sock" \
  "$NEXUS_BIN" --data-dir "$RUN_ROOT/laptop/data" session ls home --json > "$EVIDENCE_DIR/denied-session-ls.json" 2>&1; then
  echo "FAIL: 未授权的 session ls 竟然成功了" >&2
  exit 1
fi

# session ls (uses created_at_ms)
cli home session-ls-home.json session ls home
if ! jq -e '.sessions != null' "$EVIDENCE_DIR/session-ls-home.json" >/dev/null; then
  echo "FAIL: session-ls-home.json missing sessions field" >&2
  exit 1
fi

echo "=== drive-sync ==="
# 1. Enable serve on HOME (storage replica)
cli home drive-serve.json drive serve on
if ! jq -e '.serving == true' "$EVIDENCE_DIR/drive-serve.json" >/dev/null; then
  echo "FAIL: drive serve on failed" >&2
  exit 1
fi

# 2. Create workspace 'work' on LAPTOP targeting HOME as replica
cli laptop drive-create.json drive create work --replica home
if ! jq -e '.workspace == "work"' "$EVIDENCE_DIR/drive-create.json" >/dev/null; then
  echo "FAIL: drive create work failed" >&2
  exit 1
fi

# 3. Put content into work:/notes/hello.txt via stdin
echo "hello nexus drive content" | "$NEXUS_BIN" --data-dir "$RUN_ROOT/laptop/data" drive put work:/notes/hello.txt --json > "$EVIDENCE_DIR/drive-put.json"
if ! jq -e '.path == "work:/notes/hello.txt" and .size > 0' "$EVIDENCE_DIR/drive-put.json" >/dev/null; then
  echo "FAIL: drive put failed" >&2
  cat "$EVIDENCE_DIR/drive-put.json" >&2
  exit 1
fi

# 4. Check status shows added file
cli laptop drive-status.json drive status work

# 5. Commit
cli laptop drive-commit.json drive commit work -m "add hello"
if ! jq -e '.commit != null' "$EVIDENCE_DIR/drive-commit.json" >/dev/null; then
  echo "FAIL: drive commit failed" >&2
  exit 1
fi

# 6. Sync on laptop (pushes to replica)
cli laptop drive-sync-laptop.json drive sync work

# 7. Sync on home (pulls from replica)
cli home drive-sync-home.json drive sync work

# 8. List files on home
cli home drive-ls-home.json drive ls work:/notes
if ! jq -e '.entries[] | select(.name == "hello.txt")' "$EVIDENCE_DIR/drive-ls-home.json" >/dev/null; then
  echo "FAIL: drive ls on home does not contain hello.txt" >&2
  cat "$EVIDENCE_DIR/drive-ls-home.json" >&2
  exit 1
fi

# 9. Cat file content on home
"$NEXUS_BIN" --data-dir "$RUN_ROOT/home/data" drive cat work:/notes/hello.txt > "$EVIDENCE_DIR/drive-cat-home.txt"
if ! grep -q "hello nexus drive content" "$EVIDENCE_DIR/drive-cat-home.txt"; then
  echo "FAIL: drive cat content mismatch" >&2
  cat "$EVIDENCE_DIR/drive-cat-home.txt" >&2
  exit 1
fi

# 10. Where check on the owner (it holds the committed copy and the replica list)
cli laptop drive-where-laptop.json drive where work:/notes/hello.txt
if ! jq -e '.synced == true and (.replicas | index("home") != null)' "$EVIDENCE_DIR/drive-where-laptop.json" >/dev/null; then
  echo "FAIL: drive where on laptop does not show synced == true with home replica" >&2
  cat "$EVIDENCE_DIR/drive-where-laptop.json" >&2
  exit 1
fi

# 11. Remove file on laptop, commit and sync
cli laptop drive-rm.json drive rm work:/notes/hello.txt
cli laptop drive-commit-rm.json drive commit work -m "remove hello"
cli laptop drive-sync-rm-laptop.json drive sync work
cli home drive-sync-rm-home.json drive sync work

# 12. Verify file removed on home
cli home drive-ls-after-rm.json drive ls work:/notes
if jq -e '.entries[] | select(.name == "hello.txt")' "$EVIDENCE_DIR/drive-ls-after-rm.json" >/dev/null 2>&1; then
  echo "FAIL: file still present on home after remove and sync" >&2
  exit 1
fi

echo "=== anchor-relay ==="
# WebAuthn 要求 secure context：本机开发只能用 http://localhost（不是 127.0.0.1）。
cli laptop anchor-set.json anchor set http://localhost:8080

# Verify web enroll URL origin matches anchor
cli laptop web-enroll.json web enroll --label test-phone
ENROLL_URL=$(jq -r .url "$EVIDENCE_DIR/web-enroll.json")
if [[ ! "$ENROLL_URL" =~ ^http://localhost:8080/#enroll= ]]; then
  echo "FAIL: web enroll URL does not match anchor origin: $ENROLL_URL" >&2
  exit 1
fi

echo "✅ All CLI drives passed"
echo "Evidence in $EVIDENCE_DIR"
