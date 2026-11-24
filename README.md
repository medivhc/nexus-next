# Nexus

把一个人的所有设备组成一个整体，让这个人和他的 agent 用同一套身份使用其中任何一台设备上的终端、服务、网络和文件。
每台设备运行一个常驻程序；设备之间按名字互相访问，不需要配地址、端口或 SSH 密钥。

**用法见 [用户手册](docs/manual.md)。**

## 现在能做什么

已完成路线的阶段 0–5，`FEATURES.md` 里的 73 条需求都已实现并有测试：

| 阶段 | 内容 |
| --- | --- |
| 1 | 组网（加入码 + 管理设备批准）、常驻终端会话、结构化执行、agent 身份与授权、审计、对本机表现为 SSH、MCP |
| 2 | 网页节点：浏览器用通行密钥登录后进入任一设备的终端；锚点（中继 + 页面 + 自动证书） |
| 3 | 发布和使用服务、本机统一入口（SOCKS5 / HTTP）、路由规则、出口、完整节点（虚拟网卡）、流量调度、Android |
| 4 | 文件：版本化工作区、跨设备同步与分叉合并、Linux 挂载、预读 |
| 5 | 多条网络：断一条不断会话、交互流量冗余、批量分摊、计费网络 |
| 6 | 易用性：短加入码（`7KQ4M-9XZ21`，经碰头服务）、更短的长加入码；用恢复码恢复管理设备 |
| 7（进行中） | 日常可用：新旧版本互通（设备可以一台一台升级）；终端会话在常驻程序重启后保留；`nexus upgrade`（自动换回） |

还没做：凭据同步（Vault）、临时分享、硬件能力发布、远程桌面。已知限制见手册的第 17 节。
**还没有在两台不同网络的真实设备上验证过**，步骤见 `docs/real-network-check.md`。

## 上手

预编译的 Linux 二进制（x86_64、aarch64）在 Releases 页面；或者从源码构建：

```sh
cargo build --release -p nexus-cli       # target/release/nexus

nexus daemon &                           # 每台设备
nexus init --name laptop                 # 第一台设备（管理设备）
nexus join                               # 新设备：输出短加入码，例如 7KQ4M-9XZ21
nexus device approve <加入码> --name home # 管理设备：批准

nexus ls                                 # 所有设备及状态
nexus ssh home                           # 常驻终端，Ctrl-] 断开，会话继续运行
nexus exec home -- uname -a              # 结构化执行，--json 输出
nexus ssh-config >> ~/.ssh/config        # 之后 ssh/scp/git 用 home.nexus
```

## 文档

| 文档 | 内容 |
| --- | --- |
| [docs/manual.md](docs/manual.md) | 用户手册：怎么工作、从零开始的三台设备、所有命令、维护和排查 |
| [FEATURES.md](FEATURES.md) | 需求（R01–R61）和实现状态 |
| [docs/anchor.md](docs/anchor.md) | 在 VPS 上部署锚点 |
| [docs/unified-entry.md](docs/unified-entry.md) | 统一入口、路由规则、出口、`nexus run`、完整节点的细节 |
| [docs/android.md](docs/android.md) | Android 应用的构建、安装和加入 |
| [docs/real-network-check.md](docs/real-network-check.md) | 在两台真实设备上验证的清单 |
| [docs/roadmap.md](docs/roadmap.md) | 之后的开发计划：和产品文档的差距、阶段 7–9 |
| [docs/protocol.md](docs/protocol.md) | 协议与版本兼容：改协议时的规则（开发者） |
| [web/README.md](web/README.md) | 网页节点的页面：构建、本机试用、浏览器端到端测试 |
| [vendor/README.md](vendor/README.md) | iroh 和 noq 的分支：改了什么、为什么（多路径） |
| `docs/feature-points/` | 每个里程碑的功能点（Given / When / Then），开发时的规格 |
| `docs/measurements/` | 测量记录：流量调度（M3.5）、文件（M4.4）、多路径（M5） |

## 升级兼容性

当前协议：主版本 19、次版本 0（连接的协议名 `nexus/rpc/5`、`nexus/join/5`、`nexus/web/4`）。
同一主版本内新旧版本互通，设备可以一台一台升级；改协议的规则见 [docs/protocol.md](docs/protocol.md)。
升级保留已有的设备密钥、名册和工作区，浏览器需要重新登录。

## 开发

```sh
tools/ci.sh                       # 每次提交都要通过的全部检查（L0–L3）
tools/ci-fast.sh [包…]            # 日常迭代的快车道
tools/nightly.sh                  # 大量种子与变异测试
cargo test --test scenarios sim   # 只跑仿真后端的验收场景
NEXUS_SIM_SEED=7 cargo test --test scenarios node_sees_peer_status::sim   # 复现某个种子
```

`tools/manual_check.py` 在本机起几台设备，照着用户手册里带设备名的代码块从上到下执行一遍（CI 的「L3 用户手册」）：
改了命令的行为或输出，要同时改手册。

真实网络的检查要 root（CI 里用 sudo）：`tools/sched_bench.py`（流量调度）、`tools/multipath_check.py`（多路径）、
`tools/drive_bench.py`（文件的衡量）。浏览器端到端测试在 `tools/web-e2e`，Android 的在 `tools/android_e2e.py`。

发布：改根目录 `Cargo.toml` 的 `version`，CI 通过后打标签 `v<版本>` 并推送；`.github/workflows/release.yml` 构建
Linux 的静态二进制（x86_64、aarch64）发布到 Releases。不带标签手动运行这个工作流只构建、不发布。

开发按「功能文档 → 功能点 → 验收场景 → 实现」的顺序：先在 `FEATURES.md` 写需求，在 `docs/feature-points/` 写功能点，
再写失败的验收场景，最后实现。

### 代码结构

| 层 | crate | 规则 |
|---|---|---|
| 纯逻辑 | `nexus-core-proto`、`nexus-wire`、`nexus-term-proto`、`nexus-net-core`、`nexus-drive-core` | 不读时间、不碰网络和文件、不用异步运行时 |
| 接缝 | `nexus-sys`、`nexus-fabric` | 两个 trait，是运行时接触外界的唯一入口 |
| 驱动 | `nexus-sys-real`、`nexus-sys-sim`、`nexus-fabric-iroh`、`nexus-fabric-sim` | 接缝的真实实现和仿真实现 |
| 运行时 | `nexus-runtime` | 只依赖接缝，不依赖任何具体驱动 |
| 外壳 | `nexus-cli`（`nexus` 二进制）、`nexus-android`（JNI 库）、`nexus-web`（WASM 包装） | |
| 网页核心 | `nexus-web-core` | 浏览器里的节点：协议、会话、重连 |
| 测试 | `nexus-testkit` | 同一份验收场景跑在仿真和真实两种后端上 |

这些规则由 `tools/check.py` 强制执行。传输层用 iroh 和 noq 的分支（`vendor/`），通过根目录 `Cargo.toml` 的
`[patch.crates-io]` 替换。

### 写一个验收场景

场景放在 `crates/nexus-cli/tests/`，只通过命令行操作节点：

```rust
scenario! {
    name: node_sees_peer_status,
    covers: "R03, R22",            // FEATURES.md 里的需求编号
    nodes: ["laptop", "home"],
    real: yes,                      // 或 (pending "原因")
    run: |w: World| async move {
        let out = w.cli("laptop", &["status", "home", "--json"]).await;
        assert_eq!(out.code, 0);
    },
}
```

它会生成 `sim` 和 `real` 两个测试。仿真后端里每个种子跑两遍，两遍的事件轨迹必须相同。
要真实内核的场景（例如挂载）写 `sim: (pending "原因")` 只跑真实后端；要 root 的检查写成 `tools/` 下的脚本，
在文件开头标 `# covers: R..`。

## 许可证

Apache-2.0，见 `LICENSE`。`vendor/` 里的 iroh 和 noq 是 MIT OR Apache-2.0。
