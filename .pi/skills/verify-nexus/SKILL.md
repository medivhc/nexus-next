---
name: verify-nexus
description: "Drive the nexus CLI daemon and web UI end-to-end to prove user-facing behavior. Covers: network init/join/approve, terminal sessions, web enrollment/login, passkey auth, anchor relay, file sync, and drive mount. Use when a change touches CLI commands, daemon behavior, web pages, or auth flows and you need evidence the real user path works."
disable-model-invocation: true
---

# verify-nexus — nexus CLI + web verification skill

**App:** `nexus` — a device-orchestration CLI that turns a person's devices into a single identity with passwordless terminals, structured exec, agent credentials, and a web anchor for browser access.

**Surfaces:**
- **CLI/TUI (primary):** `nexus` binary — commands: `daemon`, `init`, `join`, `device approve`, `ls`, `status`, `ssh`, `exec`, `agent issue`, `web enroll/login`, `anchor`, `session`, `drive`, `mount`, `audit`, `mcp`.
- **Web UI (secondary):** Served by `nexus daemon --anchor-dev` or `--relay`, built to `web/dist` (WASM + xterm.js). Runs in Chromium via Playwright with virtual WebAuthn.

**When to reach for this skill:** Any change to CLI commands, daemon lifecycle, networking (iroh/fabric), terminal proxy, SSH proxy, web enrollment/login, passkey flows, anchor relay, drive sync, or mount. The skill drives the **real binary** and **real browser** — not mocks — and captures evidence (terminal transcripts, screenshots, JSON outputs, session listings) that survives cleanup.

---

## Launch

### Prerequisites (must be present before running the skill)

| Tool | Version | How to get |
|------|---------|------------|
| OS | Linux / macOS (Unix) | `nexus-sys-real` requires Unix APIs (`std::os::unix::fs`, `process_group`). Building the native binary fails on Windows. |
| Rust toolchain | ≥ 1.85 (workspace `rust-version`) | `rustup` |
| `wasm32-unknown-unknown` target | same as toolchain | `rustup target add wasm32-unknown-unknown` |
| `wasm-bindgen` CLI | matching `wasm-bindgen` in `Cargo.lock` | `cargo install -f wasm-bindgen-cli --version <from lock>` |
| `wasm-opt` (binaryen) | ≥ 123 | `apt install binaryen` or download from GitHub releases |
| `clang` | recent | system package manager |
| Node.js + npm | ≥ 20 | system / `fnm` / `nvm` |
| Playwright Chromium | 1.65+ (matches `playwright-core` in `tools/web-e2e/package.json`) | `npx playwright install chromium` |
| `nexus` binary | built from this repo | `cargo build --profile release -p nexus-cli` (or `--profile dev` for faster iteration) |

### Build the web assets (required for web flows)

```bash
cd /c/Users/xiangcui/nexus-next
tools/build-web.sh   # produces web/dist with WASM, xterm.js, index.html
```

Output: `web/dist/` — this directory is passed to `nexus daemon --page-dir`.

### Launch a fresh daemon pair (isolated data dirs)

The skill uses **isolated per-run directories** so multiple instances can run side by side. Each run gets a temp root (e.g., `/tmp/nexus-verify-<timestamp>/` or `%TEMP%\nexus-verify-<timestamp>\` on Windows) with subdirs `laptop/` and `home/` for the two canonical nodes.

**Launch commands (run from the repo root):**

```bash
# Build the CLI once per session
cargo build --profile release -p nexus-cli   # or --profile dev
NEXUS_BIN=target/release/nexus               # adjust for dev profile

# Create isolated run directory
RUN_ROOT=$(mktemp -d -t nexus-verify-XXXXXX)   # on Windows: $env:TEMP\nexus-verify-$(Get-Random)
mkdir -p "$RUN_ROOT/laptop" "$RUN_ROOT/home"

# laptop = anchor (serves web UI + relay on localhost:8080)
$NEXUS_BIN --data-dir "$RUN_ROOT/laptop/data" daemon --name laptop \
  --anchor-dev 127.0.0.1:8080 --page-dir web/dist \
  --no-discovery --agent-socket "$RUN_ROOT/laptop/agent.sock" &
LAPTOP_PID=$!

# home = member (relays through laptop's anchor)
$NEXUS_BIN --data-dir "$RUN_ROOT/home/data" daemon --name home \
  --relay http://127.0.0.1:8080 \
  --no-discovery --agent-socket "$RUN_ROOT/home/agent.sock" &
HOME_PID=$!
```

### Ready check (doctor)

Wait until both daemons are healthy and the anchor page is up:

```bash
# Wait for anchor HTTP
for i in {1..50}; do curl -sf http://127.0.0.1:8080 >/dev/null && break; sleep 0.2; done

# Wait for both daemons to report status
$NEXUS_BIN --data-dir "$RUN_ROOT/laptop/data" status --json
$NEXUS_BIN --data-dir "$RUN_ROOT/home/data" status --json
```

**Ready =** anchor HTTP 200 + both `status --json` return `code=0` with `online=true`.

---

## Doctor

Run before every drive to confirm the instance is worth driving:

```bash
# Returns 0 only if: both daemons alive, anchor HTTP up, both status JSON valid, ports owned by our PIDs
nexus_doctor() {
  local run_root=$1
  local bin=$2
  curl -sf http://127.0.0.1:8080 >/dev/null || return 1
  "$bin" --data-dir "$run_root/laptop/data" status --json | jq -e '.code == 0 and .online == true' >/dev/null || return 1
  "$bin" --data-dir "$run_root/home/data" status --json | jq -e '.code == 0 and .online == true' >/dev/null || return 1
  # Verify ports held by our processes (optional, best-effort)
  return 0
}
```

---

## Drive

All drives use the **real `nexus` binary** against the isolated daemons. Commands are run with `--data-dir` pointing at the run's data dir.

### Canonical flow: network init → join → approve

```bash
# laptop: init network (creates root identity + recovery code)
$NEXUS_BIN --data-dir "$RUN_ROOT/laptop/data" init --name laptop --json

# home: request to join
JOIN_CODE=$($NEXUS_BIN --data-dir "$RUN_ROOT/home/data" join --json | jq -r .code)

# laptop: approve home
$NEXUS_BIN --data-dir "$RUN_ROOT/laptop/data" device approve "$JOIN_CODE" --name home --json

# Verify both see each other online
$NEXUS_BIN --data-dir "$RUN_ROOT/laptop/data" ls --json
$NEXUS_BIN --data-dir "$RUN_ROOT/home/data" ls --json
```

### Drive a persistent terminal session (exec / ssh)

```bash
# Structured execution on home from laptop (non-interactive, recommended for automation)
$NEXUS_BIN --data-dir "$RUN_ROOT/laptop/data" exec home --json -- sh -c 'echo hello; echo world'

# Interactive session (note: nexus ssh takes node, --session, --size, NOT trailing commands)
# $NEXUS_BIN --data-dir "$RUN_ROOT/laptop/data" ssh home
```

### Drive structured exec (agent-style)

```bash
# Issue a short-lived agent credential
AGENT=$($NEXUS_BIN --data-dir "$RUN_ROOT/laptop/data" agent issue --label builder --grant term.exec@home --ttl 4h --json | jq -r .credential)

# Use it from a clean environment (simulates agent)
NEXUS_AGENT=$AGENT $NEXUS_BIN --data-dir "$RUN_ROOT/laptop/data" exec home -- make --version
```

### Drive web enrollment + login (Playwright)

The skill ships a Node.js helper at `.pi/skills/verify-nexus/helpers/web-drive.mjs` that wraps the Playwright flow from `tools/web-e2e/run.mjs`. It takes the run root, nexus binary, and dist dir as args and returns a JSON evidence object.

**Invocation:**

```bash
node .pi/skills/verify-nexus/helpers/web-drive.mjs \
  --nexus "$NEXUS_BIN" \
  --dist web/dist \
  --run-root "$RUN_ROOT" \
  --chrome "$(which chromium || which google-chrome || echo /usr/bin/google-chrome)"
```

**What it does (exact user path, no shortcuts):**
1. `nexus web enroll --label phone` on laptop → gets enrollment URL pointing at anchor
2. Opens URL in Chromium with virtual WebAuthn authenticator (CDP)
3. Clicks "Enroll" → waits for "已登记为 phone"
4. Opens anchor page → clicks "用通行密钥登录" → selects "phone" passkey
5. Clicks "home" device's "终端" button → waits for shell prompt (`$`)
6. Types `echo hello-$((6*7))\n` → verifies output `hello-42`
7. Clicks "离开" → verifies session `main` still exists on home (`nexus session ls home --json`)
8. Re-enters, freezes page 20s via CDP → thaws → verifies session survives
9. Closes page → verifies no browser storage remains (localStorage, sessionStorage, cookies, IndexedDB, caches, service workers)
10. Verifies WASM gzip size ≤ 1.2 MB budget

**Returns (stdout JSON):**
```json
{
  "ok": true,
  "evidence": {
    "enrollment_url": "...",
    "login_ms": 842,
    "login_to_prompt_ms": 1203,
    "wasm_bytes": 987654,
    "wasm_gzip_bytes": 456789,
    "steps": [
      {"step": "打开页面", "ms": 12},
      {"step": "点「用通行密钥登录」并选通行密钥", "ms": 830},
      {"step": "点 home 的「终端」", "ms": 361}
    ]
  }
}
```

### Drive workspace content storage and sync (drive)

```bash
# Enable serve on replica device
$NEXUS_BIN --data-dir "$RUN_ROOT/laptop/data" drive serve on --json

# Create workspace targeting replica
$NEXUS_BIN --data-dir "$RUN_ROOT/laptop/data" drive create work --replica laptop --json

# Put file via stdin, commit and push
echo "content" | $NEXUS_BIN --data-dir "$RUN_ROOT/laptop/data" drive put work:/test.txt --json
$NEXUS_BIN --data-dir "$RUN_ROOT/laptop/data" drive commit work -m "add test" --json
$NEXUS_BIN --data-dir "$RUN_ROOT/laptop/data" drive sync work --json

# Peer pulls workspace metadata and reads content on-demand
$NEXUS_BIN --data-dir "$RUN_ROOT/home/data" drive sync work --json
$NEXUS_BIN --data-dir "$RUN_ROOT/home/data" drive cat work:/test.txt
```

### Drive mount (Linux only, requires FUSE)

```bash
# Requires: fusermount3, CAP_SYS_ADMIN or root
$NEXUS_BIN --data-dir "$RUN_ROOT/laptop/data" mount home /mnt/nexus-home --json &
MOUNT_PID=$!
sleep 2
ls /mnt/nexus-home
kill $MOUNT_PID
fusermount3 -u /mnt/nexus-home
```

---

## Evidence

All evidence is written to a **per-run evidence directory** that survives cleanup:

```
$RUN_ROOT/evidence/
├── cli/
│   ├── init-laptop.json
│   ├── join-home.json
│   ├── approve-home.json
│   ├── ls-laptop.json
│   ├── ls-home.json
│   ├── ssh-home.json
│   ├── exec-home.json
│   ├── drive-serve.json
│   ├── drive-create.json
│   ├── drive-put.json
│   ├── drive-status.json
│   ├── drive-commit.json
│   ├── drive-sync-laptop.json
│   ├── drive-sync-home.json
│   ├── drive-ls-home.json
│   ├── drive-cat-home.txt
│   ├── drive-where-home.json
│   ├── drive-rm.json
│   ├── drive-commit-rm.json
│   ├── drive-sync-rm-laptop.json
│   ├── drive-sync-rm-home.json
│   └── drive-ls-after-rm.json
├── web/
│   ├── enrollment-url.txt
│   ├── login-metrics.json
│   ├── screenshot-terminal.png
│   ├── screenshot-frozen.png
│   ├── storage-after-close.json
│   └── wasm-size.json
└── doctor/
    └── pre-drive.json
```

**Proof standards (enforced by the helpers):**
- Exercise the **real user path** — CLI commands exactly as documented, web clicks exactly as a user would.
- Capture **action + resulting state** — not just the final screen.
- Verify **side effects** — files written, rows inserted, sessions listed, browser storage cleared.
- **No mocks** for production boundaries (iroh, WebAuthn, FUSE). The virtual WebAuthn authenticator is a *test double for the platform*, not a mock of our code.
- Dry-run/test modes: verify what they actually skip by observing files/network/git, not by trusting the flag name.

---

## Cleanup

```bash
cleanup() {
  local run_root=$1
  # Kill our daemon processes (by PID file, not by name)
  [[ -f "$run_root/laptop/daemon.pid" ]] && kill "$(cat "$run_root/laptop/daemon.pid")" 2>/dev/null || true
  [[ -f "$run_root/home/daemon.pid" ]] && kill "$(cat "$run_root/home/daemon.pid")" 2>/dev/null || true
  # Unmount if mounted
  fusermount3 -u /mnt/nexus-home 2>/dev/null || true
  # Remove run directory EXCEPT evidence/
  find "$run_root" -mindepth 1 -maxdepth 1 ! -name 'evidence' -exec rm -rf {} + 2>/dev/null || true
  # Evidence stays at $run_root/evidence for the caller to collect
}
```

**Never** `pkill nexus` or `killall nexus` — that kills the user's session.

---

## Helpers

The skill ships these executables in `.pi/skills/verify-nexus/helpers/`:

| Helper | Purpose | Invocation |
|--------|---------|------------|
| `build-web.sh` | Builds web assets to `web/dist` (wrapper around repo's `tools/build-web.sh`) | `.pi/skills/verify-nexus/helpers/build-web.sh` |
| `launch-daemons.sh` | Launches isolated laptop+home daemons, writes PID files, waits for ready | `.pi/skills/verify-nexus/helpers/launch-daemons.sh <run_root> <nexus_bin> [port=8080]` |
| `doctor.sh` | Read-only health check | `.pi/skills/verify-nexus/helpers/doctor.sh <run_root> <nexus_bin>` |
| `cli-drive.sh` | Runs the canonical CLI flow (init/join/approve/ssh/exec/drive), writes JSON evidence | `.pi/skills/verify-nexus/helpers/cli-drive.sh <run_root> <nexus_bin> <evidence_dir>` |
| `web-drive.mjs` | Runs the Playwright web flow, returns JSON evidence | `node .pi/skills/verify-nexus/helpers/web-drive.mjs --nexus <bin> --dist <dist> --run-root <root> [--chrome <path>]` |
| `cleanup.sh` | Tears down daemons, preserves evidence | `.pi/skills/verify-nexus/helpers/cleanup.sh <run_root>` |

**All helpers are executable** (`chmod +x`) and their invocation is shown above. A helper the reader has to reverse-engineer is not a helper.

---

## Feature Map

See `.pi/skills/verify-nexus/features/` for the maintained map. Each feature file answers:
- **Sub-features** — breakdown
- **How to get to it (user POV)** — exact commands/clicks
- **Driving it with verify-nexus** — which helper + args
- **Gotchas** — preconditions, flakiness, platform limits

**Seeded features (top 5):**
1. `network-init-join` — init network, join device, approve
2. `persistent-terminal` — `nexus ssh` / `nexus exec` with session survival
3. `web-enroll-login` — passkey enrollment, login, terminal in browser
4. `drive-sync` — push/pull files between devices
5. `anchor-relay` — laptop as anchor, home relays through it

---

## Quick Start (for an agent reading this cold)

```bash
# 0. Prerequisites: Rust, wasm tools, Node, Playwright Chromium (see Launch)
# 1. Build
cargo build --profile release -p nexus-cli
.pie/skills/verify-nexus/helpers/build-web.sh
NEXUS_BIN=target/release/nexus

# 2. Launch isolated daemons
RUN_ROOT=$(mktemp -d -t nexus-verify-XXXXXX)
.pie/skills/verify-nexus/helpers/launch-daemons.sh "$RUN_ROOT" "$NEXUS_BIN"

# 3. Doctor
.pie/skills/verify-nexus/helpers/doctor.sh "$RUN_ROOT" "$NEXUS_BIN" || exit 1

# 4. Drive CLI flow
.pie/skills/verify-nexus/helpers/cli-drive.sh "$RUN_ROOT" "$NEXUS_BIN" "$RUN_ROOT/evidence/cli"

# 5. Drive web flow
node .pi/skills/verify-nexus/helpers/web-drive.mjs \
  --nexus "$NEXUS_BIN" --dist web/dist --run-root "$RUN_ROOT" \
  > "$RUN_ROOT/evidence/web/evidence.json"

# 6. Cleanup (evidence survives)
.pie/skills/verify-nexus/helpers/cleanup.sh "$RUN_ROOT"

# 7. Evidence at $RUN_ROOT/evidence/
```

---

## Maintenance

Run `/maintain-verification-skill verify-nexus` when:
- A new CLI command or web page is added
- A flow changes (e.g., enrollment steps, terminal protocol)
- Evidence captures become stale (selectors, JSON schemas)
- A new platform is supported (Windows mount, Android daemon)

The feature map is the source of truth — a proof that drives one entry point is incomplete when the map lists others.