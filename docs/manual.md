# Nexus 用户手册

Nexus 把你的所有设备组成一个整体。每台设备上运行一个常驻程序，设备之间按名字互相访问；
你和你的 agent 用同一套身份使用任何一台设备上的终端、服务、网络和文件。不需要配地址、端口或 SSH 密钥。

本手册按当前实现（阶段 0–9，到 M9.2 为止，见 `FEATURES.md` 和 `docs/roadmap.md`）写成。第一次用，先读第 2 节（怎么工作）和第 3 节（从零开始），
之后的章节是按功能的参考。每条命令都有 `--help`，加 `--json` 得到结构化输出。

> 手册里写成 ` ```sh laptop ` 这类带设备名的代码块，CI 每次都会在本机起几台设备、照着从上到下执行一遍
> （`tools/manual_check.py`），所以它们和当前的程序一致。没有设备名的代码块要 root、要域名或一直在前台运行，没有自动执行。

目录

1. [概念](#1-概念)
2. [怎么工作](#2-怎么工作)
3. [从零开始：三台设备](#3-从零开始三台设备)
4. [安装、运行与维护](#4-安装运行与维护)
5. [组网](#5-组网)
6. [终端与执行](#6-终端与执行)
7. [agent](#7-agent)
8. [发布和使用服务](#8-发布和使用服务)
9. [统一入口、路由与出口](#9-统一入口路由与出口)
10. [完整节点（虚拟网卡）](#10-完整节点虚拟网卡)
11. [锚点与网页节点](#11-锚点与网页节点)
12. [文件（Drive）](#12-文件drive)
13. [临时分享](#13-临时分享)
14. [Vault（秘密）](#14-vault秘密)
15. [多条网络](#15-多条网络)
16. [Android](#16-android)
17. [排查问题](#17-排查问题)
18. [退出码](#18-退出码)
19. [已知限制](#19-已知限制)

---

## 1. 概念

| 概念 | 含义 |
| --- | --- |
| 网络 | 你的所有设备。第一台执行 `nexus init` 的设备建立它 |
| 管理设备 | 有网络根密钥的那台：执行过 `nexus init` 的，或者用恢复码 `nexus recover` 过的。只有它能批准和移除设备、签发 agent 凭据、登记网页通行密钥、设置锚点 |
| 名册 | 网络里有哪些设备、哪些 agent 被撤销、哪些通行密钥、锚点在哪。由管理设备签名，推送到每台设备 |
| 常驻程序 | `nexus daemon`。每台设备一个；`nexus` 的其他命令都是通过本机的控制套接字让它去做 |
| 锚点 | 一台常开、有公网地址和域名的设备（通常是 VPS）：提供中继、托管网页节点的页面 |
| 存储设备 | 执行过 `nexus drive serve on` 的设备，保存工作区的完整副本。锚点通常兼做存储设备 |
| agent | 由管理设备签发的身份，有范围和期限，只能做凭据里授权的事 |
| 网页节点 | 浏览器用通行密钥登录后成为的临时节点 |
| 临时分享 | 一条限时、限次、只读的链接，不在网络里的人用它在浏览器里看一个文件或目录、一个网页服务或一个终端会话 |
| Vault | 你自己的秘密（密码、令牌），加密后只放在你指定的设备上，按条目授权给 agent |

授权写作 `能力@资源`，例如 `term.exec@home`、`svc.connect@home/svc/web`。名册里你自己的设备之间默认互相可用；
agent 和网页登录只能用授权里写明的。经过一台设备的每次访问（包括被拒绝的）都记在那台设备的审计记录里。

## 2. 怎么工作

### 身份和名册

每台设备第一次启动时生成自己的密钥，设备的身份就是它的公钥（`nexus ls` 里那串 8 位的就是它的开头）。
`nexus init` 在管理设备上另外生成一把**网络根密钥**，用它签名名册。设备之间连上时互相出示身份，
只接受名册里有的设备，所以**不需要 SSH 密钥或密码**：在名册里就认识，被移出名册就连不上。

名册每次变化（批准、移除、撤销 agent、登记通行密钥、设置锚点），管理设备都把新版本推给在线的设备；
不在线的设备下次和任何一台设备通信时拿到新版本（每个请求都带着名册版本，旧的一方会收到新名册）。名册有版本号，旧版本不会覆盖新版本。

### 设备之间怎么连上

连接用 QUIC，端到端加密，按设备的公钥寻址，不按 IP 地址：

1. **直连**：两台设备在同一个局域网，或至少一边有公网地址，直接连上。
2. **打洞**：两边都在 NAT 后面时，先经中继连上，同时交换各自看到的外部地址，尝试打通一条直连路径；
   打通了流量就换到直连上，中继只在一开始用到。
3. **中继**：打洞失败（例如两边都是对称 NAT、或者公司网络只放行 443 端口）时一直经中继。

中继默认是 iroh 的公共中继；设了锚点（第 11 节）之后，锚点也是中继。设备的地址变化（换网络、休眠后醒来）时
通过中继和地址发现服务重新找到对方，连接自动恢复，终端会话不受影响。

`nexus ls` 和 `nexus status <设备>` 显示每台设备现在是直连还是经中继，`nexus path <设备>` 显示每条路径的细节。

### 加入码

`nexus join` 生成一个一次性的随机数，和新设备的身份、地址一起组成**长加入码**。谁拿着它，谁就能让这台设备
接受自己的名册（加入自己的网络），所以加入码要在 10 分钟内交给管理设备，不要贴到公开的地方。

**短加入码**是 10 个字符（50 位随机）。新设备从短码派生两把密钥：一把决定碰头服务上的位置并签名，一把加密。
它把加密后的长加入码放在那个位置上；管理设备输入短码，派生出同样的密钥，取回、解密，之后和长码完全一样。
碰头服务和任何看得到它的人都只看到密文；派生故意做得慢（Argon2id），从公开的数据反推短码要上百万年的计算。

### 中继和锚点能看到什么

中继只转发加密后的数据包，看不到内容，也不能冒充任何一台设备。锚点同时是名册里的一台普通设备：
它只能做你授权给它的事，和其他设备一样；网页节点的页面从锚点下载，页面里的程序在浏览器里和设备直接建立加密连接，
锚点也只是转发。

### 数据存在哪里

每台设备的状态在它的数据目录里（默认 `~/.local/share/nexus`）：

| 文件 | 内容 |
| --- | --- |
| `device_key.bin` | 这台设备的私钥 |
| `root_key.bin` | 网络根密钥，**只在管理设备上有** |
| `roster.bin`、`root_pub.bin` | 名册和根公钥 |
| `agents.bin` | 管理设备签发过的 agent |
| `shares-v1.bin` | 这台设备创建的临时分享（到期时间、次数、是否撤销） |
| `vault-v1.bin` | 这台设备被授权的 Vault 条目（只有密文）；管理设备上还有每条给过哪些设备 |
| `drive-v2.bin`、`cas/` | 工作区的提交、目录和本机有的文件内容 |
| `ctl.sock` | 命令行和常驻程序之间的控制套接字 |
| `upgrade.json` | 最近一次升级的状态（`nexus upgrade --status`） |
| `sessions/` | 每个终端会话的持有进程的套接字、pid 和日志 |

每个终端会话由一个独立的**持有进程**管理（同一个 `nexus` 二进制，`ps` 里显示为 `nexus ... session-hold`）：
常驻程序重启（升级、崩溃、被结束）时，会话和里面的程序照常运行，常驻程序回来后自动接管，连着的客户端几秒内接回，
屏幕和之前一样。设备本身重启时会话结束。

### 管理设备和恢复码

管理设备是整个网络唯一能改名册的地方。`nexus init` 输出的**恢复码就是网络根密钥本身**（十六进制），只显示这一次。

恢复码要离线保存（打印出来或记在密码管理器里）。管理设备丢失或数据目录损坏时，现有的设备照常互相访问
（它们有名册），只是没有设备能改名册了。这时在**网络里任何一台还在的设备**上：

```sh
nexus recover <恢复码>                   # 空格和 - 忽略，不区分大小写，抄写时可以分组
nexus device revoke laptop               # 再把丢了的那台移出名册
```

这台设备成为管理设备：名册里它显示为管理设备、原来的显示为成员，新名册推送到各设备。之后它能做管理设备的所有事。

- 原来签发过的 agent 的列表只在原来的管理设备上：`nexus agent ls` 看不到它们，它们的凭据照常有效到期限为止。
  要撤销其中一个，在用过它的设备上 `nexus audit --json` 找到它的 `agent_id`，再 `nexus agent revoke <agent_id>`。
- 恢复码不属于这个网络、格式不对、本机已经是管理设备、或本机还不属于任何网络时，什么都不改并说明原因。
- **网络里一台设备都不剩时，恢复码没有用处**：重新 `nexus init` 建一个网络。
- 恢复码（或者原来的管理设备）落到别人手里，等于网络被接管：目前还不能更换根密钥，只能重新建网络。

## 3. 从零开始：三台设备

例子：

- **laptop**：笔记本，在家里或公司的 NAT 后面，当管理设备；
- **home**：家里的台式机，也在 NAT 后面；
- **vps**：一台云服务器，有公网地址。

先在每台设备上装好 `nexus`（第 4 节：下载预编译版本或从源码构建），然后：

### 第一步：建立网络

在 laptop 上启动常驻程序，建立网络。laptop 成为管理设备：

```sh laptop
nexus daemon --name laptop &
nexus init
```

`init` 会输出恢复码，离线保存好（第 2 节）。

### 第二步：加入 home 和 vps

在 home 上启动常驻程序，让它输出加入码：

```sh home
nexus daemon --name home &
nexus join
```

输出的第一个是**短加入码**，例如 `7KQ4M-9XZ21`，10 分钟内有效。把它告诉 laptop（念出来、手敲都行，
不区分大小写，分隔符可有可无），在 laptop 上批准：

```sh laptop
nexus device approve <加入码> --name home
```

短码经碰头服务换成 home 的身份和地址（第 2 节「加入码」），然后 **laptop 主动去连 home**：
先经 home 连着的中继，所以两边都在 NAT 后面也能加入。
连上之后 laptop 把签好的名册交给 home，home 从此认识网络里的所有设备。vps 同样：

```sh vps
nexus daemon --name vps &
nexus join
```

```sh laptop
nexus device approve <加入码> --name vps
nexus ls
nexus status home
```

`nexus ls` 列出所有设备：在线与否、直连还是经中继、往返时间。home 和 laptop 都在 NAT 后面时，
一开始是「经中继」，打洞成功后几秒内变成「直连」。

### 第三步：用另一台设备的终端

```sh laptop
nexus exec home -- uname -a
```

进入 home 上的常驻终端会话（交互式，不在 CI 里执行）：

```sh
nexus ssh home          # Ctrl-] 断开，会话继续在 home 上运行；再次 nexus ssh home 回到原来的屏幕
```

让系统自带的 ssh、scp、git 也能用（`~/.ssh/config` 里加一段，之后用 `<设备>.nexus`）：

```sh laptop
mkdir -p ~/.ssh && nexus ssh-config >> ~/.ssh/config
```

### 第四步：发布一个服务

home 上有一个只监听本机的网页服务（这里用 Python 自带的，在 8000 端口），把它发布给网络：

```sh home
python3 -m http.server 8000 --bind 127.0.0.1 >/dev/null 2>&1 &
sleep 1
nexus publish files --port 8000 --proto http
```

在 laptop 上使用。服务不用对外开端口，也不用密码：home 按 laptop 的身份判定能不能用：

```sh laptop
nexus ls home
nexus run --use home/files -- sh -c 'curl -s http://$NEXUS_SVC_HOME_FILES/ | head -3'
```

`nexus run` 在本机开好一个端口转发到服务，把地址放在 `NEXUS_SVC_HOME_FILES` 里，再运行你的程序；
程序退出端口就关了。要一个一直开着的端口用 `nexus connect home/files --listen 127.0.0.1:9000`（前台运行，Ctrl-C 结束）。

### 第五步：在设备之间同步文件

vps 一直开着，让它当存储设备，保存工作区的完整副本：

```sh vps
nexus drive serve on
```

在 laptop 上建一个工作区，写一个文件，提交。提交后自动推到 vps：

```sh laptop
nexus drive create notes
echo "买牛奶" | nexus drive put notes:/todo.txt
nexus drive commit notes -m "第一条"
nexus drive where notes:/todo.txt
```

home 取回，读到同一个文件。这时 laptop 可以关机：内容在 vps 上：

```sh home
nexus drive sync notes
nexus drive cat notes:/todo.txt
```

在 Linux 上可以把工作区挂成一个目录，编辑器和编译器直接用（第 12 节）。

### 第六步：给 agent 一个受限的身份

让一个自动化程序只能在 home 上执行命令，1 小时后失效：

```sh laptop
nexus agent issue --label ci --grant term.exec@home --ttl 1h --out ci.cred
NEXUS_AGENT=ci.cred nexus exec home -- echo 来自 agent
NEXUS_AGENT=ci.cred nexus exec vps -- true || echo "退出码 $?"
```

第二条命令失败：凭据里没有 vps，对这个 agent 来说 vps 不存在（退出码 103）。
home 的审计记录里能看到 agent 做过什么：

```sh home
nexus audit
```

### 第七步（可选）：锚点和浏览器

vps 有域名（例如 `bewith.link`）时，把它设为锚点：它在 443 端口上同时提供中继和网页节点的页面，
之后所有设备优先用它做中继，任何浏览器都能用通行密钥登录进入任一台设备的终端。步骤见第 11 节。

### 之后

- 每台设备的常驻程序要一直开着：配成开机自启（第 4 节）。
- 新设备重复第二步。设备丢了：在 laptop 上 `nexus device revoke <设备>`。
- 网络有两个出口（例如有线和 5G）时什么都不用配，见第 15 节。

## 4. 安装、运行与维护

### 下载预编译版本（Linux）

仓库的 Releases 页面有每个版本的静态二进制：`linux-x86_64` 和 `linux-aarch64`（ARM 服务器、树莓派），
不依赖系统库，任何发行版都能用。压缩包里有 `nexus`、本手册和许可证。

```sh
tar xzf nexus-0.1.0-linux-x86_64.tar.gz
sudo install nexus-0.1.0-linux-x86_64/nexus /usr/local/bin/
sha256sum -c SHA256SUMS --ignore-missing      # 和 Releases 页面上的 SHA256SUMS 一起下载时校验
nexus --version
```

所有设备要用同一个版本（第 4 节「升级」）。

### 从源码构建

其他系统，或者要改代码时（需要 Rust 工具链）：

```sh
cargo build --release -p nexus-cli      # 二进制在 target/release/nexus
sudo install target/release/nexus /usr/local/bin/
```

挂载和多路径只在 Linux 上可用；macOS 和 Windows 还没有试过能不能编译。

### 运行常驻程序

```sh
nexus daemon                            # 名字默认取主机名
nexus daemon --name home
```

| 选项 | 作用 |
| --- | --- |
| `--name <名字>` | 设备名（字母、数字、`-`、`_`），默认取主机名 |
| `--data-dir <目录>` | 数据目录，默认 `$NEXUS_DATA_DIR` 或 `~/.local/share/nexus`。命令行要用同一个（同样的选项或环境变量） |
| `--relay default\|disabled\|<地址>` | 中继：iroh 的公共中继（默认）、不用，或自建的中继 |
| `--no-discovery` | 不用公共地址发现服务（也就默认不用公共的碰头服务） |
| `--relay-only` | 只经中继通信，不开 UDP 直连（禁用 UDP 的沙箱、受限网络） |
| `--rendezvous default\|disabled\|<地址>` | 短加入码用的碰头服务：公共的（用了地址发现时的默认）、不用，或自建的（锚点的 `https://<锚点>/pkarr`） |
| `--proxy 127.0.0.1:1080` | 开本机统一入口（第 9 节） |
| `--tun nexus0` | 成为完整节点（第 10 节，需要 root） |
| `--anchor <域名>` | 作为锚点运行（第 11 节） |
| `--agent-socket <路径>` | 给其他系统账号下的 agent 用的入口（第 7 节） |

`nexus status` 查看本机状态；`nexus status <设备>` 查看另一台设备（在线与否、走直连还是中继、往返时间）。
排查问题时设 `NEXUS_LOG=info` 再启动，日志写到标准错误。
连中继和地址发现服务时，除了内置的根证书，还信任 `$SSL_CERT_FILE` 指的证书包（没设就用系统的，如 `/etc/ssl/certs/ca-certificates.crt`）：
企业代理或沙箱的出站代理换了证书时也连得上。

### 开机自启（systemd）

以你自己的账号运行（终端会话里的 shell 是这个账号的）。`~/.config/systemd/user/nexus.service`：

```ini
[Unit]
Description=Nexus
After=network-online.target

[Service]
ExecStart=/usr/local/bin/nexus daemon --name home
Restart=on-failure
# 只结束常驻程序本身：终端会话的持有进程要在重启服务时留下来
KillMode=process

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now nexus
loginctl enable-linger $USER            # 没有登录时也运行
```

锚点和完整节点要 root，写成系统服务（`/etc/systemd/system/nexus.service`，`ExecStart` 里加 `--data-dir /var/lib/nexus`）。

### 升级

下载新版本的压缩包（和 `SHA256SUMS` 放在一起），在要升级的设备上：

```sh
nexus upgrade --file nexus-0.2.0-linux-x86_64.tar.gz   # 也可以直接给 nexus 二进制
nexus upgrade --status                                  # 最近一次升级：pending、confirmed 或 rolled_back
nexus upgrade --rollback                                # 换回上一个版本
```

常驻程序先检查新版本（能运行、协议主版本相同、能读数据目录；有 `SHA256SUMS` 时校验），不合格就不换并说明原因（107）。
合格的话把自己的二进制换掉（原来的留作 `<二进制>.prev`），用同样的参数重新启动；终端会话不断。
新版本要在 60 秒内（`--confirm-within`）连上升级前在线的设备中的一台，连不上就自动换回原来的版本再启动，
`nexus upgrade --status` 里写着原因。新版本启动就崩溃时，要有 systemd 这样的管理程序把它重新拉起来：拉起 3 次还没确认就换回。

常驻程序要能写它的二进制所在的目录（装在 `/usr/local/bin` 时常驻程序要以 root 运行，或者把 nexus 装在自己的目录里）。
目前只在 Linux 和其他 Unix 上；数据目录里的密钥、名册和工作区都保留。不用 `nexus upgrade` 也可以：停掉常驻程序，换二进制，再启动。

**从一台设备升级其他设备**：新版本只要下载到一台设备上，经 Nexus 送到其他设备：

```sh
nexus upgrade --file nexus-0.2.0-linux-x86_64.tar.gz --to home,work   # 指定的设备
nexus upgrade --file nexus-0.2.0-linux-x86_64.tar.gz --all            # 其他所有在线的设备（按名字）
nexus upgrade --file nexus-0.2.0-linux-x86_64.tar.gz                  # 最后升级本机
```

`--to` 和 `--all` 不升级本机。一台一台地升级：每台按上面的方式检查、换上、确认，命令等到它确认或换回再升级下一台；
一台换回了就不再继续，后面的设备标为没有升级。每台设备一行结果（`--json` 时是 `devices` 数组，
`state` 是 `confirmed`、`rolled_back`、`failed`、`timed_out` 或 `skipped`）。全部确认时退出码 0，
否则是第一台没确认的设备的原因：检查不过 107、换回了 106、设备不存在 103、等不到结果 124、那台设备的版本较旧还不支持这样升级 110
（先在它上面手动升级一次）。目标设备要和新版本是同一种系统和架构（不是的话检查时就不过）；
调用者要能在那台设备上执行命令（`nexus exec`）。

常驻程序执行的命令都带着两个环境变量：`NEXUS_EXE`（常驻程序自己的二进制）和 `NEXUS_DATA_DIR`（它的数据目录），
所以 `nexus exec home -- sh -c '"$NEXUS_EXE" upgrade --status'` 这样的命令问的是 home 上的常驻程序。

**设备可以一台一台升级**：协议主版本相同的新旧版本互相连得上，日常操作（列出设备、终端、执行、服务、工作区同步）照常。
只在新版本才有的功能用到旧版本的设备上时，命令以退出码 110 结束，说明哪台设备版本较旧、要升级。
`nexus ls` 里版本和本机不同的设备会标出它的版本。

协议主版本变了的发布（发布说明里会写明）是例外：新旧设备互相连不上，要一起升级。0.1.0 之前的开发版本和 0.1.0 不兼容。
网页节点的页面由锚点提供，和锚点同一个版本；浏览器在升级后要重新登录。

### 备份

- 管理设备：离线保存恢复码就够了（第 2 节）；要保留签发过的 agent 列表和 Vault 条目，再备份数据目录里的 `agents.bin` 和 `vault-v1.bin`。
- 其他设备不需要备份身份：丢了就 `nexus device revoke` 再重新加入。工作区里的内容只要提交过并且有存储设备，就在存储设备上。

### 移除一台设备

在管理设备上 `nexus device revoke <设备>`：名册推到各设备后，其他设备不再接受它的连接和请求。
它自己的数据目录不会被远程清除；要彻底清掉，在那台设备上停掉常驻程序并删除数据目录。

## 5. 组网

```sh
nexus init --name laptop                 # 第一台设备，成为管理设备（--name 可省略，用常驻程序的名字）
nexus join                               # 新设备：输出短加入码和长加入码（10 分钟内有效）
nexus device approve 7KQ4M-9XZ21 --name home   # 管理设备上：用短码或长码批准
nexus ls                                 # 所有设备及其状态
nexus device revoke home                 # 把一台设备移出名册
```

批准时由**管理设备主动连新设备**，所以管理设备在 NAT 后面不影响加入（第 2 节）。

**短加入码和长加入码**：短码（`XXXXX-XXXXX`）要经碰头服务，默认用 iroh 公共的地址发现服务；长码（`nxj2-` 开头）
自己带着新设备的身份和地址，不需要任何外部服务。两种用哪个都行：

- 新设备关了地址发现（`--no-discovery`）又没指定碰头服务时，`join` 只给出长码，并说明原因。
- 管理设备连不上碰头服务时（例如它所在的网络挡了外网），用长码。
- 自建网络（不用公共服务）时，所有设备用 `--rendezvous https://<锚点>/pkarr` 启动，短码经锚点交换（第 11 节）。

一台设备同时只有一个有效的加入码：再执行一次 `join`，之前的短码和长码都作废；用过的短码也立即作废。

前提是**两边都能连上同一个中继**。默认的公共中继在有些网络里连不上，表现为批准时报「连不上这台设备」或超时。
这时设一个锚点（第 11 节）当中继，或者所有设备都用 `--relay <自建中继地址>` 启动。

## 6. 终端与执行

### 常驻终端会话

```sh
nexus ssh home                           # 进入 home 上的会话 main（不存在就创建）
nexus ssh home --session build           # 另一个会话
```

- 按 **Ctrl-]** 断开：会话继续在 home 上运行，下次连上看到的屏幕和断开时一样。
- 网络中断时自动重连；同一个会话可以被多个客户端同时连着，终端尺寸跟着最近一次输入的客户端。
- 会话里的 shell 退出时，`nexus ssh` 以它的退出码退出。
- 远端的常驻程序重启时会话不受影响，`nexus ssh` 自动接回。断开期间会话里的 shell 退出了的，重连时提示会话已经结束（退出码 109），
  不会悄悄换成一个新的会话。

```sh
nexus session ls home                    # 列出 home 上的会话
nexus session kill home build            # 结束一个会话，连着的客户端随之退出
```

### 结构化执行

```sh laptop
nexus exec home -- uname -a
nexus exec home --json -- echo hi        # 结束后输出完整的输出、退出码、是否超时、是否截断
nexus exec home --timeout 60 --cwd /tmp --env RUST_LOG=info -- sh -c 'pwd; echo $RUST_LOG'
```

命令不经过 shell（要 shell 就像上面一样写 `sh -c`）；本机的标准输入转发给远端命令；退出码就是远端命令的退出码。
超时退出码 124，输出超过 `--max-output`（默认 64 MB）为 125，无法启动为 127。

### 系统自带的 ssh、scp、git

```sh
nexus ssh-config >> ~/.ssh/config
ssh home.nexus
scp file home.nexus:/tmp/
git clone home.nexus:/path/repo
```

认证由 Nexus 的身份完成，不需要任何 SSH 密钥。交互式登录进入常驻会话 main。

## 7. agent

在管理设备上签发凭据：

```sh laptop
nexus agent issue --label builder --grant term.exec@home --grant 'term.session.*@home/session/build' --ttl 4h --out builder.cred
nexus agent ls
nexus agent revoke builder               # 标签，或者 agent 的完整标识（nexus audit --json 里的 agent_id）
```

不加 `--out` 时凭据直接打印出来。凭据含 agent 的私钥，像密码一样保管；`--out` 写的文件权限是 600。

| 能力 | 资源 | 允许 |
| --- | --- | --- |
| `node.info` | `设备` | 查看状态 |
| `term.exec` | `设备` | `nexus exec` |
| `term.session.attach` / `.list` / `.kill` | `设备/session/会话名` | 进入、列出、结束终端会话 |
| `svc.connect` | `设备/svc/服务名` | 连接发布的服务 |
| `net.exit` | `设备/exit` | 经这台设备的出口出去 |
| `drive.read` / `drive.write` | `设备/drive/工作区` | 读、写工作区 |
| `vault.read` | `设备/vault/条目` | 在那台设备上 `nexus vault get` / `ls` 这个条目（第 14 节） |

能力末尾的星号匹配一组（`term.session.*`）；资源的一段写成星号匹配任意一段（`drive.read@*/drive/work`），
末尾的两个星号匹配其下的一切（`node.info@**`）。

agent 使用凭据（凭据文本或凭据文件的路径都可以）：

```sh
NEXUS_AGENT=<凭据或凭据文件> nexus exec home -- make
nexus --agent builder.cred ls
nexus mcp                                # MCP 服务器（标准输入输出）：nexus_ls、nexus_status、nexus_exec、nexus_session_ls
```

- agent 只能看到和做凭据里授权的事：没有授权的设备对它来说不存在（103），授权了设备但没有这项能力是 105。
- agent 不能做任何管理操作；撤销后名册推送到各设备，它的请求一律被拒绝；过了期限自动失效。

**以独立系统账号运行的 agent**：常驻程序加 `--agent-socket <那个账号能进入的目录>/agent.sock`，
agent 一侧设置同样的 `NEXUS_AGENT_SOCKET` 和 `NEXUS_AGENT`。这个入口只接受 agent 凭据，agent 用不了你本人的控制入口。

**审计**：`nexus audit` 列出经过本机的每次访问：时间、主体（设备、agent 或通行密钥的名字）、能力、资源和结果。

## 8. 发布和使用服务

在服务所在的设备上发布（服务可以只监听本机）：

```sh home
nexus publish web --port 8080 --proto http
nexus publish llm --port 8000 --proto http --env OPENAI_BASE_URL=http://{addr}/v1
nexus unpublish web
nexus ls home                            # home 上你能用的服务，以及它们的用法
```

`--env` 声明使用方需要的环境变量，`{addr}` 换成使用方访问服务的地址。

在任何设备上使用：

```sh
nexus open home/web                              # 在本机浏览器里打开网页服务（前台运行，Ctrl-C 停止）
nexus open home/web/admin?tab=2                  # 打开服务上的某个页面
nexus connect home/web --listen 127.0.0.1:8080   # 本机开一个端口转发过去（前台运行）
nexus connect home/db                            # 标准输入输出直接接到服务上
```

`nexus open` 只用于 `--proto http` 发布的服务：在 `127.0.0.1` 上开一个端口（`--port` 指定，默认挑空闲的），
用 `$BROWSER`（没有时用系统默认的浏览器）打开网址，并把网址显示出来。没给路径时，服务的说明里有
`http://{addr}...` 形式的环境变量就打开那个网址（例如上面的 llm 打开 `/v1`），否则打开根路径。
在服务器上用 `--no-browser` 只显示网址；浏览器打不开时也照样服务。浏览器发的 `Host` 是 `127.0.0.1:<端口>`，
只认自己域名的服务可能拒绝，这时用统一入口（`web.home.internal`，第 9 节）。

```sh laptop
nexus run --use home/llm -- sh -c 'echo $OPENAI_BASE_URL'   # 开好端口、按服务的说明设好环境变量，再运行程序
```

使用方不需要任何密钥：服务所在的设备按使用方的身份判定，密钥留在那台设备上。
`nexus run` 还会设置 `NEXUS_SVC_<设备>_<服务>=127.0.0.1:<端口>`（名字转成大写，其他字符换成 `_`）；
可以给多个 `--use`；程序退出后端口关闭，退出码就是程序的退出码。

### 远程桌面

Nexus 不自己传画面，而是让现成的远程桌面（VNC 或 RDP）经 Nexus 的网络连起来：被看的设备上打开系统自带的
屏幕共享或装一个 VNC 服务端，Nexus 把它发布成服务；看的设备上 Nexus 开一个本机端口，启动现成的查看器。
连接经过 Nexus（加密、按身份判定、有审计），远程桌面服务端不需要对外开端口。

先在被看的设备上打开远程桌面服务端：

- macOS：系统设置 → 通用 → 共享 → 打开「屏幕共享」（VNC，端口 5900；登录用这台 Mac 的账户）。
- Windows（专业版）：设置 → 系统 → 远程桌面（RDP，端口 3389）。家庭版没有，可以装一个 VNC 服务端。
- Linux：`wayvnc 127.0.0.1`（Wayland）、`x11vnc -localhost -usepw`（X11），或者 GNOME 设置里的远程桌面（RDP）。

能只监听本机（127.0.0.1）的就只监听本机：经 Nexus 的连接都从本机进来。
下面两段分别在被看的设备（home）和看的设备（laptop）上运行。

```sh
nexus desktop share                        # 依次试本机的 5900、5901、3389，认出 VNC 或 RDP，发布成 home/desktop
nexus desktop share --port 5902 --proto vnc --name screen2   # 别的端口；给了 --proto 就不识别
```

```sh
nexus desktop ls                           # 在线设备上能用的远程桌面
nexus desktop open home                    # 开本机端口、启动查看器（前台运行，Ctrl-C 停止）
nexus desktop open home/screen2 --no-viewer --port 5999   # 只开端口，自己用查看器连 127.0.0.1:5999
```

`nexus desktop open` 显示的地址是 `vnc://127.0.0.1:<端口>` 或 `rdp://127.0.0.1:<端口>`。查看器：设置了
`NEXUS_DESKTOP_VIEWER` 就执行它（地址作为最后一个参数）；否则 macOS 用系统的「屏幕共享」或 Windows App，
Windows 用 `mstsc`（RDP）或 `vncviewer`（VNC），Linux 依次试 `vncviewer`、`remmina`、`xdg-open`（VNC）或
`xfreerdp3`、`xfreerdp`、`remmina`（RDP）。找不到查看器时照样服务，并说明手动连哪个地址。
`nexus open home/desktop` 遇到远程桌面也是一样。也可以不用 `nexus desktop share`，直接
`nexus publish desktop --port 5900 --proto vnc`。

远程桌面服务端自己的登录（VNC 密码、系统账户）照常要输；Nexus 只负责连通。网页里还不能看远程桌面。

## 9. 统一入口、路由与出口

### 本机统一入口

```sh
nexus daemon --proxy 127.0.0.1:1080      # SOCKS5 和 HTTP 代理同一个端口，只能监听本机地址
curl -x socks5h://127.0.0.1:1080 http://web.home.internal/
curl -x http://127.0.0.1:1080 http://web.home.internal/
```

`<服务>.<设备>.internal` 是网络里发布的服务。其余连接按路由规则走。

### 路由规则

```sh laptop
nexus route add --domain example.com --reject          # 这个域名和它的子域名
nexus route add --cidr 10.0.0.0/8 --direct
nexus route add --domain example.org --via vps         # 经网络里的设备 vps 出去
nexus route add --cidr 172.16.0.0/12 --socks5 127.0.0.1:7890   # 交给本机已有的代理工具
nexus route ls
nexus route explain www.example.com:443                # 这个目标会走哪里
nexus route rm 1
```

按编号从小到大，第一条匹配的规则生效，都不匹配时直连。被拒绝时 SOCKS5 回应「规则不允许」，HTTP 代理回应 403。
规则对本机的统一入口和完整节点生效。

### 出口

```sh vps
nexus exit on                            # 开放出口
nexus exit status
nexus exit off
```

开放后名册里你自己的设备默认可用；agent 和网页登录要有 `net.exit@vps/exit`。
出口不替别人连出口设备自己的本机地址（要给别人用的本机程序应当发布成服务），出口设备所在局域网的地址可以连。

## 10. 完整节点（虚拟网卡）

```sh
sudo nexus daemon --tun nexus0           # 网段默认 198.18.0.0/16（--tun-net 改），名字服务在 198.18.0.2
curl http://web.home.internal/           # 不设代理，任何程序都能用
```

- `<服务>.<设备>.internal` 解析成本机临时地址，连这个地址的任何端口都到那个服务；授权和审计同 `nexus connect`。
- 路由规则里不直连的域名也解析成本机临时地址，连接按规则走；其余名字照常解析。
- 有 systemd-resolved 时自动把 `.internal` 交给名字服务；否则按日志里的说明配置（例如 dnsmasq 的
  `server=/internal/198.18.0.2`）。
- `nexus status` 显示虚拟网卡、网段和名字服务的地址。第一版只接 TCP；`--cidr` 规则只在统一入口生效。

## 11. 锚点与网页节点

### 锚点

锚点 = 一台常开、有公网地址和域名的设备，在 443 端口上同时提供中继和网页节点的页面，证书向 Let's Encrypt 自动申请。
完整的部署步骤见 `docs/anchor.md`，要点：

```sh
# VPS 上（域名的 A/AAAA 记录指向它，开放 80 和 443）
tools/build-web.sh                       # 生成页面文件 web/dist，复制到 VPS 上，例如 /srv/nexus/web
sudo nexus --data-dir /var/lib/nexus daemon --name vps --anchor bewith.link --page-dir /srv/nexus/web
sudo nexus --data-dir /var/lib/nexus join     # 再在管理设备上批准

# 管理设备上
nexus anchor set https://bewith.link     # 各设备把它加为中继；网页登记链接默认指向它上面的页面
nexus anchor show
```

已有证书时用 `--tls-cert`、`--tls-key`，不自动申请；先试一遍流程可以加 `--acme-staging`。

锚点还在 `https://<锚点域名>/pkarr` 提供**碰头服务**（短加入码用）：不想依赖公共服务时，所有设备用
`--rendezvous https://<锚点域名>/pkarr --no-discovery` 启动。记录只保存 15 分钟。

### 网页节点

浏览器不用装任何东西，用通行密钥登录后成为一个临时节点，可以看到被授权的设备、进入它们的终端会话
（行为和 `nexus ssh` 一样）、打开被授权的网页服务、浏览被授权的工作区，还可以在你逐项同意后把浏览器所在设备的摄像头、麦克风、位置开放给自己的设备。浏览器的所有流量都经锚点的中继。

```sh laptop
nexus web enroll --label phone --page http://localhost:8080   # 管理设备上：输出登记链接（默认 10 分钟内有效）
nexus web ls
```

在要用的浏览器里打开登记链接，创建通行密钥，登记就完成了（之后才出现在 `nexus web ls` 里）；
之后在任何浏览器打开 `https://<锚点域名>/` 用它登录。设了锚点时不用 `--page`。`nexus web revoke phone` 撤销。

| | |
| --- | --- |
| 默认权限 | 所有设备的终端会话和状态查看（`term.session.*@**`、`node.info@**`）；用 `--grant` 改 |
| 期限 | 一次登录最长 12 小时 |
| 撤销 | 撤销后不能再登录，已有的网页连接立即断开 |
| 痕迹 | 页面不用任何浏览器存储；关掉页面，临时身份和凭证就没了 |
| 断网 | 自动重连会话 |
| 网页服务 | 设备后面列出它的网页服务，点开在新标签页打开 |
| 工作区 | 能读的工作区列在设备下面：逐级浏览目录，文本和图片直接预览（只取开头 1 MiB），任何文件都能下载；只读 |
| 硬件 | 摄像头、麦克风、位置默认都关着，逐项开启；页面显示哪台设备在用 |

**网页服务**：设备后面列出它上面用 `--proto http` 发布的服务，点开在新标签页里打开（地址是 `/svc/<随机令牌>/`），
页面里的请求都经 Nexus 到那个服务；登录的页面要一直开着。服务设置的 cookie 只记在登录页面的内存里，
服务的页面在浏览器里存的东西在登录页面关闭时清掉。不支持 WebSocket 和流式回应（回应读完才显示），
页面里写死 `http://localhost:<端口>/…` 的完整网址也到不了服务。需要授权 `svc.connect`（例如 `--grant 'svc.connect@home/svc/web'`）。

页面里看到的工作区是存储设备（`nexus drive serve on`，第 12 节）上最近同步的提交，还没提交或同步的改动看不到。
默认权限不含读工作区，登记时要明确给出，例如：

```sh
nexus web enroll --label phone --grant 'term.session.*@**' --grant 'node.info@**' --grant 'drive.read@**' --grant 'svc.connect@**'
nexus web enroll --label kiosk --grant 'node.info@**' --grant 'drive.read@anchor/drive/docs'   # 只能看 anchor 上的 docs
```

### 硬件能力（摄像头、麦克风、位置）

登录后的页面有「本机硬件」一栏。摄像头、麦克风、位置三项**默认都关着**，每项要单独点「开启」，再在浏览器的权限弹窗里同意。
开了的那一项，名册里的设备可以取用：摄像头拍一张、麦克风录一小段、位置报一次坐标。

- 页面上实时显示哪台设备正在用、最近谁用过。
- 点「关闭」或者关掉、离开页面，立即停止采集，之后的请求一律拒绝。
- 不在名册里的节点什么都拿不到；没开的那项也拿不到（105）。

设备上用 `nexus hw` 取用。参数是网页节点这次登录的临时标识和地址提示，**页面上「本机硬件」下面直接给出了这两条命令**，
复制到任何一台设备上执行就行：

```sh
nexus hw ls <网页节点标识> --hint <地址提示>                  # 开了哪几项
nexus hw capture <网页节点标识> location --hint <地址提示>    # 换成 camera、microphone 也行；--json 里带原始数据（base64）
```

摄像头拍一张 JPEG，麦克风录 3 秒，位置给经纬度和精度。标识每次登录都不一样，重新登录后要用页面上新的命令。
agent 不能用 `nexus hw`。

### 页面放在哪

| 方式 | 适合 |
| --- | --- |
| 锚点（`--anchor <域名> --page-dir`） | 正式使用：https、任何地方的浏览器都能打开 |
| 本机试用（`--anchor-dev 127.0.0.1:3340 --page-dir web/dist`，再 `nexus anchor set http://localhost:3340`） | 没有域名时在这台机器的浏览器里试：页面和中继都在本机 |
| `nexus web serve --dir web/dist`（默认 127.0.0.1:8080，按 Ctrl-C 停止） | 只托管页面，中继用名册里的锚点或公共中继；登记时加 `--page http://localhost:8080` |

通行密钥要求页面是 https，只有 localhost 例外，所以后两种只能在这台机器上的浏览器里用。

## 12. 文件（Drive）

工作区是版本化的内容树：有名字，不属于某台设备；路径写作 `<工作区>:/<路径>`，例如 `work:/src/main.rs`。
写入只落本机，提交后才同步；存储设备保存完整副本，其他设备只取用到的内容。

### 本机

```sh laptop
nexus drive create work
echo hi | nexus drive put work:/notes/hi.txt
nexus drive ls work:/
nexus drive status work                          # 还没提交的改动
nexus drive commit work -m "整理目录"
nexus drive cat work:/notes/hi.txt
nexus drive log work
```

```sh
nexus drive import ~/src/project work:/          # 导入本地目录（保留可执行位和符号链接）
nexus drive cat work:/big.bin --offset 1048576 --length 4096   # 只读一段
nexus drive cat work:/notes/hi.txt --at 56f005ed # 读旧版本：提交号（nexus drive log 里的）开头几位
nexus drive rm -r work:/tmp
```

### 跨设备

```sh
nexus drive serve on                     # 在存储设备（例如 vps）上：保存工作区的完整副本
nexus drive create work                  # 之后创建的工作区自动以开了 serve 的设备为副本（或 --replica <设备>）
nexus drive commit work -m "..."         # 提交后自动推到存储设备
nexus drive sync work                    # 别的设备：取回历史和目录结构（内容用到时再取），或推送本机的提交
nexus drive where work:/data             # 副本在哪、是否已同步、本机有多少内容；只在本机时明确写出来
```

没有存储设备时提交只在本机，`commit` 和 `where` 会写明「只在本机」。

两台设备各自提交了对方没有的修改时，`sync` 报告**分叉**，不覆盖任何一边。合并：

```sh home
echo "home 改的" | nexus drive put notes:/todo.txt
nexus drive commit notes -m "home 的修改"
```

```sh laptop
echo "laptop 改的" | nexus drive put notes:/todo.txt
nexus drive commit notes -m "laptop 的修改"
nexus drive sync notes || echo "分叉，退出码 $?"
nexus drive merge notes
nexus drive ls notes:/
```

按文件三方合并：只有一边改的文件取改过的；两边都改了的文件保留本机版本，对方的另存为 `<文件>.conflict-<设备>`。
合并后提交并推送，别的设备 `sync` 就拿到合并结果。

### 挂载（Linux）

```sh
nexus drive mount work:/ ~/work          # 前台运行；fusermount -u ~/work 或 Ctrl-C 卸载
```

编辑器、编译器直接用这个目录。挂载里的新建、修改、删除、改名、`chmod +x` 都成为本机的未提交改动，
`nexus drive commit` 之后才复制到别的设备。`drive sync` 进来的修改约 1.5 秒内在挂载里可见。
修改时间不保存在工作区里：挂载开始时是挂载的时间，内容变了才变（`make`、`cargo` 能看出改动）。

### 预读

新设备上第一次用一个大工作区时，边用边取要一次次去存储设备取。可以先记下一次任务读了哪些文件，以后提前取好：

```sh laptop
nexus drive record work --name build     # 开始记录本机读了哪些文件
nexus drive cat work:/notes/hi.txt       # 做一次任务（通常是在挂载里构建）
nexus drive record work --stop           # 保存成清单 build，并交给存储设备
nexus drive prefetch work --profile build   # 别的设备上：按清单提前取好内容
nexus drive prefetch work --all             # 取整个工作区（之后可以离线用）
```

也可以挂载期间记录，卸载时保存：`nexus drive mount work:/ ~/work --record build`。
预读适合提前做（例如同步之后在后台）；在慢网络上，临用前才预读并不比边用边取快。

agent 读写工作区要有 `drive.read` / `drive.write`，`drive ls` 只列出它能读的工作区。

## 13. 临时分享

把一个文件或目录、一个用 `--proto http` 发布的网页服务、或一个正在运行的终端会话，用一条链接临时给不在网络里的人看。
访客不需要通行密钥，在浏览器里打开链接就能用；一切都是**只读**的。分享只能分享**本机**上的东西。

```sh laptop
nexus share create --drive work:/notes --ttl 30m --max-uses 3     # 工作区里的文件或目录
nexus share ls                                                    # 编号、类型、用了几次、是否撤销
```

```sh
nexus share create --service web --ttl 2h                         # 本机发布的网页服务
nexus share create --terminal main --ttl 1h                       # 本机的终端会话：访客只能看，按键被忽略
nexus share revoke <编号>                                         # 立即失效，访客已经打开的页面断开
```

`create` 输出编号和链接（`https://<页面>/#share=…`）：页面默认是锚点（`nexus anchor set`），没有锚点时是 `https://bewith.link`，
用 `--page` 改。链接里带着随机令牌，拿到链接的人就能用，按密码一样对待。

| | |
| --- | --- |
| 期限 | `--ttl`，默认 1 小时；过期后打开显示已失效 |
| 次数 | `--max-uses <N>`：每打开一次链接算一次，用完即失效；不设则不限 |
| 文件或目录 | 只能看分享的那个路径之内：分享单个文件时看不到别的文件，分享目录时出不去；文本和图片预览，可以下载 |
| 网页服务 | 页面里直接打开，和网页节点打开服务的限制一样（第 11 节） |
| 终端 | 实时看到屏幕和输出；访客的按键和窗口大小都不送到会话里，会话本身不受影响 |
| 记录 | 每次打开和访问（包括过期、次数用完、已撤销、越界被拒的）都记在本机的 `nexus audit` 里，主体写作 `share:<编号>` |

- 访客的流量和网页节点一样经锚点的中继到这台设备，所以这台设备要开着。
- 分享的是这台设备上工作区的**当前内容**（包括还没提交的改动），不是存储设备上的版本；
  这台设备上还没取到的内容，先 `nexus drive prefetch`（第 12 节）。
- 只有创建分享的设备能列出和撤销它；agent 不能创建分享。

## 14. Vault（秘密）

Vault 保存你自己的秘密（API 令牌、密码），只放在你指定的设备上，网络上和磁盘上都只有密文；agent 要按条目授权才能读，
每次取用都有记录。写入、修改和删除只能在管理设备上做。

```sh laptop
echo -n 'sk-old' | nexus vault put openai-key --device home --device vps   # 管理设备上：写入，给 home 和 vps（管理设备自己总在内）
nexus vault put openai-key --value 'sk-new' --device home                  # 同名再写一次就是轮换：版本 +1，vps 上的副本被擦掉
nexus vault ls                                                             # 本机能看到的条目、版本、给了哪些设备
```

```sh home
nexus vault get openai-key               # 被授权的设备上：解密输出（每次都记进 nexus audit）
nexus vault sync                         # 写入时不在线的设备：从管理设备取回自己被授权的条目
```

```sh
nexus vault rm openai-key                # 管理设备上：删除，并通知各设备擦掉
```

- 条目名只能用字母、数字、`-` 和 `_`。不给 `--value` 时从标准输入读，避免秘密留在 shell 历史里。
- 写入时管理设备把条目分别加密推给每台被授权的设备；没被授权的设备磁盘上不会有它，`vault sync` 也取不到。
- agent 要有 `vault.read@<设备>/vault/<条目>` 才能在那台设备上读（例如 `--grant 'vault.read@home/vault/openai-key'`），
  `vault ls` 只列出它被授权的条目；agent 不能写入或删除。读取被拒绝的尝试也记入审计。

**吊销设备后要轮换哪些秘密**：管理设备记着每个条目给过哪些设备。`nexus device revoke <设备>` 的输出里列出那台设备接触过的条目，
之后随时可以再查：

```sh laptop
nexus vault rotate-needed                # 所有已吊销设备接触过、还没轮换的条目
nexus vault rotate-needed --device old-laptop
```

去对应的服务里换掉那个秘密，再 `nexus vault put` 写入新值，它就从清单里消失。

**换了管理设备之后**（用恢复码，第 2 节）：条目原来只在原来的管理设备和被授权的设备上。`nexus recover` 时新的管理设备
从在线的设备收回它们持有的条目（它有根密钥，解得开）；当时不在线的设备，等它上线后在新管理设备上 `nexus vault sync` 再收。
在这之前，其他设备上的 `nexus vault sync` 只更新、不删除本机的条目，不会因为新管理设备的清单短而丢掉秘密。

## 15. 多条网络

设备有多个网络接口（例如有线和 5G）时，到另一台设备的连接同时经过每个接口：

- **断一条不断会话**：一条路径不通（接口还在但没信号）时，数据在几十毫秒内换到别的路径上，会话不断；
  恢复后重新用上。接口出现或消失后 5 秒内跟上。
- **交互冗余**：终端会话、按键、查询这类交互流量在每条可用路径上都发，取先到的。
- **批量分摊**：大传输按各路径的容量分摊。
- **计费网络**：蜂窝网络和手机热点（`wwan`、`rmnet`、`ccmni`、`usb`、`rndis` 等接口，或系统标明的蜂窝设备）
  自动识别为计费，只承担交互流量和故障接管，不承担批量传输。

```sh laptop
nexus path                               # 到每台设备的每条路径：本机地址、对方地址、直连或中继、往返时间、字节数
nexus path home --json
nexus iface                              # 本机的网络接口，以及是否计费（自动识别还是手动设置）
```

```sh
nexus iface wlan0 --metered on           # 手动设置：on、off，或 auto 回到自动识别
```

什么都不用配。目前在 Linux 上可用；两边都要是这个版本。

## 16. Android

Android 上是一个应用（`link.bewith.nexus`）：系统的 VPN 是入口，手机上的任何应用都能按名字访问服务
（`http://web.home.internal/`），规则里送去出口的域名经出口设备出去，其余流量照常走手机自己的网络。

构建和安装见 `docs/android.md`。目前加入网络要用调试版系统上的命令行完成（应用界面上的加入流程还没做），
多路径和计费网络的识别在 Android 上还没做。

## 17. 排查问题

| 现象 | 看什么、怎么办 |
| --- | --- |
| 命令报退出码 101 | 本机的常驻程序没在运行，或 `--data-dir` / `NEXUS_DATA_DIR` 和常驻程序不一致 |
| 批准时连不上、超时 | 两边连不上同一个中继：设锚点或用 `--relay <自建中继>`，必要时加 `--no-discovery` |
| 短加入码报「找不到」（109） | 输错了、过了 10 分钟、已经用过，或新设备又执行了一次 `join`（只有最新的有效）；也可能两边用的不是同一个碰头服务 |
| `join` 只给出长码 | 新设备没有碰头服务（`--no-discovery` 又没有 `--rendezvous`），或连不上它：原因在输出里；用长码 |
| 管理设备报「连不上碰头服务」（108） | 用长码，或者让两边都用锚点的碰头服务 |
| 一直走中继、延迟高 | `nexus status <设备>` 看路径；两边都是对称 NAT 时打洞会失败，把中继（锚点）放在离两边近的地方 |
| 想知道连接走了哪条路 | `nexus path <设备>` |
| agent 报「不认识节点」（103） | 凭据里没有这台设备的任何授权 |
| 访问被拒绝（105） | 看目标设备上的 `nexus audit`：记着主体、能力和资源；给 agent 或通行密钥补授权 |
| 连服务报 108「本机程序没有在运行」 | 服务所在设备上，发布的端口没有程序在监听 |
| 某个域名走错了路 | `nexus route explain <主机:端口>` |
| 数据在哪、是否只有一份 | `nexus drive where <路径>` |
| 访客说分享链接打不开、已失效 | 在创建它的设备上 `nexus share ls`：撤销了、次数用完或过了 `--ttl`；那台设备要开着 |
| `vault get` 报不存在（109） | 本机没被授权这个条目，或写入时本机不在线：`nexus vault sync` |
| 管理设备丢了 | 在任何一台还在的设备上 `nexus recover <恢复码>`（第 2 节） |
| 退出码 110「版本较旧」 | 说明里那台设备不认识这项新功能：升级它（`nexus ls` 能看到每台设备的版本） |
| 连不上、报协议版本不兼容 | 两台设备的协议主版本不同：一起升级到同一个发布 |
| 需要更多日志 | 启动常驻程序时设 `NEXUS_LOG`，例如 `NEXUS_LOG=info` 或 `NEXUS_LOG=iroh=debug`，日志写到标准错误 |

跨网络的实际表现（直连还是中继、上手要多久）可以按 `docs/real-network-check.md` 的清单测一遍。

## 18. 退出码

| 码 | 含义 |
| --- | --- |
| 0 | 成功 |
| 101 | 连不上本机的常驻程序 |
| 102 | 用法错误 |
| 103 | 不认识这台设备 |
| 104 | 内部错误 |
| 105 | 没有授权 |
| 106 | 当前状态不允许（例如没有分叉时 merge、同步时分叉） |
| 107 | 输入不合法 |
| 108 | 连不上目标设备或服务 |
| 109 | 不存在（工作区、文件、服务、预读清单、接口、短加入码、分享、Vault 条目） |
| 110 | 版本不够新：对方（或本机）不认识这个请求，说明里写着哪台设备要升级 |
| 124 / 125 / 127 | `exec`：超时 / 输出超限 / 无法启动 |

出错时，标准错误上是一行 JSON：`{"error": <类别>, "message": <说明>}`。

## 19. 已知限制

- **还没有在真实的跨网络设备上验证过**：自动化测试都在同一台机器上（回环地址或网络命名空间）。
- 不能更换网络根密钥：恢复码或管理设备被别人拿到时只能重新建网络。恢复后看不到原来签发的 agent 列表（第 2 节）。
- 终端会话在设备重启后不保留；在 Windows 和 Android 上，常驻程序重启时会话也会结束。
- 完整节点和 Android 的用户态协议栈不支持 TCP 半关闭：发完数据就关闭写方向的程序（例如 `echo ... | nc`）收不到回应。
- 完整节点和 Android 第一版只转发 TCP，不接管全部流量。
- 挂载只支持 Linux；没有硬链接、扩展属性、文件锁。合并结果不保留空目录。
- 多路径和计费网络识别只支持 Linux；两条链路的批量吞吐约为一条的 1.5 倍；经中继的路径没有测过。
- 网页节点打开网页服务时不支持 WebSocket 和流式回应，页面里写死 `localhost:<端口>` 的完整网址到不了服务（第 11 节）。
- 网页节点的硬件能力：开了的那一项名册里的任何设备都能取用，没有再按设备细分；只能拍照、录一小段、取一次位置，没有连续的视频或音频流。
- 临时分享只读，只能分享本机上的东西；`nexus share ls` 不显示是否已过期。
- Vault 只能在管理设备上写入，写入时不在线的设备要等管理设备在线时 `nexus vault sync`。
  用恢复码换了管理设备后，「吊销后要轮换哪些条目」的记录留在原来的管理设备上，新的管理设备从收回的条目重新开始记。
- 预编译版本只有 Linux（x86_64、aarch64）；没有安装包。`nexus upgrade` 不会自己下载新版本（先下载到一台设备上，再用 `--to`/`--all` 送到其他设备），Windows 上还不支持。
- 还没有：Android 应用内加入（第 16 节）；Windows 的虚拟网卡。
- 远程桌面只是让现成的 VNC / RDP 经 Nexus 连通：网页里还不能看；没有在真实的 macOS 屏幕共享、Windows 远程桌面上验证过。
