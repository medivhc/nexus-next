# Nexus

把一个人的所有设备组成一个整体，让这个人和他的 agent 用同一套身份使用其中任何一台设备上的能力。

当前进度：**阶段 1 完成**（组网、免密终端、结构化执行、agent 身份与授权、对本机表现为 SSH）。
需求见 `FEATURES.md`，各里程碑的功能点见 `docs/feature-points/`。尚待在两台不同网络的真实设备上验证，步骤见 `docs/real-network-check.md`。

## 上手

```sh
# 每台设备
nexus daemon &

# 第一台设备
nexus init --name laptop

# 新设备：输出一段加入码
nexus join
# 第一台设备：批准
nexus device approve <加入码> --name home

nexus ls                          # 所有设备及状态
nexus ssh home                    # 常驻终端，Ctrl-] 断开，会话继续运行
nexus exec home -- uname -a       # 结构化执行，--json 输出
nexus ssh-config >> ~/.ssh/config # 之后 ssh/scp/git 用 home.nexus

# agent
nexus agent issue --label builder --grant term.exec@home --ttl 4h
NEXUS_AGENT=<凭据> nexus exec home -- make
nexus mcp                         # 给 agent 框架用的 MCP 服务器
nexus audit                       # 本机的审计记录
```

以独立系统账号运行的 agent：常驻程序加 `--agent-socket /某个那个账号能进入的目录/agent.sock`，
agent 一侧设置同样的 `NEXUS_AGENT_SOCKET` 和 `NEXUS_AGENT`。这样 agent 用不了你本人的控制入口，只能做凭据允许的事。

## 升级兼容性

当前协议为 RPC `nexus/rpc/4`、加入 `nexus/join/4`、网页 `nexus/web/3`（wire 版本 9）。
设备程序和网页 WASM 需要一起升级；旧版本的连接会因协议不匹配而失败。
升级保留已有设备密钥和名册（加入锚点之前保存的名册照样能读出，签名仍然有效），浏览器需要重新登录。

## 运行

```sh
tools/ci.sh                       # 每次提交都要通过的全部检查
cargo test                        # 全部测试
cargo test --test scenarios sim   # 只跑仿真后端的验收场景
NEXUS_SIM_SEED=7 cargo test --test scenarios node_sees_peer_status::sim   # 复现某个种子
NEXUS_SIM_SEEDS=1000 cargo test --test scenarios sim                      # 跑更多种子
```

试用常驻程序：

```sh
cargo run -q -- --data-dir /tmp/nexus-a daemon --name laptop &
cargo run -q -- --data-dir /tmp/nexus-a status --json
```

## 代码结构

| 层 | crate | 规则 |
|---|---|---|
| 纯逻辑 | `nexus-core-proto`、`nexus-wire` | 不读时间、不碰网络和文件、不用异步运行时 |
| 接缝 | `nexus-sys`、`nexus-fabric` | 两个 trait，是运行时接触外界的唯一入口 |
| 驱动 | `nexus-sys-real`、`nexus-sys-sim`、`nexus-fabric-sim` | 接缝的真实实现和仿真实现 |
| 运行时 | `nexus-runtime` | 只依赖接缝，不依赖任何具体驱动 |
| 外壳 | `nexus-cli` | `nexus` 二进制 |
| 测试 | `nexus-testkit` | 同一份验收场景跑在仿真和真实两种后端上 |

这些规则由 `tools/check.py` 强制执行。

## 写一个验收场景

场景放在 `crates/nexus-cli/tests/scenarios.rs`，只通过命令行操作节点：

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

## 许可证

Apache-2.0，见 `LICENSE`。
