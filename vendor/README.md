# 传输层的分支

阶段 5（多路径）要改传输层：iroh 1.4.1、noq 1.4.0、noq-proto 1.4.0、noq-udp 1.4.0 从 crates.io 原样复制到这里，
通过根目录 `Cargo.toml` 的 `[patch.crates-io]` 替换。许可证都是 MIT OR Apache-2.0（见各目录的 LICENSE 文件）。

M11.5 起完整节点和 Android 的用户态协议栈 ipstack 1.0.1 也在这里（许可证 Apache-2.0），同样经 `[patch.crates-io]` 替换。

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
| 收到对方的 FIN 只结束读方向、不替本端发 FIN；本端 shutdown 且在途数据都被确认后才发；FIN 可以带数据；去掉 CLOSE_WAIT 超时强行发 FIN；本端先关时对方的 FIN 立刻结束读方向；没 shutdown 就丢掉的连接发 RST | ipstack `src/stream/tcp.rs`（`HalfClose`） | TCP 半关闭：`echo ... \| nc` 收不到回应（M11.5，R89） |
