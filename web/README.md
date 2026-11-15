# 网页节点的页面

- `index.html`、`app.js`、`app.css`：页面本身。通行密钥用浏览器的 WebAuthn 接口；协议、会话、重连都在
  WebAssembly 里的网页节点核心（`crates/nexus-web`，包装 `crates/nexus-web-core`）。
- 构建：`tools/build-web.sh`，产物在 `web/dist`。
- 本机试用：`nexus web serve --dir web/dist`，然后
  `nexus web enroll --label dev --page http://localhost:8080`，在浏览器里打开输出的链接。
  中继来自登记链接或通行密钥里的入口信息，都没有时用页面所在的站点。
- 页面不使用任何浏览器存储：临时身份和登录凭证只在内存里。
