# drive-probe

M4.0 的技术验证：FUSE 透传的开销、FastCDC + BLAKE3 的吞吐。结果见 docs/feature-points/M4.0.md。

```sh
cargo build --release
target/release/drive-probe chunk <路径>...
target/release/drive-probe mount <源目录> <挂载点>
```
