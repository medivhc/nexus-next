# Nexus Next 网络运行时测试用例（N0 / N1 / N2）

本测试用例文档逐条映射 `docs/feature-points/N1.md` 与 `docs/feature-points/N2.md` 的验收标准，按项目测试分层（L0 静态门禁 → L1 纯逻辑单测 → L2 运行时仿真 → L3 redroid Android E2E → 真机手工验收）组织。

---

## 1. 验收标准与测试用例映射总览

| 用例编号 | 对应验收标准 | 测试层级 | 实现位置 / 执行命令 |
|---|---|---|---|
| TC-L0-01 | N1 门禁 1 | L0 静态检查 | `python3 tools/check.py && python3 tools/check.py --self-test` |
| TC-N1-01 | FP-N1-01 | L1 纯逻辑单测 | `crates/nexus-net-core/src/lib.rs` (`domain_exact_does_not_match_subdomains`) |
| TC-N1-02 | FP-N1-02 | L1 纯逻辑单测 | `crates/nexus-net-core/src/lib.rs` (`audit_detects_shadowed_duplicate_and_invalid_rules`) |
| TC-N1-03 | FP-N1-03 | L1 纯逻辑单测 | `crates/nexus-net-core/src/lib.rs` (`clash_import_parses_supported_subset_and_reports_skipped`) |
| TC-N1-04 | FP-N1-03 | L1 纯逻辑单测 | `crates/nexus-net-core/src/lib.rs` (`clash_import_never_silently_downgrades_unmapped_proxy_to_direct`) |
| TC-N1-05 | FP-N1-04 | L2 运行时仿真 | `crates/nexus-runtime/tests/network_runtime.rs` (`route_import_saves_lkg_and_rollback_restores`) |
| TC-N1-06 | FP-N1-02 | L2 运行时仿真 | `crates/nexus-runtime/tests/network_runtime.rs` (`route_audit_reports_shadowed_rules_via_ctl`) |
| TC-N1-07 | FP-N1-05 | L2 运行时仿真 | `crates/nexus-runtime/tests/network_runtime.rs` (`exit_datagram_forwards_udp_via_authorized_exit`) |
| TC-N1-08 | FP-N1-06 | L2 运行时仿真 | `crates/nexus-runtime/tests/network_runtime.rs` (`exit_datagram_rejects_loopback_and_own_addresses`) |
| TC-N1-09 | FP-N1-06 | L2 运行时仿真 | `crates/nexus-runtime/tests/network_runtime.rs` (`tun_udp_never_falls_back_to_direct_on_reject_or_socks5`) |
| TC-N1-10 | FP-N1-07 | L1 纯逻辑单测 | `crates/nexus-net-core/src/lib.rs` (`app_policy_always_includes_self_and_sanitizes`) |
| TC-N1-11 | FP-N1-08 | L3 redroid E2E | InstaBox `redroid` (`tools/build-android.sh x86_64` + `adb install` + 启动/状态/停止验证) |
| TC-N2-01 | FP-N2-01 | L1 纯逻辑单测 | `crates/nexus-net-core/src/lib.rs` (`power_policy_defaults_to_balanced_without_cellular_keepalive`) |
| TC-N2-02 | FP-N2-02 | L3 redroid E2E | InstaBox `redroid` 启动默认模式，`logcat` 确认 `balanced` 且未请求蜂窝 |
| TC-N2-03 | FP-N2-03 | L3 redroid E2E | InstaBox `redroid` 切换 `realtime` 与切回 `balanced`，确认无崩溃且日志状态正确 |
| TC-N2-04 | FP-N2-04 | L3 redroid E2E | InstaBox `redroid` 触发 `action=stop`，确认 5 秒内释放 VPN 与核心进程正常退出 |
| TC-REAL-01 | N1/N2 真机门禁 | 真机手工（BLOCKED） | 真实 Android 手机（arm64-v8a）+ 异网 VPS/家庭节点 + 8 小时息屏功耗对照 |

---

## 2. 详细用例设计

### 2.1 L1 纯逻辑层测试用例（`nexus-net-core`）

- **TC-N1-01 精确域名匹配与子域名隔离**
  - 输入：规则 `[DomainExact("api.example.com") -> Exit("jp"), Domain("example.com") -> Direct]`
  - 断言：
    - `decide("api.example.com:443")` == `Rule { index: 0, outbound: Exit("jp") }`
    - `decide("sub.api.example.com:443")` == `Rule { index: 1, outbound: Direct }`
    - `decide("API.EXAMPLE.COM.:443")` == `Rule { index: 0, outbound: Exit("jp") }`（大小写与尾点归一化）
- **TC-N1-02 规则静态审计**
  - 输入：包含宽规则遮蔽窄规则、重复规则、CIDR 子网被父网遮蔽、非法 SOCKS5 地址的规则列表
  - 断言：`audit_rules` 返回精确的 `IssueKind::Shadowed`、`IssueKind::Duplicate`、`IssueKind::InvalidUpstream` 及 1-based 规则索引
- **TC-N1-03 Clash YAML 子集解析与审计报告**
  - 输入：包含 `DOMAIN`、`DOMAIN-SUFFIX`、`IP-CIDR`（带 `no-resolve`）、`IP-CIDR6`、`MATCH`、`GEOIP,CN,DIRECT`、`SCRIPT,foo,PROXY`、`PROCESS-NAME,curl,DIRECT`、`MITM` 段的 YAML 文本
  - 断言：`imported` 仅含合法支持规则；`skipped` 包含 `GEOIP` / `SCRIPT` / `PROCESS-NAME` 及明确原因；`notes` 记录 `MATCH` 语义提示且无脚本/MITM 被启用
- **TC-N1-04 未映射代理组绝不静默降级为直连**
  - 输入：`DOMAIN,secret.example.com,JP-Group`，且 `ImportOptions { default_exit: None, default_socks5: None }`
  - 断言：`imported` 为空，`skipped` 记录该条规则因缺少可用出口映射而被跳过（不会变成 `Outbound::Direct`）
- **TC-N1-10 App 绕过策略防环路归一化**
  - 输入：`bypass_packages = ["com.tencent.mm", "  ", "com.tencent.mm", "bad pkg!", "com.eg.android.AlipayGphone"]`，`self_package = "link.bewith.nexus"`
  - 断言：输出去重后的 `["com.eg.android.AlipayGphone", "com.tencent.mm", "link.bewith.nexus"]`，强制包含自身，剔除非法包名
- **TC-N2-01 电源策略默认值与三模式映射**
  - 输入：`PowerMode::default()`、`PowerMode::parse("balanced" | "powersave" | "realtime" | "invalid")`
  - 断言：`Balanced` 与 `PowerSave` 下 `keep_cellular_with_wifi == false`；仅 `Realtime` 下为 `true`；非法字符串安全回退 `Balanced`

### 2.2 L2 运行时仿真测试用例（`nexus-runtime`）

- **TC-N1-05 规则原子导入保存 LKG 并支持一键回滚**
  - 步骤：初始添加 1 条规则 `a.test -> Reject` → 调用 `CtlRequest::RouteImport { replace: true }` 导入 `[b.test -> Direct]` → 校验当前规则仅为 `b.test` → 调用 `CtlRequest::RouteRollback` → 校验规则恢复为 `a.test -> Reject`
- **TC-N1-06 控制通道执行规则审计**
  - 步骤：添加 `example.com -> Direct` 与 `sub.example.com -> Reject` → 调用 `CtlRequest::RouteAudit` → 校验返回 `Shadowed` 诊断
- **TC-N1-07 通用公网 UDP 经已授权出口转发**
  - 步骤：仿真双节点 `phone` 与 `home`；`home` 开启 `exit_enabled` 并在非回环 IP 启动模拟 UDP 回声服务；`phone` 配置 `Domain("udp.far.test") -> Exit("home")` 并开启 TUN；`phone` 向 `udp.far.test` 的 FakeIP 发 UDP 载荷 `"ping-udp"`
  - 断言：收到 `"echo:ping-udp"`，且 `home` 审计记录包含 `net.exit`
- **TC-N1-08 出口 UDP 严格拒绝本机回环地址**
  - 步骤：`home` 将 `local.test` 解析为 `127.0.0.1`；`phone` 经 `Exit("home")` 发送 UDP 到 `local.test:9000`
  - 断言：出口拒绝转发（`Denied`），不触达 `home` 本机 `127.0.0.1:9000`
- **TC-N1-09 Reject / Socks5 规则下的 UDP 绝不偷偷直连**
  - 步骤：`phone` 配置 `Domain("blocked.test") -> Reject` 与 `Domain("socks.test") -> Socks5("127.0.0.1:7890")`，向两者 FakeIP 发送 UDP
  - 断言：两端均不产生任何直连 `udp_connect` 调用，流被干净关闭

### 2.3 L3 Android redroid E2E 测试用例

- **TC-N1-11 & TC-N2-02/03/04**：
  1. 执行 `tools/build-android.sh x86_64` 编译 `target/android/nexus.apk`（验证 Rust JNI + Java `javac/d8/aapt2/apksigner` 无报错）。
  2. `adb install -r target/android/nexus.apk` 安装到 InstaBox `redroid` (`Android 14`)。
  3. 默认启动 `am start -n link.bewith.nexus/.MainActivity --es name phone-n1 --es relay disabled --ez discovery false`，验证 `logcat` 输出 `电源模式：balanced（不主动保持蜂窝）`，且核心套接字 `ctl.sock` 正常就绪。
  4. 发送 `--es power_mode realtime` 切换电源模式，验证日志切换至 `realtime` 且应用无崩溃；再切回 `balanced` 验证回调释放。
  5. 发送 `--es action stop`，验证 5 秒内 VPN 停止、`daemon.log` 记录 `nexus: 已停止`。

### 2.4 真机手工验收清单（BLOCKED — 需真实 Android 手机）

> 以下项目受限于云端沙箱无物理手机、真实蜂窝基带与硬件电量计，状态标为 **BLOCKED（待真机执行）**，供用户在真实手机上按步骤执行：

1. **构建 arm64-v8a APK**：`rustup target add aarch64-linux-android && ANDROID_HOME=/opt/android-sdk tools/build-android.sh arm64-v8a`
2. **安装与加入（无需 root）**：`adb install -r target/android/nexus.apk`，打开应用授权 VPN，在管理设备执行 `nexus device approve <短加入码>`。
3. **N1 日常替代验证**：导入脱敏 Clash 规则，验证国内 App 绕过 VPN 直连、目标服务经出口访问、`.internal` 私有服务访问及 UDP 公网出口。
4. **N2 功耗对照（8 小时息屏 × 3 轮）**：固定亮度/后台/Wi-Fi，对比「无 VPN」「原有 Clash/EasyTier」「Nexus `balanced`」「Nexus `realtime`」，通过 `adb shell dumpsys batterystats` 记录电量与唤醒次数。
5. **60 秒回退验证**：应用内点击「停止 VPN」，确认 60 秒内恢复既有网络或原 VPN 应用。
