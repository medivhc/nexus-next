# anchor-relay — 锚点设备托管网页与中继

## Sub-features
- `anchor-dev` — laptop 以 `--anchor-dev <addr> --page-dir <dist>` 启动，同一端口同时服务网页资源和 iroh 中继
- `relay` — home 以 `--relay <anchor-url>` 启动，所有流量经锚点中转
- `anchor set` — `nexus anchor set <url>` 将锚点 URL 写入名册，供设备发现
- `web enroll` URL 指向锚点 — 登记链接的 origin 必须是锚点

## How to get to it (user POV)
```bash
# 设备 A (laptop) 作为锚点
nexus daemon --anchor-dev 127.0.0.1:8080 --page-dir web/dist --name laptop &

# 设备 B (home) 通过锚点中继
nexus daemon --relay http://127.0.0.1:8080 --name home &

# 建立网络
nexus init --name laptop
nexus device approve <join-code> --name home

# 将锚点 URL 写入名册（供 web enroll 生成的链接使用）
nexus anchor set http://127.0.0.1:8080

# 验证：home 的流量经 laptop 中转
nexus ls                  # 双方在线
nexus web enroll --label test  # 输出的 URL 以 http://127.0.0.1:8080/#enroll= 开头
```

## Driving it with verify-nexus
```bash
# launch-daemons.sh starts exactly this topology:
#   laptop: --anchor-dev 127.0.0.1:8080 --page-dir web/dist
#   home:   --relay http://127.0.0.1:8080
# cli-drive.sh runs: anchor set, then web enroll (verifies URL origin)
# web-drive.mjs runs: full enroll/login flow against the anchor
```
No separate helper — the topology is established by `launch-daemons.sh` and exercised by `cli-drive.sh` + `web-drive.mjs`.

## Gotchas
- **Single port for page + relay** — `--anchor-dev` serves both HTTP (web UI) and iroh relay on the same TCP port. This is a design constraint (M2.4 anchor). The helper verifies both work: `curl http://127.0.0.1:8080` returns HTML, and iroh connections from home succeed.
- **HTTPS in production** — `--anchor-dev` uses HTTP for dev. Production anchor uses `--anchor` with ACME certificates (not tested by this skill).
- **Page dir must exist** — `tools/build-web.sh` must run first. `launch-daemons.sh` checks for `web/dist/index.html`.
- **Relay URL must match anchor** — home's `--relay` URL must be the anchor's HTTP origin. Mismatch = connection refused.
- **Anchor set is idempotent** — Running `nexus anchor set` twice with same URL succeeds. The helper verifies this.
- **Discovery disabled** — `--no-discovery` is used in the skill to avoid multicast noise in isolated runs. Real networks use local discovery + relay.