# web-enroll-login — 网页端通行密钥登记、登录与终端

## Sub-features
- `enroll` — `nexus web enroll --label <name>` 生成登记链接，指向名册锚点
- `login` — 打开锚点页面，点击「用通行密钥登录」，选择通行密钥
- `terminal` — 登录后点击设备的「终端」按钮，获得 xterm.js 终端
- `leave` — 点击「离开」，会话在设备端保持
- `freeze-thaw` — 页面冻结 20 秒（超过 15 秒断线判定）后恢复，会话存活
- `no-storage` — 关闭页面后浏览器无任何存储残留

## How to get to it (user POV)
```bash
# 前提：laptop 以 --anchor-dev 启动，home 以 --relay 连接，网络已建立

# 1. 登记通行密钥（R29）
nexus web enroll --label phone    # 输出登记 URL，形如 http://anchor:8080/#enroll=...
# 在浏览器打开 URL
# 点击「创建通行密钥」（按钮 ID 为 #enroll-go）→ 完成 WebAuthn 仪式 → 显示「已登记为 phone」

# 2. 登录（R30, R36）
# 打开锚点页面 http://anchor:8080
# 点击「用通行密钥登录」（#login-go）→ 选择 "phone" 通行密钥 → 完成 WebAuthn
# 看到设备列表：home, laptop
# 点击 home 的「终端」→ 等待 shell 提示符（$）
# 输入命令验证：echo hello-$((6*7)) → 输出 hello-42

# 3. 离开后会话还在（R32）
# 点击「离开」（#term-leave）→ 回到设备列表
# nexus session ls home --json 显示会话 main 仍有客户端（或刚离开后客户端=0 但会话存在）

# 4. 冻结恢复（R34, FP-2.5-03）
# 再次进入 home 终端
# 页面冻结 20 秒（CDP: Page.setWebLifecycleState frozen）→ 解冻
# 状态机经历 Notice::Reconnecting → Notice::Reconnected 重连
# 输入命令验证：echo back-$((40+2)) → 输出 back-42

# 5. 关闭不留存储（R32, FP-2.5-02）
# 关闭页面（runBeforeUnload: true）
# 等待 home 上会话客户端变 0
# 重新打开页面 → 显示登录界面，无设备列表
# 检查 localStorage/sessionStorage/cookies/IndexedDB/caches/service workers 均为空
```

## Driving it with verify-nexus
```bash
# web-drive.mjs runs the EXACT flow above with Playwright + virtual WebAuthn
node .pi/skills/verify-nexus/helpers/web-drive.mjs \
  --nexus "$NEXUS_BIN" \
  --dist web/dist \
  --run-root "$RUN_ROOT" \
  --chrome /usr/bin/google-chrome \
  > "$RUN_ROOT/evidence/web/evidence.json"
```
**Selectors used (stable, from web/index.html):**
- `#enroll-go` — enroll page submit button (text is "创建通行密钥")
- `#login-go` — login page "用通行密钥登录" button
- `#device-list li` — device list items
- `#device-list li:has-text('home') button` — home's "终端" button
- `.xterm-rows` — xterm output area (wait for `$` prompt)
- `#term-leave` — "离开" button

**Virtual WebAuthn (CDP):**
```js
await cdp.send("WebAuthn.enable");
await cdp.send("WebAuthn.addVirtualAuthenticator", {
  options: { protocol: "ctap2", transport: "internal", hasResidentKey: true,
    hasUserVerification: true, isUserVerified: true,
    automaticPresenceSimulation: true }
});
```

**Returns JSON evidence** with `login_ms`, `login_to_prompt_ms`, `wasm_gzip_bytes`, `steps[]`.

## Gotchas
- **UI 按钮文字是「创建通行密钥」** — HTML 中按钮标签为 `<button id="enroll-go">创建通行密钥</button>`，不是「登记」。自动化驱动应始终使用 `#enroll-go` 选择器。
- **静态 HTTP 托管 vs 协议层交互** — `web_serve.rs` 是纯静态 HTTP 托管（GET/HEAD），**没有任何 HTTP 登记或登录 endpoint**。所有的登记（`WebEnroll`）和登录（`WebLogin`）均在浏览器 WASM 节点与设备节点之间经由 iroh QUIC 传输层协议（`Proto::Web`）以二进制 wire 格式处理。
- **Anchor must be laptop** — `web-drive.mjs` assumes laptop runs `--anchor-dev` on `127.0.0.1:8080` and home runs `--relay http://127.0.0.1:8080`. `launch-daemons.sh` enforces this.
- **Virtual WebAuthn requires Chromium** — Firefox WebDriver does not support `WebAuthn.addVirtualAuthenticator`. The helper hardcodes Chromium.
- **Enrollment URL points at anchor** — `nexus web enroll` on laptop generates a URL with the anchor's origin. If anchor is not laptop, enrollment fails. This is by design (R29).
- **2-second budget for login→prompt** — FP-2.5-05 requires `login_to_prompt_ms ≤ 2000`. The helper fails if exceeded.
- **WASM size budget 1.2 MB gzip** — FP-2.5-04. The helper fetches `/pkg/nexus_web_bg.wasm`, gzips at level 9, fails if > 1.2 MB.
- **Page freeze uses CDP** — `Page.setWebLifecycleState` requires CDP session. The helper creates one per page.
- **No headless by default** — `--no-sandbox` is passed for CI; headed mode works too. The helper accepts `--headless` flag.
- **Windows Chrome path** — Default `/usr/bin/google-chrome` won't work on Windows. Pass `--chrome "C:\Program Files\Google\Chrome\Application\chrome.exe"` or use `chrome.exe` from PATH.