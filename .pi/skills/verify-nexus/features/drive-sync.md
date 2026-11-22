# drive-sync — 工作区内容寻址存储与同步

## Sub-features
- `serve on` — 本机作为存储设备，保存登记到这里的工作区的完整副本（锚点通常开着）
- `create <ws> --replica <device>` — 创建一个空工作区，把完整副本保存到指定设备
- `put <ws>:/<path>` — 把标准输入写成工作区里的文件（新建或覆盖，中间的目录自动建立），`--exec` 设可执行位
- `import <dir> <ws>:/<path>` — 把本地目录导入工作区（文件、子目录、符号链接，保留可执行位）
- `commit <ws> -m <msg>` — 把还没提交的改动生成一次提交
- `sync <ws>` — 和存储设备同步：取回新的提交和目录结构（文件内容用到时再取），推送本机的提交，或报告分叉
- `merge <ws>` — 分叉时把存储设备上的提交合并进来：两边都改了的文件保留本机的版本，对方的另存为 `<文件>.conflict-<设备>`
- `where <ws>:/<path>` — 数据在哪里：副本、是否已同步、未提交的改动、本机有多少内容
- `ls [ws:]` 或 `ls <ws>:/<path>` — 列出工作区，或者工作区里的一个目录（可 `--at <提交号>` 看某次提交）
- `cat <ws>:/<path>` — 输出文件内容（可 `--offset` / `--length` 只读一段）
- `prefetch <ws>` — 按预读清单（或 `--all`）把文件内容提前取到本机
- `record <ws> --name <n>` / `--stop` — 记录这台设备读了工作区的哪些文件，得到预读清单
- `rm <ws>:/<path> [-r]` — 删除文件（目录要加 `-r`）
- `status <ws>` — 列出还没提交的改动
- `log <ws>` — 提交历史，新的在前
- `mount <ws>:/<path> <dir>` — 把工作区（或其中一个目录）挂到一个空目录上（Linux），挂载里的写入只落本机，用 `commit` 提交

> **路径写法**：所有工作区操作用 `<工作区>:/<路径>`。工作区名只能是小写字母、数字、连字符。这与早期设想的「设备间 push/pull 文件」模型不同——drive 是 Git 式的内容寻址工作区，不是文件传输工具。

## How to get to it (user POV)
```bash
# 前提：网络已建立，有一台设备开了 serve（锚点通常开着）

# 存储设备（锚点）
nexus drive serve on --json            # {"schema":..., "serving":true}

# 工作区所有者
nexus drive create work --replica <anchor> --json
echo "hello nexus drive" | nexus drive put work:/docs/hello.txt --json
nexus drive status work --json         # 显示未提交改动
nexus drive commit work -m "add hello" --json
nexus drive sync work --json           # 推送提交到锚点

# 另一台设备（新设备第一次打开工作区也用它）
nexus drive sync work --json           # 取回提交和目录结构
nexus drive ls work:/docs --json       # 列出目录
nexus drive cat work:/docs/hello.txt   # 输出 "hello nexus drive"
nexus drive where work:/docs/hello.txt --json   # 本地缓存状态

# 删除 + 提交 + 再同步
nexus drive rm work:/docs/hello.txt --json
nexus drive commit work -m "remove hello" --json
nexus drive sync work --json
```

## Driving it with verify-nexus
```bash
# cli-drive.sh 的 drive-sync 段（注意：用真实的工作区命令，不是 push/pull）
# 1. 锚点 serve on
# 2. 工作区所有者 create work --replica home
# 3. put work:/verify-test-<ts>.txt（内容 "drive sync test content"）
# 4. status work（验证文件出现）
# 5. commit work -m "add"
# 6. sync work（推送到锚点）
# 7. 另一台设备 sync work（取回）
# 8. ls work:/（验证文件出现）
# 9. cat work:/<file>（验证内容）
# 10. where work:/<file>（验证 synced=true）
# 11. rm work:/<file> + commit + sync（验证删除同步）
```
Evidence in `$RUN_ROOT/evidence/cli/`：
- `drive-serve.json`, `drive-create.json`, `drive-put.json`, `drive-status.json`,
  `drive-commit-add.json`, `drive-sync-push.json`, `drive-sync-pull.json`,
  `drive-ls.json`, `drive-cat.txt`, `drive-where.json`,
  `drive-rm.json`, `drive-commit-rm.json`, `drive-sync-final.json`

## Gotchas
- **不是 `drive push` / `drive pull`** — 这两个子命令不存在。数据摄入用 `put` / `import` + `commit`，取回用 `sync` + `cat` / `prefetch` / `mount`。`ls` 不带路径时列出工作区，不是远程设备。
- **路径是 `<工作区>:/<路径>`，不是 `<设备> <文件>`** — 工作区副本保存在开了 `serve` 的设备上，用 `--replica` 指定；`sync` 与这些副本同步。
- **内容寻址存储** — 文件被切成内容定义的块（FastCDC：最小 16 KB、平均 64 KB、最大 256 KB），以 BLAKE3 哈希索引到不可变提交快照。`where` 显示本机缓存了多少内容、是否同步。
- **懒加载** — `sync` 取回提交和目录结构，文件内容用到时（`cat` / `mount`）再取。`prefetch` 按预读清单提前取。
- **分叉用 `merge`** — 两边都改了同一文件时，本机的版本保留，对方的另存为 `<文件>.conflict-<设备>`。
- **`mount` 只落本机** — 挂载里的写入只写本机，要 `commit` 才同步出去。`record --name <n>` 卸载时保存这次读了哪些文件的清单。
- **`commit` 需要 `drive.commit` 授权** — agent 凭据要带这个 grant。
- **大文件 / 并发 / 符号链接** — 内容寻址模型天然支持大文件和符号链接（`import` 保留），但并发写同一工作区会分叉，需要 `merge`。这些是独立特性（`drive-large`、`drive-concurrent`），本特性不覆盖。