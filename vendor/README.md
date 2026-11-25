# 传输层的分支

阶段 5（多路径）要改传输层：iroh 1.4.1、noq 1.4.0、noq-proto 1.4.0、noq-udp 1.4.0 从 crates.io 原样复制到这里，
通过根目录 `Cargo.toml` 的 `[patch.crates-io]` 替换。许可证都是 MIT OR Apache-2.0（见各目录的 LICENSE 文件）。

每一处改动都标注 `NEXUS:`，可以用 `grep -rn "NEXUS:" vendor/` 找到。升级上游时，先把上游新版本复制进来，
再把这些改动重新加上。

| 改动 | 位置 | 原因 |
| --- | --- | --- |
| 发出的接口跟源地址走（`IP_PKTINFO` 的接口编号） | noq-udp `src/unix.rs`、`src/nexus_ifindex.rs` | 一个套接字上的多条路径分别从各自的网络接口出去（有线、5G） |
| 直连路径都保留、都是 Available；任一连接上的直连路径在到同一设备的所有连接上打开 | iroh `src/socket/remote_map/remote_state.rs`（`NEXUS_SINGLE_IP_PATH`） | 原来只保留一条直连路径，其余关掉 |
| 多路径时每 5 秒检查，本机地址没有路径就重新打洞 | 同上（`check_connections`） | 断过又恢复的接口重新用上 |
| 第一次超时从最早一个未确认的包算起 | noq-proto `src/connection/mod.rs`（`pto_time_and_space`） | 一直在发数据的路径断了，原来要等拥塞窗口塞满才超时（秒级） |
| 超时的路径在有别的健康路径时：在途的包立刻算丢、不再发新数据，只发探测 | 同上（`nexus_fail_over`、`scheduling_info`） | 数据马上在别的路径上重发 |
| 多路径时确认走它所确认的路径；可疑路径照样发自己的确认 | noq-proto `src/connection/mod.rs`、`src/connection/spaces.rs` | 确认都走一条路径时，发送方会把另一条好路径误判为断了 |
| 交互冗余：优先级 ≥ 8 的流的数据在另一条健康路径上再发一次 | noq-proto `src/connection/streams/state.rs`（`nexus_dups`）、`src/connection/mod.rs`、`src/connection/spaces.rs` | 交互流量两条路同时发，取先到的 |
| 只减新确认的字节 | noq-proto `src/connection/send_buffer.rs`、`streams/send.rs`、`streams/state.rs` | 同一段数据被确认两次时原来会下溢 |
| 任何一端标为 Backup 就按 Backup 调度；Available 路径可疑时 Backup 立刻接管 | noq-proto `src/connection/paths.rs`（`nexus_status`）、`mod.rs` | 计费网络（对端的接口计费时本端也不发批量） |
| 计费的本机地址：从它们出去的直连路径是 Backup | iroh `src/lib.rs`（`nexus_set_metered_ips`）、`remote_state.rs` | 计费网络只承担交互和故障接管 |
| 直连都可疑时 Backup 路径（中继、计费的接口）也算健康路径：在途的数据马上转过去 | noq-proto `src/connection/mod.rs`（`nexus_has_healthy_path_besides`） | 直连都断时原来要等直连被放弃（5 秒）才在中继上重发（M11.4） |
| BBR 的应用受限标记：上一段受限的数据送达时，现在仍受限就重新标记（原来是 `else if`） | noq-proto `src/congestion/bbr3/mod.rs`（`on_end_acks`） | 零星的交互流量被当成带宽到顶，起步阶段过早结束，批量传输开头几秒只有一半速度（M11.4） |
