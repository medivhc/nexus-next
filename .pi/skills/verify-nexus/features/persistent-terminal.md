# persistent-terminal — 常驻终端会话与结构化执行

## Sub-features
- `ssh` — `nexus ssh <device> [--session <name>]` 交互式常驻终端，Ctrl-]（0x1d）断开，会话继续运行
- `exec` — `nexus exec <device> [--json] -- <cmd>` 结构化执行，`--json` 输出 `{exit_code, stdout, stderr, ...}`，适合 agent
- `agent issue` — `nexus agent issue --label <l> --grant <g> --ttl <t>` 签发带 nxa1- 前缀的 agent 凭据
- `session ls` — 列出设备上的会话、客户端计数与创建时间（`created_at_ms`）
- `session kill` — 结束指定会话

## How to get to it (user POV)
```bash
# 前提：网络已建立，双方在线（见 network-init-join）

# 交互式终端（Ctrl-] 断开，会话保持在后台）
nexus ssh home                    # 进入 home 的主会话 main
# 再次连接，回到同一会话
nexus ssh home --session main

# 结构化执行（非交互，适合脚本/agent）
nexus exec home -- uname -a       # 直接输出命令结果
nexus exec home --json -- make    # JSON 输出：{exit_code, stdout, stderr, duration_ms, ...}

# Agent 凭据执行（细粒度授权，如仅允许在 home 上执行终端）
nexus agent issue --label builder --grant term.exec@home --ttl 4h --json
# 凭据以 nxa1- 开头，通过环境变量或 --agent 传入
NEXUS_AGENT=<凭据> nexus exec home -- cargo test

# 查看与结束会话
nexus session ls home --json      # 显示会话名、客户端数、created_at_ms
nexus session kill home main --json # 结束会话
```

## Driving it with verify-nexus
```bash
# cli-drive.sh runs these after network-init-join:
# 1. exec home --json -- sh -c 'echo hello; echo world'   (verifies structured output)
# 2. agent issue → exec with credential                   (verifies agent auth)
# 3. session ls home --json                               (verifies session metadata)
# Note: interactive ssh cannot take trailing command arguments (-- <cmd>).
# It takes node, --session, --size. Trailing commands are for nexus exec.
```
Evidence written to `$RUN_ROOT/evidence/cli/`:
- `exec-home.json` — JSON output with `exit_code: 0`
- `agent-issue.json`, `agent-exec.json` — credential flow (`nxa1-` token)
- `session-ls-home.json` — session list with `created_at_ms` and client count

## Gotchas
- **`exec --json` 的退出码字段是 `exit_code`**，不是 `code`。同时输出 `schema: 2`, `duration_ms`, `signal`, `timed_out`, `truncated`, `stdout`, `stderr`。
- **`nexus ssh` 不接受 `-- <cmd>` 尾随命令参数** — `Cmd::Ssh` 仅接受 `node`, `--session`, `--size`。要在远程执行一条命令，请使用 `nexus exec <device> -- <cmd>` 或通过 `ssh_proxy.rs`。
- **会话默认名为 `main`** — `nexus ssh` 不指定 `--session` 时连接 `main`。断开用 Ctrl-]（0x1d），终端打印 `[已断开，会话 main 仍在运行]` 并退出 0。
- **会话时间戳字段是 `created_at_ms`** — `nexus session ls --json` 输出 `{"schema": 2, "sessions": [{"name": ..., "clients": ..., "created_at_ms": ...}]}`。
- **Agent 凭据前缀是 `nxa1-`** — 由 `nexus agent issue` 签发，包含用根密钥签发的证书、grants 和 TTL。未授权的动作（如无 grant 访问 `session ls`）会被拒绝，退出码 105（`EXIT_DENIED`）。
- **Windows PTY** — `nexus ssh` 在 Windows 上使用 `portable-pty` + `crossterm`。自动化驱动应使用 `exec` 或基于 Web 的 xterm.js 流。