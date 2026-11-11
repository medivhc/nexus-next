# Nexus

把一个人的所有设备组成一个整体，让这个人和他的 agent 用同一套身份使用其中任何一台设备上的能力。

当前进度：**M0**（仓库骨架、测试工具箱、结构约束）。需求见 `FEATURES.md`。

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
