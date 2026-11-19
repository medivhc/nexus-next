# Nexus 用户手册

Nexus 把你的所有设备组成一个整体：每台设备上运行一个常驻程序，设备之间按名字互相访问，
你和你的 agent 用同一套身份使用任何一台设备上的终端、服务、网络和文件。不需要配地址、端口或 SSH 密钥。

本手册按当前实现（阶段 0–5，见 `FEATURES.md`）写成。每条命令都有 `--help`，加 `--json` 得到结构化输出。

目录

1. [概念](#1-概念)
2. [安装与运行](#2-安装与运行)
3. [组网](#3-组网)
4. [终端与执行](#4-终端与执行)
5. [agent](#5-agent)
6. [发布和使用服务](#6-发布和使用服务)
7. [统一入口、路由与出口](#7-统一入口路由与出口)
8. [完整节点（虚拟网卡）](#8-完整节点虚拟网卡)
9. [锚点与网页节点](#9-锚点与网页节点)
10. [文件（Drive）](#10-文件drive)
11. [多条网络](#11-多条网络)
12. [Android](#12-android)
13. [排查问题](#13-排查问题)
14. [退出码](#14-退出码)
15. [已知限制](#15-已知限制)

---

## 1. 概念

| 概念 | 含义 |
| --- | --- |
| 网络 | 你的所有设备。第一台执行 `nexus init` 的设备建立它 |
| 管理设备 | 执行过 `nexus init` 的那台。只有它能批准和移除设备、签发 agent 凭据、登记网页通行密钥、设置锚点 |
| 名册 | 网络里有哪些设备和 agent，由管理设备签名，推送到每台设备。设备之间按名册互相认识 |
| 常驻程序 | `nexus daemon`。每台设备一个，命令行通过本机的控制套接字和它说话 |
| 锚点 | 一台常开、有公网地址的设备（通常是 VPS）：提供中继、托管网页节点的页面、保存文件的完整副本 |
| agent | 由管理设备签发的身份，有范围和期限，只能做凭据里授权的事 |
| 网页节点 | 浏览器用通行密钥登录后成为的临时节点 |

授权写作 `能力@资源`，例如 `term.exec@home`、`svc.connect@home/svc/web`。名册里你自己的设备之间默认互相可用；
agent 和网页登录只能用授权里写明的。经过一台设备的每次访问（包括被拒绝的）都记在那台设备的审计记录里。

## 2. 安装与运行

现在没有安装包，从源码构建：

```sh
cargo build --release -p nexus-cli      # 二进制在 target/release/nexus，复制到 PATH 里
```

每台设备运行常驻程序（要一直开着；开机自启需要自己配，例如写一个 systemd 单元）：

```sh
nexus daemon                            # 名字默认取主机名
nexus daemon --name home
```

常用选项：

| 选项 | 作用 |
| --- | --- |
| `--data-dir <目录>` | 数据目录，默认 `$NEXUS_DATA_DIR` 或 `~/.local/share/nexus`。命令行和常驻程序要用同一个 |
| `--relay default\|disabled\|<地址>` | 中继：iroh 的公共中继（默认）、不用，或自建的中继 |
| `--no-discovery` | 不用公共地址发现服务 |
| `--proxy 127.0.0.1:1080` | 开本机统一入口（第 7 节） |
| `--tun nexus0` | 成为完整节点（第 8 节，需要 root） |
| `--anchor <域名>` | 作为锚点运行（第 9 节） |

`nexus status` 查看本机状态；`nexus status <设备>` 查看另一台设备（在线与否、走直连还是中继、往返时间）。

## 3. 组网

```sh
# 第一台设备（它成为管理设备）
nexus init --name laptop                 # 会输出恢复码，离线保存好

# 每台新设备：输出一段加入码（10 分钟内有效）
nexus join

# 管理设备上：批准
nexus device approve <加入码> --name home

nexus ls                                 # 所有设备及其状态
nexus device revoke home                 # 把一台设备移出名册
```

加入码里带着新设备的身份和它当前的地址（直连地址和中继地址）。批准时由**管理设备主动连新设备**，
所以管理设备在 NAT 后面不影响加入。两边都在 NAT 后面时经中继连通，之后尝试打洞，打通了换成直连。

前提是**两边都能连上同一个中继**。默认的公共中继在有些网络里连不上，表现为批准时报「连不上这台设备」或超时。
这时设一个锚点（第 9 节）当中继，或者所有设备都用 `--relay <自建中继地址>` 启动。

## 4. 终端与执行

### 常驻终端会话

```sh
nexus ssh home                           # 进入 home 上的会话 main（不存在就创建）
nexus ssh home --session build           # 另一个会话
```

- 按 **Ctrl-]** 断开：会话继续在 home 上运行，下次连上看到的屏幕和断开时一样。
- 网络中断时自动重连；同一个会话可以被多个客户端同时连着，终端尺寸跟着最近一次输入的客户端。
- 会话里的 shell 退出时，`nexus ssh` 以它的退出码退出。

```sh
nexus session ls home                    # 列出 home 上的会话
nexus session kill home build            # 结束一个会话，连着的客户端随之退出
```

### 结构化执行

```sh
nexus exec home -- uname -a
nexus exec home --json -- make test      # 结束后输出完整的输出、退出码、是否超时、是否截断
nexus exec home --timeout 60 --cwd /srv/app --env RUST_LOG=info -- ./run.sh
```

命令不经过 shell；本机的标准输入转发给远端命令；退出码就是远端命令的退出码。
超时退出码 124，输出超过 `--max-output`（默认 64 MB）为 125，无法启动为 127。

### 系统自带的 ssh、scp、git

```sh
nexus ssh-config >> ~/.ssh/config
ssh home.nexus
scp file home.nexus:/tmp/
git clone home.nexus:/path/repo
```

认证由 Nexus 的身份完成，不需要任何 SSH 密钥。交互式登录进入常驻会话 main。

## 5. agent

在管理设备上签发凭据：

```sh
nexus agent issue --label builder --grant term.exec@home --grant 'term.session.*@home/session/build' --ttl 4h
nexus agent issue --label builder --grant term.exec@home --ttl 4h --out builder.cred   # 写进文件（权限 600）
nexus agent ls
nexus agent revoke builder
```

| 能力 | 资源 | 允许 |
| --- | --- | --- |
| `node.info` | `设备` | 查看状态 |
| `term.exec` | `设备` | `nexus exec` |
| `term.session.attach` / `.list` / `.kill` | `设备/session/会话名` | 进入、列出、结束终端会话 |
| `svc.connect` | `设备/svc/服务名` | 连接发布的服务 |
| `net.exit` | `设备/exit` | 经这台设备的出口出去 |
| `drive.read` / `drive.write` | `设备/drive/工作区` | 读、写工作区 |

能力末尾的星号匹配一组（`term.session.*`）；资源的一段写成星号匹配任意一段（`drive.read@*/drive/work`），
末尾的两个星号匹配其下的一切（`node.info@**`）。

agent 使用凭据：

```sh
NEXUS_AGENT=<凭据或凭据文件> nexus exec home -- make
nexus --agent builder.cred ls
nexus mcp                                # MCP 服务器（标准输入输出）：nexus_ls、nexus_status、nexus_exec、nexus_session_ls
```

agent 只能看到和做凭据里授权的事，不能做管理操作；撤销后名册推送到各设备，它的请求一律被拒绝。
凭据含 agent 的私钥，像密码一样保管。

**以独立系统账号运行的 agent**：常驻程序加 `--agent-socket <那个账号能进入的目录>/agent.sock`，
agent 一侧设置同样的 `NEXUS_AGENT_SOCKET` 和 `NEXUS_AGENT`。这样 agent 用不了你本人的控制入口。

**审计**：`nexus audit` 列出经过本机的每次访问，包括主体（设备、agent 或通行密钥的名字）、能力、资源和结果。

## 6. 发布和使用服务

在服务所在的设备上发布（服务可以只监听本机）：

```sh
nexus publish web --port 8080 --proto http
nexus publish llm --port 8000 --proto http --env OPENAI_BASE_URL=http://{addr}/v1
nexus unpublish web
nexus ls home                            # home 上你能用的服务，以及它们的用法
```

`--env` 声明使用方需要的环境变量，`{addr}` 换成使用方访问服务的地址。

在任何设备上使用：

```sh
nexus connect home/web --listen 127.0.0.1:8080   # 本机开一个端口转发过去
nexus connect home/db                            # 标准输入输出直接接到服务上
nexus run --use home/llm -- python app.py        # 先开好端口、按服务的说明设好环境变量，再运行程序
```

使用方不需要任何密钥：服务所在的设备按使用方的身份判定，密钥留在那台设备上。
`nexus run` 还会设置 `NEXUS_SVC_<设备>_<服务>=127.0.0.1:<端口>`；程序退出后端口关闭，退出码就是程序的退出码。

## 7. 统一入口、路由与出口

### 本机统一入口

```sh
nexus daemon --proxy 127.0.0.1:1080      # SOCKS5 和 HTTP 代理同一个端口，只能监听本机地址
curl -x socks5h://127.0.0.1:1080 http://web.home.internal/
curl -x http://127.0.0.1:1080 http://web.home.internal/
```

`<服务>.<设备>.internal` 是网络里发布的服务。其余连接按路由规则走。

### 路由规则

```sh
nexus route add --domain example.com --reject          # 这个域名和它的子域名
nexus route add --cidr 10.0.0.0/8 --direct
nexus route add --domain example.com --via anchor      # 经网络里的设备 anchor 出去
nexus route add --cidr 10.0.0.0/8 --socks5 127.0.0.1:7890   # 交给本机已有的代理工具
nexus route ls
nexus route explain www.example.com:443                # 这个目标会走哪里
nexus route rm 1
```

第一条匹配的规则生效，都不匹配时直连。被拒绝时 SOCKS5 回应「规则不允许」，HTTP 代理回应 403。

### 出口

```sh
nexus exit on                            # 在 anchor 上：开放出口
nexus exit status
nexus exit off
```

开放后名册里你自己的设备默认可用；agent 和网页登录要有 `net.exit@anchor/exit`。
出口不替别人连出口设备自己的本机地址（要给别人用的本机程序应当发布成服务），出口设备所在局域网的地址可以连。

## 8. 完整节点（虚拟网卡）

```sh
sudo nexus daemon --tun nexus0           # 网段默认 198.18.0.0/16，名字服务在 198.18.0.2
curl http://web.home.internal/           # 不设代理，任何程序都能用
```

- `<服务>.<设备>.internal` 解析成本机临时地址，连这个地址的任何端口都到那个服务；授权和审计同 `nexus connect`。
- 路由规则里不直连的域名也解析成本机临时地址，连接按规则走；其余名字照常解析。
- 有 systemd-resolved 时自动把 `.internal` 交给名字服务；否则按日志里的说明配置（例如 dnsmasq 的
  `server=/internal/198.18.0.2`）。
- `nexus status` 显示虚拟网卡、网段和名字服务的地址。第一版只接 TCP；`--cidr` 规则只在统一入口生效。

## 9. 锚点与网页节点

### 锚点

锚点 = 一台常开、有公网地址和域名的设备，在 443 端口上同时提供中继和网页节点的页面，证书向 Let's Encrypt 自动申请。
部署步骤见 `docs/anchor.md`，要点：

```sh
# VPS 上（域名的 A/AAAA 记录指向它，开放 80 和 443）
sudo nexus --data-dir /var/lib/nexus daemon --name anchor --anchor bewith.link --page-dir /srv/nexus/web
nexus --data-dir /var/lib/nexus join     # 再在管理设备上批准

# 管理设备上
nexus anchor set https://bewith.link     # 各设备把它加为中继；登记链接默认指向它上面的页面
nexus anchor show
```

页面文件用 `tools/build-web.sh` 生成（`web/dist`）。

### 网页节点

浏览器不用装任何东西，用通行密钥登录后成为一个临时节点，可以看到被授权的设备、进入它们的终端会话
（行为和 `nexus ssh` 一样）。浏览器的所有流量都经锚点的中继。

```sh
nexus web enroll --label phone           # 管理设备上：输出登记链接（默认 10 分钟内有效）
nexus web ls
nexus web revoke phone
```

在要用的浏览器里打开登记链接，创建通行密钥；之后在任何浏览器打开 `https://<锚点域名>/` 用它登录。

| | |
| --- | --- |
| 默认权限 | 所有设备的终端会话和状态查看（`term.session.*@**`、`node.info@**`）；用 `--grant` 改 |
| 期限 | 一次登录最长 12 小时 |
| 撤销 | 撤销后不能再登录，已有的网页连接立即断开 |
| 痕迹 | 页面不用任何浏览器存储；关掉页面，临时身份和凭证就没了 |
| 断网 | 自动重连会话 |

本机试用（不需要域名）：管理设备 `nexus daemon --anchor-dev 127.0.0.1:8080 --page-dir web/dist`，
`nexus anchor set http://localhost:8080`，然后 `nexus web enroll --label dev`，在这台机器的浏览器里打开链接。
通行密钥要求页面是 https，只有 localhost 例外。

## 10. 文件（Drive）

工作区是版本化的内容树：有名字，不属于某台设备；路径写作 `<工作区>:/<路径>`，例如 `work:/src/main.rs`。
写入只落本机，提交后才同步；存储设备（锚点通常是）保存完整副本，其他设备按需取用到的内容。

### 本机

```sh
nexus drive create work
nexus drive import ~/src/project work:/         # 导入本地目录（保留可执行位和符号链接）
echo hi | nexus drive put work:/notes/hi.txt
nexus drive ls work:/src
nexus drive cat work:/big.bin --offset 1048576 --length 4096   # 只读一段
nexus drive rm -r work:/tmp
nexus drive status work                          # 还没提交的改动
nexus drive commit work -m "整理目录"
nexus drive log work
nexus drive cat work:/notes/hi.txt --at <提交号前 8 位>         # 读旧版本
```

### 跨设备

```sh
nexus drive serve on                     # 在存储设备（例如锚点）上：保存工作区的完整副本
nexus drive create work                  # 之后创建的工作区自动以开了 serve 的设备为副本（或 --replica <设备>）
nexus drive commit work -m "..."         # 提交后自动推到存储设备
nexus drive sync work                    # 别的设备：取回历史和目录结构（内容用到时再取），或推送本机的提交
nexus drive where work:/data             # 副本在哪、是否已同步、本机有多少内容；只在本机时明确写出来
```

两台设备各自提交了对方没有的修改时，`sync` 报告**分叉**，不覆盖任何一边：

```sh
nexus drive merge work                   # 按文件三方合并；两边都改了的文件保留本机版本，对方的另存为 <文件>.conflict-<设备>
```

### 挂载（Linux）

```sh
nexus drive mount work:/ ~/work          # 前台运行；fusermount -u ~/work 或 Ctrl-C 卸载
```

编辑器、编译器直接用这个目录。挂载里的新建、修改、删除、改名、`chmod +x` 都成为本机的未提交改动，
`nexus drive commit` 之后才复制到别的设备。`drive sync` 进来的修改约 1.5 秒内在挂载里可见。
修改时间不保存在工作区里：挂载开始时是挂载的时间，内容变了才变（`make`、`cargo` 能看出改动）。

### 预读

```sh
nexus drive record work --name build     # 开始记录本机读了哪些文件
cargo build                              # 在挂载里做一次任务
nexus drive record work --stop           # 保存成清单 build，并交给存储设备
nexus drive mount work:/ ~/work --record build   # 也可以挂载期间记录，卸载时保存

nexus drive prefetch work --profile build   # 新设备上：按清单提前取好内容
nexus drive prefetch work --all             # 取整个工作区（之后可以离线用）
```

预读适合提前做（例如同步之后在后台）；在慢网络上，临用前才预读并不比边用边取快。

agent 读写工作区要有 `drive.read` / `drive.write`，`drive ls` 只列出它能读的工作区。

## 11. 多条网络

设备有多个网络接口（例如有线和 5G）时，到另一台设备的连接同时经过每个接口：

- **断一条不断会话**：一条路径不通（接口还在但没信号）时，数据在几十毫秒内换到别的路径上，会话不断；
  恢复后重新用上。
- **交互冗余**：终端会话、按键、查询这类交互流量在每条可用路径上都发，取先到的。
- **批量分摊**：大传输按各路径的容量分摊。
- **计费网络**：蜂窝网络和手机热点（`wwan`、`rmnet`、`ccmni`、`usb`、`rndis` 等接口，或系统标明的蜂窝设备）
  自动识别为计费，只承担交互流量和故障接管，不承担批量传输。

```sh
nexus path                               # 到每台设备的每条路径：本机地址、对方地址、直连或中继、往返时间、字节数
nexus path home --json
nexus iface                              # 本机的网络接口，以及是否计费（自动识别还是手动设置）
nexus iface wlan0 --metered on           # 手动设置：on、off，或 auto 回到自动识别
```

目前在 Linux 上可用。

## 12. Android

Android 上是一个应用（`link.bewith.nexus`）：系统的 VPN 是入口，手机上的任何应用都能按名字访问服务
（`http://web.home.internal/`），规则里送去出口的域名经出口设备出去，其余流量照常走手机自己的网络。

构建和安装见 `docs/android.md`。目前加入网络要用调试版系统上的命令行完成（应用界面上的加入流程还没做），
多路径和计费网络的识别在 Android 上还没做。

## 13. 排查问题

| 现象 | 看什么、怎么办 |
| --- | --- |
| 命令报退出码 101 | 本机的常驻程序没在运行，或 `--data-dir` 和常驻程序不一致 |
| 批准时连不上、超时 | 两边连不上同一个中继：设锚点或用 `--relay <自建中继>`，必要时加 `--no-discovery` |
| 一直走中继、延迟高 | `nexus status <设备>` 看 `path`；两边都是对称 NAT 时打洞会失败，把中继放在离两边近的地方 |
| 想知道连接走了哪条路 | `nexus path <设备>` |
| 访问被拒绝（105） | 看目标设备上的 `nexus audit`：记着主体、能力和资源；给 agent 或通行密钥补授权 |
| 某个域名走错了路 | `nexus route explain <主机:端口>` |
| 数据在哪、是否只有一份 | `nexus drive where <路径>` |
| 需要更多日志 | 启动常驻程序时设 `NEXUS_LOG`，例如 `NEXUS_LOG=info` 或 `NEXUS_LOG=iroh=debug`，日志写到标准错误 |

跨网络的实际表现（直连还是中继、上手要多久）可以按 `docs/real-network-check.md` 的清单测一遍。

## 14. 退出码

| 码 | 含义 |
| --- | --- |
| 0 | 成功 |
| 101 | 连不上本机的常驻程序 |
| 102 | 用法错误 |
| 103 | 不认识这台设备 |
| 104 | 内部错误 |
| 105 | 没有授权 |
| 106 | 当前状态不允许（例如没有分叉时 merge） |
| 107 | 输入不合法 |
| 108 | 连不上目标设备 |
| 109 | 不存在（工作区、文件、服务、预读清单） |
| 124 / 125 / 127 | `exec`：超时 / 输出超限 / 无法启动 |

出错时，标准错误上是一行 JSON：`{"error": <类别>, "message": <说明>}`。

## 15. 已知限制

- **还没有在真实的跨网络设备上验证过**：自动化测试都在同一台机器上（回环地址或网络命名空间）。
- 完整节点和 Android 的用户态协议栈不支持 TCP 半关闭：发完数据就关闭写方向的程序（例如 `echo ... | nc`）收不到回应。
- 完整节点和 Android 第一版只转发 TCP，不接管全部流量。
- 挂载只支持 Linux；没有硬链接、扩展属性、文件锁。合并结果不保留空目录。
- 多路径和计费网络识别只支持 Linux；两条链路的批量吞吐约为一条的 1.5 倍；经中继的路径没有测过。
- 网页节点目前只有终端和状态查看；文件浏览、打开发布的网页服务、发布摄像头等硬件能力还没做。
- 设备程序和网页页面要一起升级：协议（wire 版本 16）不匹配时连接失败。
- 还没有：凭据同步（Vault）、临时分享、硬件能力发布、远程桌面。
