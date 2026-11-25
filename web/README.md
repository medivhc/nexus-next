# 网页节点的页面

- `index.html`、`app.js`、`share.mjs`、`app.css`：页面本身（含 M9.1 临时分享访客视图 `#share=...`）。`sw.js`：打开网页服务时的转发程序（Service Worker，M8.2 & M9.1）。通行密钥用浏览器的 WebAuthn 接口；协议、会话、重连都在
  WebAssembly 里的网页节点核心（`crates/nexus-web`，包装 `crates/nexus-web-core`）。
- 构建：`tools/build-web.sh`，产物在 `web/dist`。
- 前端单元测试：`node --test web/share.test.mjs`。
- 本机试用：管理设备以开发用的锚点运行
  `nexus daemon --anchor-dev 127.0.0.1:8080 --page-dir web/dist`（同一个端口上是页面和中继），
  `nexus anchor set http://localhost:8080`，然后 `nexus web enroll --label dev`，在浏览器里打开输出的链接；或用 `nexus share create` 生成 `#share=...` 临时分享链接在浏览器里免登录打开。
  只要页面不要中继时也可以 `nexus web serve --dir web/dist`。
  中继来自登记链接、分享链接或通行密钥里的入口信息，都没有时用页面所在的站点。
- 正式部署：见 docs/anchor.md。
- 页面不使用任何浏览器存储：临时身份、登录凭证与访客分享会话只在内存里。

## 浏览器端到端测试（L4）

GitHub 的 CI 里有同样的一步（`web` 任务：构建页面，在无头 Chrome 里跑一遍）。

```sh
tools/build-web.sh
cargo build --release -p nexus-cli
cd tools/web-e2e && npm install
node run.mjs --nexus ../../target/release/nexus --dist ../../web/dist
```

laptop 以开发用的锚点运行，浏览器经它取页面、经它的中继连 laptop 和 home。
覆盖：登记、登录、进会话、离开后会话还在、浏览工作区（目录、文本和图片预览、下载）、打开网页服务（新标签页、图片、绝对路径的脚本和链接、不认得的令牌）、页面冻结 20 秒后自动恢复、关闭后不留存储且设备上的客户端离开、临时分享（免登录打开 `#share=...` 浏览目录与预览文件、页内打开网页服务、只读观看终端、撤销后失效提示）、
WebAssembly 体积预算（gzip ≤ 1.2 MB）、上手步骤和耗时。

| 日期 | 结果（云端机器 Ubuntu 24.04，Chrome 无头模式，每次连续 3 次） |
| --- | --- |
| 2026-11-15（M2.3） | 登录 126–189 毫秒，从点登录到出现提示符约 3.3 秒；WebAssembly gzip 1.45 MB |
| 2026-11-16（M2.4，经锚点） | 登录 120–192 毫秒，从点登录到出现提示符约 3.3 秒 |
| 2026-11-16（M2.5） | 3 步：打开页面 59–81 毫秒，登录 99–120 毫秒，进终端 262–300 毫秒；从点登录到出现提示符 379–399 毫秒；WebAssembly gzip 1.04 MB |

那 3 秒来自中继：自建中继不提供 QUIC 地址探测，设备启动后要等探测超时才选定它，期间经它连不上（M2.5 修正）。
