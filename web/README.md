# 网页节点的页面

- `index.html`、`app.js`、`app.css`：页面本身。通行密钥用浏览器的 WebAuthn 接口；协议、会话、重连都在
  WebAssembly 里的网页节点核心（`crates/nexus-web`，包装 `crates/nexus-web-core`）。
- 构建：`tools/build-web.sh`，产物在 `web/dist`。
- 本机试用：`nexus web serve --dir web/dist`，然后
  `nexus web enroll --label dev --page http://localhost:8080`，在浏览器里打开输出的链接。
  中继来自登记链接或通行密钥里的入口信息，都没有时用页面所在的站点。
- 页面不使用任何浏览器存储：临时身份和登录凭证只在内存里。

## 浏览器端到端测试（L4）

```sh
tools/build-web.sh
cargo build --release -p nexus-cli
cargo install iroh-relay --version 1.4.1 --features server --locked
cd tools/web-e2e && npm install
node run.mjs --nexus ../../target/release/nexus --relay-bin ~/.cargo/bin/iroh-relay --dist ../../web/dist
```

2026-11-15 在云端机器（Ubuntu 24.04，Chrome 无头模式）上连续 3 次通过：登录 126–189 毫秒，
从点登录到出现提示符约 3.3 秒（其中约 3 秒是浏览器第一次经中继连上第二台设备，M2.5 处理）；
WebAssembly 3.7 MB，gzip 1.45 MB。
