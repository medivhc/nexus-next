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

加入网络目前用命令行完成（界面上的加入流程还没有做）。命令行要读应用的数据目录，需要 root（例如调试版系统）：

```sh
adb push target/android/arm64-v8a/nexus /data/local/tmp/nexus
adb shell "NEXUS_DATA_DIR=/data/data/link.bewith.nexus/files /data/local/tmp/nexus join"   # 显示加入码
nexus device approve <加入码> --name phone                                                   # 在管理设备上
```

之后手机上的任何应用都能按名字访问服务（`http://web.home.internal/`），规则里送去出口的域名经出口设备出去
（`nexus route add --domain example.com --via anchor`），其余流量照常走手机自己的网络。

启动时的参数：`--es name <设备名>`、`--es relay default|disabled|<中继地址>`、`--ez discovery true|false`、
`--ez multipath true|false`（默认 true：请求蜂窝网络在有 Wi-Fi 时也保持在线，见下）。
停止：`adb shell am start -n link.bewith.nexus/.MainActivity --es action stop`。
核心的日志在 `/data/data/link.bewith.nexus/files/daemon.log`；数据目录里放一个名为 `trace` 的文件，
下次启动时各个库的调试日志也会写进去。

## 实测

`tools/android_e2e.py` 在 adb 连着的设备（默认 redroid 的 `127.0.0.1:5555`）上装应用，
让本机当 home，检查内网访问、出口和其余流量。需要免密 sudo（临时改 `/etc/hosts`）。

`tools/android_multipath.py`（M11.4）检查多路径和计费网络识别：在 redroid 的网络命名空间里加两条 veth 链路连到
另一个命名空间里的 home，看两条直连路径、断一条时会话不断，以及系统报告的网络和 `nexus iface` 一致。需要 root。

## 多条网络和计费网络（M11.4）

- 应用用 ConnectivityManager 跟踪系统的每个网络（不含 VPN），把接口名和「是否计费」交给核心
  （没有 `NET_CAPABILITY_NOT_METERED`、也不是运营商临时不计费的就算计费）。`nexus iface` 里这些接口的自动识别按系统的；
  系统没报告的接口仍按名字识别。手动设置（`nexus iface <接口> --metered on|off|auto`）照样优先。
- 系统在有 Wi-Fi 时会关掉移动数据。应用请求蜂窝网络（`CHANGE_NETWORK_STATE`），让它一直在线；
  计费的蜂窝网络只承担交互流量和故障接管，平时只有心跳和交互冗余的流量。`--ez multipath false` 时不请求。
- 核心的多路径和 Linux 是同一份代码：每个接口的地址都打洞，包从源地址所在的接口发出（`IP_PKTINFO`）。
  真机上这依赖 Android 按出口接口选路由表的规则，还没有在有 Wi-Fi 和 5G 的真机上验证。
