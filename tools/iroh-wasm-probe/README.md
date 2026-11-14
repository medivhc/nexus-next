# iroh 浏览器版技术验证（M2.0）

浏览器里的 iroh（WebAssembly）经本机中继连上一个原生节点，发一句话、收回声。

```sh
rustup target add wasm32-unknown-unknown            # 需要能访问 static.rust-lang.org；ring 需要 clang
cargo build --release --bin serve
RUSTFLAGS='--cfg getrandom_backend="wasm_js"' cargo build --release --lib --target wasm32-unknown-unknown
wasm-bindgen --target web --out-dir pkg target/wasm32-unknown-unknown/release/probe.wasm   # 版本与 Cargo.lock 一致
wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
  --enable-mutable-globals --enable-reference-types --enable-multivalue pkg/probe_bg.wasm -o pkg/probe_bg.wasm  # binaryen 123；旧版会生成坏文件
./target/release/serve > serve.log &                 # 输出：中继地址 节点标识
python3 -m http.server 8088 &
npm install playwright-core && read URL ID < serve.log && node run.mjs "$URL" "$ID" 8088
```

## 结果（2026-11-14，Ubuntu 24.04，Chrome 无头模式，iroh 1.4.1，wasm-bindgen 0.2.130）

| 项 | 结果 |
|---|---|
| 编译 | 通过：`default-features = false` 加 `tls-ring`（没有加密实现时 presets 不可用） |
| 互通 | 通过：浏览器经本机中继连上原生节点，回声正确；含建立端点、连中继、握手的整次调用首次 60–80 毫秒，之后约 20 毫秒 |
| 体积 | wasm-bindgen 输出 5.2 MB；wasm-opt -Oz 后 2.3 MB，gzip 986 KB，brotli 776 KB |
| 注意 | binaryen 108（Ubuntu 软件源）优化后的文件浏览器无法加载，需要新版 |
