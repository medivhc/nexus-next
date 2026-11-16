# 网页节点的页面

- `index.html`、`app.js`、`app.css`：页面本身。通行密钥用浏览器的 WebAuthn 接口；协议、会话、重连都在
  WebAssembly 里的网页节点核心（`crates/nexus-web`，包装 `crates/nexus-web-core`）。
- 构建：`tools/build-web.sh`，产物在 `web/dist`。
- 本机试用：管理设备以开发用的锚点运行
  `nexus daemon --anchor-dev 127.0.0.1:8080 --page-dir web/dist`（同一个端口上是页面和中继），
  `nexus anchor set http://localhost:8080`，然后 `nexus web enroll --label dev`，在浏览器里打开输出的链接。
  只要页面不要中继时也可以 `nexus web serve --dir web/dist`。
  中继来自登记链接或通行密钥里的入口信息，都没有时用页面所在的站点。
- 正式部署：见 docs/anchor.md。
- 页面不使用任何浏览器存储：临时身份和登录凭证只在内存里。

## 浏览器端到端测试（L4）

```sh
tools/build-web.sh
cargo build --release -p nexus-cli
cd tools/web-e2e && npm install
node run.mjs --nexus ../../target/release/nexus --dist ../../web/dist
```

laptop 以开发用的锚点运行，浏览器经它取页面、经它的中继连 laptop 和 home。
2026-11-16（M2.4，经锚点）连续 3 次通过：登录 120–192 毫秒，从点登录到出现提示符约 3.3 秒。

2026-11-15 在云端机器（Ubuntu 24.04，Chrome 无头模式）上连续 3 次通过：登录 126–189 毫秒，
从点登录到出现提示符约 3.3 秒（其中约 3 秒是浏览器第一次经中继连上第二台设备，M2.5 处理）；
WebAssembly 3.7 MB，gzip 1.45 MB。
