# network-init-join — 网络初始化、加入与批准

## Sub-features
- `init` — 第一台设备建立网络，生成 Ed25519 根身份与一次性恢复码（32 字节十六进制，64 字符）
- `join` — 新设备生成加入码（前缀 `nxj1-`，后接 postcard 序列化后的十六进制字符串，超过 100 字符）
- `approve` — 管理设备批准加入，名册版本自增，新设备成为成员
- `ls` / `status` — 双向在线确认、名册版本前进、角色正确

## How to get to it (user POV)
```bash
# 设备 A (laptop)
nexus daemon &                    # 启动常驻程序
nexus init --name laptop          # 建立网络，得到恢复码（妥善保存）

# 设备 B (home)
nexus daemon &                    # 启动常驻程序
nexus join                        # 输出加入码（nxj1- 开头的编码字符串，非纯十六进制）

# 设备 A
nexus device approve <加入码> --name home  # 批准，指定成员名

# 双方验证
nexus ls                          # 两台设备均显示 online=true
nexus status --json               # 双方 roster_version 均为 2（或更高）；laptop: role=admin; home: role=member
```

## Driving it with verify-nexus
```bash
# Launch already done by launch-daemons.sh (laptop + home daemons on isolated data dirs)
.pi/skills/verify-nexus/helpers/cli-drive.sh "$RUN_ROOT" "$NEXUS_BIN" "$RUN_ROOT/evidence/cli"
```
The helper runs the exact commands above against the isolated daemons and writes JSON evidence to `$RUN_ROOT/evidence/cli/`:
- `init-laptop.json` — init output (root, recovery_code, schema)
- `join-home.json` — join code (starts with `nxj1-`)
- `approve-home.json` — approve result
- `ls-laptop.json`, `ls-home.json` — both show each other online
- `status-laptop.json`, `status-home.json` — roles and roster versions (both at version 2)

## Gotchas
- **加入码格式** — 不是裸的 64 字符十六进制，而是 `nxj1-` 前缀 + postcard 序列化的十六进制编码，包含临时公钥、nonce、名称和地址提示，长度超过 100 字符。
- **名册版本推进** — `approve` 会使名册版本在管理设备上自增到 2，并推送给新设备。所以批准完成后，**双方**的 `roster_version` 都是 2，而不是 laptop 保持 1。
- **Isolated data dirs required** — `launch-daemons.sh` creates `$RUN_ROOT/laptop/data` and `$RUN_ROOT/home/data`; never reuse the user's real `~/.local/share/nexus` or agent sockets.
- **Daemon must be running** — `cli-drive.sh` assumes daemons are up (doctor passes). If doctor fails, the helper exits non-zero with a clear message.
- **Recovery code is one-time** — the helper does not test recovery; that's a separate feature (`disaster-recovery` — not yet seeded).
- **Name collision** — `approve --name` must be unique; the helper uses fixed names `laptop`/`home` which are guaranteed unique in the isolated run.
- **CAP_NET_ADMIN for real backend** — in simulation (default for L2 tests) no special caps needed; real backend tests (L3) need `CAP_NET_ADMIN` for TUN device. The skill's daemons run with `--no-discovery` to avoid multicast, but still need TUN for iroh. On Windows/macOS the real backend uses a userspace transport — no caps needed.