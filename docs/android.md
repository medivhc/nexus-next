# Android

Android 上的 Nexus 是一个应用（`link.bewith.nexus`）：系统的 VPN 是入口，核心和命令行的常驻程序是同一份代码，
在应用进程里运行。功能点和已知限制见 `docs/feature-points/M3.6.md`。

## 构建

需要 JDK 17、Android SDK（`platforms;android-34`、`build-tools;34.0.0`、NDK）和 rustup 的 Android 目标：

```sh
rustup target add aarch64-linux-android x86_64-linux-android
ANDROID_HOME=~/android-sdk tools/build-android.sh arm64-v8a          # 真机；redroid / 模拟器用 x86_64
```

产物：`target/android/nexus.apk`（调试密钥签名），以及 `target/android/<abi>/nexus`（Android 上的命令行，调试用）。

## 装到手机上并加入网络

```sh
adb install -r target/android/nexus.apk
adb shell am start -n link.bewith.nexus/.MainActivity --es name phone   # 第一次会弹出 VPN 授权
```

打开应用、同意 VPN 授权后，还没加入网络的手机直接在屏幕上显示加入码（M11.2，`docs/feature-points/M11.2.md`）：

- 短加入码（大字）和要在管理设备上运行的命令：`nexus device approve XXXXX-XXXXX`；
- 加入码的二维码：里面是大写的长加入码，用任何扫码工具读出文字交给 `nexus device approve`，不依赖碰头服务；
- 长加入码（点一下复制）和「换一个加入码」按钮。加入码快过期时自动换新。

管理设备批准后，几秒内屏幕变成「已加入网络」，列出网络里的设备。不需要 root，也不需要在手机上运行命令行。

调试时仍然可以用命令行（要 root 读应用的数据目录）：

```sh
adb push target/android/arm64-v8a/nexus /data/local/tmp/nexus
adb shell "NEXUS_DATA_DIR=/data/data/link.bewith.nexus/files /data/local/tmp/nexus status"
```

之后手机上的任何应用都能按名字访问服务（`http://web.home.internal/`），规则里送去出口的域名经出口设备出去
（`nexus route add --domain example.com --via anchor`），其余流量照常走手机自己的网络。

DNS 引流的公网域名同时支持 TCP 和 UDP。设备出口的 UDP 要求协议 19.6，并沿用 `nexus exit on` 的开放设置。
使用 `--socks5` 时，UDP 要求上游接受无认证的 `UDP ASSOCIATE`。协商失败、出口拒绝和中继断开会在
`daemon.log` 中显示原因，数据报只会发往规则指定的出口。已发布的私有 UDP 服务仍可按 `.internal` 名称访问。

启动时的参数：`--es name <设备名>`、`--es relay default|disabled|<中继地址>`、`--ez discovery true|false`、
`--ez multipath true|false`（默认 true：请求蜂窝网络在有 Wi-Fi 时也保持在线，见下）。
`--es rendezvous default|disabled|<碰头服务地址>`（短加入码用；自建网络里是 `https://<锚点>/pkarr`）。
停止：`adb shell am start -n link.bewith.nexus/.MainActivity --es action stop`。
核心的日志在 `/data/data/link.bewith.nexus/files/daemon.log`；数据目录里放一个名为 `trace` 的文件，
下次启动时各个库的调试日志也会写进去。

手机上可用「停用 VPN」恢复系统原来的网络，再用「启用 VPN」连接。停用状态和「绕过 VPN 的应用」列表会保存。绕过设置在下次启用时生效。Android 用系统网络处理被排除应用的连接和 DNS，这些应用无法通过 Nexus VPN 访问 `.internal` 服务。
「代理失败记录」显示核心最近记录的 TCP 和 UDP 转发失败。默认仅接管 `198.18.0.0/16` 临时地址。

Android 10 及以后可通过「按应用选择出口」为指定应用设置设备出口或 SOCKS5。重启 VPN 后，未绕过的应用纳入 IPv4 和 IPv6 接管。已设置出口的应用经指定出口连接公网域名和字面 IP，私有服务及拒绝规则优先。可确认归属、没有指定出口的连接遵循现有规则。Android 无法确认应用归属或共享 UID 的策略冲突时关闭连接。

按应用接管开启时，VPN 内的普通 TCP/UDP DNS 查询使用本地解析。`.internal` 由 Nexus 处理，公网 A 查询返回临时地址，实际解析由选定出口负责。AAAA 和其他查询类型返回空应答，端口 853 关闭。应用自行通过 HTTPS 发送的 DNS 内容仍是应用流量的一部分。绕过应用使用系统网络及其 DNS。

「代理规则」可查看、添加和删除域名规则，并选择 Nexus 设备出口或 SOCKS5 上游。也可以粘贴 Clash/Mihomo 的 `rules` 或 `payload` 中的 `DOMAIN-SUFFIX` 条目。`DIRECT` 和 `REJECT` 保持原义，一个命名策略及未写策略的条目使用所填出口。导入按原顺序追加，已有规则先匹配。遇到其他规则类型、多个命名策略或无等价行为的内置策略，界面标出原因，整批保留原来的配置。

## 实测

`tools/android_join_e2e.py` 只通过界面加入网络：从屏幕（uiautomator）读出短码、本机当管理设备批准，
再从屏幕确认已加入；`--no-rendezvous` 改用二维码（有 `zbarimg` 时从截图里真的扫出来）。

`tools/android_e2e.py` 在 adb 连着的设备（默认 redroid 的 `127.0.0.1:5555`）上装应用，
让本机当 home，检查内网访问、出口和其余流量。需要免密 sudo（临时改 `/etc/hosts`）。

### 模拟器代理和切网回归

先构建 x86_64 APK，再启动专用的 Android Emulator AVD。回归使用 Android 14（API 34）的 AOSP x86_64
`userdebug` 镜像、默认开放的 `AndroidWifi` 和模拟蜂窝，要求 `adb root`。优先使用有 KVM 的宿主机。

```sh
tools/build-android.sh x86_64
python3 tools/android_proxy_e2e.py --serial emulator-5554
```

脚本启动宿主机本地回声端点和两个 SOCKS5 上游，生成并安装两个独立 UID 的探针应用。它先在 Wi-Fi 下检查
启动 VPN 前的 TCP/UDP 连通性，再验证按应用选路、绕过、域名和 IPv6。切换 Wi-Fi → 蜂窝 → Wi-Fi 时，脚本核对
系统默认网络和 VPN 会话 ID，并用新建的 TCP/UDP 连接验证恢复。拒绝 `UDP ASSOCIATE` 的上游用于检查核心失败
记录和目标端是否收到回退流量。脚本最后停用 VPN 并确认两个协议恢复直连，随后恢复原来的 VPN 偏好和无线开关，
卸载探针应用。请使用可供覆盖安装 Nexus APK 的测试 AVD。

若宿主机的 HTTP 代理拒绝虚拟 Wi-Fi 到 `10.0.2.2` 的 TCP 请求，启动 AVD 时给 `netsimd` 指定本地转发器，
运行测试时带上对应端口。转发器只把本次的测试端口（默认 `19180`–`19182`）接到本机，其余请求沿用
`http_proxy` 或 `HTTP_PROXY`（无认证的 HTTP 代理地址）。

```sh
# 在另一个终端启动 AVD，等待系统就绪
"$ANDROID_HOME/emulator/emulator" -avd Nexus_API34 -netsim-args '--http-proxy=http://127.0.0.1:19185'
python3 tools/android_proxy_e2e.py --serial emulator-5554 --local-http-port 19185
```

独立 Nexus 节点的私有服务和设备出口可继续用 `tools/android_e2e.py` 验证。真机切网、后台恢复和功耗见
[真实异网验收清单](real-network-check.md)。

`tools/android_multipath.py`（M11.4）检查多路径和计费网络识别：在 redroid 的网络命名空间里加两条 veth 链路连到
另一个命名空间里的 home，看两条直连路径、断一条时会话不断，以及系统报告的网络和 `nexus iface` 一致。需要 root。

## 多条网络和计费网络（M11.4）

- 应用用 ConnectivityManager 跟踪系统的每个网络（不含 VPN），把接口名和「是否计费」交给核心
  （没有 `NET_CAPABILITY_NOT_METERED`、也不是运营商临时不计费的就算计费）。`nexus iface` 里这些接口的自动识别按系统的；
  系统没报告的接口仍按名字识别。手动设置（`nexus iface <接口> --metered on|off|auto`）照样优先。
- 系统在有 Wi-Fi 时会关掉移动数据。应用请求蜂窝网络（`CHANGE_NETWORK_STATE`），让它一直在线；
  计费的蜂窝网络只承担交互流量和故障接管，平时只有心跳和交互冗余的流量。`--ez multipath false` 时不请求。
- 可在手机上打开「按需保持蜂窝备用链路」，下次启用 VPN 时生效。此模式在 Wi-Fi 可用且虚拟网卡有应用出站流量时请求蜂窝网络，连续 90 秒未检测到流量便释放请求。核心自身的组网包不经过虚拟网卡，因而不延长这段时间。
- 核心的多路径和 Linux 是同一份代码：每个接口的地址都打洞，包从源地址所在的接口发出（`IP_PKTINFO`）。
  真机上这依赖 Android 按出口接口选路由表的规则，还没有在有 Wi-Fi 和 5G 的真机上验证。
