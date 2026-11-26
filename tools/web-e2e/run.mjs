// 浏览器端到端测试（测试方案 L4）：真实的 Chromium + 虚拟认证器 + 两台设备的常驻程序。
// laptop 同时是开发用的锚点（--anchor-dev）：在同一个端口上提供页面和中继，浏览器的一切流量都经它。
// 只覆盖关键路径：登记、登录、进会话、输入输出、离开后会话还在、浏览工作区和下载、打开网页服务、硬件能力、临时分享、浏览器里不留存储。
//
// 用法：node run.mjs --nexus <nexus 可执行文件> --dist <web/dist> [--chrome <路径>] [--page-port 8080]
//
// covers: R28, R29, R30, R32, R34, R35, R36, R70, R71, R72, R73
import { chromium } from "playwright-core";
import { spawn, execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parseArgs } from "node:util";
import { gzipSync } from "node:zlib";

const { values: opt } = parseArgs({
  options: {
    nexus: { type: "string" },
    dist: { type: "string" },
    chrome: { type: "string", default: "/usr/bin/google-chrome" },
    "page-port": { type: "string", default: "8080" },
  },
});
for (const k of ["nexus", "dist"]) if (!opt[k]) throw new Error(`缺少 --${k}`);

const root = mkdtempSync(join(tmpdir(), "nexus-web-e2e-"));
const pageUrl = `http://localhost:${opt["page-port"]}`;
const children = [];
const t0 = Date.now();
const log = (...a) => console.log(`[${((Date.now() - t0) / 1000).toFixed(1)}s]`, ...a);

function background(cmd, args, env = {}) {
  const p = spawn(cmd, args, { env: { ...process.env, ...env }, stdio: ["ignore", "pipe", "pipe"] });
  let out = "";
  p.stdout.on("data", (d) => (out += d));
  p.stderr.on("data", (d) => (out += d));
  children.push(p);
  return { proc: p, output: () => out };
}

const dataDir = (n) => join(root, n, "data");
function nexus(node, args, input) {
  const out = execFileSync(opt.nexus, ["--data-dir", dataDir(node), ...args], {
    env: { ...process.env, NEXUS_AGENT_SOCKET: join(root, node, "agent.sock"), HOME: join(root, node) },
    encoding: "utf8",
    input,
  });
  return out.trim().startsWith("{") ? JSON.parse(out) : out;
}

async function waitFor(what, fn, ms = 15000) {
  const end = Date.now() + ms;
  for (;;) {
    try {
      const v = await fn();
      if (v) return v;
    } catch (e) {
      if (Date.now() > end) throw new Error(`${what}：${e.message}`);
    }
    if (Date.now() > end) throw new Error(`${what}：${ms} 毫秒内没有等到`);
    await new Promise((r) => setTimeout(r, 200));
  }
}

let browser;
try {
  // laptop 是锚点（页面和中继），home 只以它为中继。
  const daemonArgs = {
    laptop: ["--anchor-dev", `127.0.0.1:${opt["page-port"]}`, "--page-dir", opt.dist],
    home: ["--relay", pageUrl],
  };
  for (const n of ["laptop", "home"]) {
    mkdirSync(join(root, n), { recursive: true });
    background(opt.nexus, ["--data-dir", dataDir(n), "daemon", "--name", n, ...daemonArgs[n], "--no-discovery",
      "--agent-socket", join(root, n, "agent.sock")], { HOME: join(root, n), SHELL: "/bin/sh", PS1: "$ " });
  }
  await waitFor("锚点", () => fetch(pageUrl).then((r) => r.ok));
  await waitFor("常驻程序", () => nexus("laptop", ["status", "--json"]) && nexus("home", ["status", "--json"]));
  nexus("laptop", ["init", "--name", "laptop", "--json"]);
  const { code } = nexus("home", ["join", "--json"]);
  nexus("laptop", ["device", "approve", code, "--name", "home", "--json"]);
  nexus("laptop", ["anchor", "set", pageUrl, "--json"]);
  // 工作区（R70）：laptop 同时是存储设备。
  nexus("laptop", ["drive", "serve", "on", "--json"]);
  nexus("laptop", ["drive", "create", "work", "--json"]);
  nexus("laptop", ["drive", "put", "work:/notes/hello.txt", "--json"], "hello from the workspace\n");
  nexus("laptop", ["drive", "put", "work:/dot.svg", "--json"],
    '<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><circle cx="4" cy="4" r="4"/></svg>');
  nexus("laptop", ["drive", "commit", "work", "-m", "e2e", "--json"]);
  // 网页服务（R71）：home 上一个普通的 HTTP 服务器托管的小网站。
  const site = join(root, "site");
  mkdirSync(join(site, "static"), { recursive: true });
  writeFileSync(join(site, "index.html"), `<!doctype html><title>home web</title>
<p id="msg">waiting</p><img id="pic" src="dot.svg"><a id="about" href="/about.html">about</a>
<script src="/static/app.js"></script>`);
  writeFileSync(join(site, "static", "app.js"), `document.getElementById("msg").textContent = "script ran";`);
  writeFileSync(join(site, "dot.svg"), '<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><circle cx="4" cy="4" r="4"/></svg>');
  writeFileSync(join(site, "about.html"), `<!doctype html><title>about</title><p id="about-text">about page</p>`);
  const sitePort = String(Number(opt["page-port"]) + 1);
  background("python3", ["-m", "http.server", sitePort, "--bind", "127.0.0.1", "--directory", site]);
  await waitFor("网站", () => fetch(`http://127.0.0.1:${sitePort}/`).then((r) => r.ok));
  nexus("home", ["publish", "web", "--port", sitePort, "--proto", "http", "--json"]);
  log("网络就绪");

  // 假的摄像头和麦克风，权限弹窗自动同意（硬件能力，R72）。
  browser = await chromium.launch({ executablePath: opt.chrome,
    args: ["--no-sandbox", "--use-fake-device-for-media-stream", "--use-fake-ui-for-media-stream"] });
  const context = await browser.newContext();
  const page = await context.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.log("页面错误：", m.text()); });
  page.on("pageerror", (e) => console.log("页面异常：", e.message));
  const cdp = await context.newCDPSession(page);
  await cdp.send("WebAuthn.enable");
  await cdp.send("WebAuthn.addVirtualAuthenticator", {
    options: { protocol: "ctap2", transport: "internal", hasResidentKey: true, hasUserVerification: true,
      isUserVerified: true, automaticPresenceSimulation: true },
  });

  // 登记（R29）。
  // 不给 --page：登记链接指向名册里的锚点。
  const { url } = nexus("laptop", ["web", "enroll", "--label", "phone", "--grant", "term.session.*@**",
    "--grant", "node.info@**", "--grant", "drive.read@**", "--grant", "svc.connect@**", "--json"]);
  if (!url.startsWith(`${pageUrl}/#enroll=`)) throw new Error(`登记链接没有指向锚点：${url}`);
  await page.goto(url);
  await page.click("#enroll-go");
  await page.waitForSelector("text=已登记为 phone", { timeout: 20000 });
  const keys = nexus("home", ["web", "ls", "--json"]).passkeys.map((k) => k.label);
  if (keys.join() !== "phone") throw new Error(`home 上的通行密钥：${keys}`);
  log("登记成功");

  // 上手成本（R36，FP-2.5-05）：打开页面 → 点登录并选通行密钥 → 点终端，共 3 步，每步计时。
  const steps = [];
  let mark = Date.now();
  const step = (name) => { const now = Date.now(); steps.push({ step: name, ms: now - mark }); mark = now; };
  await page.goto(pageUrl);
  await page.waitForSelector("#login-go");
  step("打开页面");
  const loginStart = Date.now();
  await page.click("#login-go");
  await page.waitForSelector("#device-list li", { timeout: 20000 });
  step("点「用通行密钥登录」并选通行密钥");
  const devices = await page.$$eval("#device-list .name", (els) => els.map((e) => e.textContent));
  if ([...devices].sort().join() !== "home,laptop") throw new Error(`设备列表：${devices}`);
  const loginMs = Date.now() - loginStart;
  await page.click("#device-list li:has-text('home') button");
  const screen = () => page.$eval(".xterm-rows", (e) => e.innerText);
  await waitFor("提示符", async () => (await screen()).includes("$"), 20000);
  step("点 home 的「终端」");
  const toPromptMs = Date.now() - loginStart;
  await page.keyboard.type("echo hello-$((6*7))\n");
  await waitFor("命令输出", async () => (await screen()).includes("hello-42"));
  log(`登录 ${loginMs} 毫秒；从点登录到出现提示符 ${toPromptMs} 毫秒`);
  if (toPromptMs > 2000) throw new Error(`从点登录到出现提示符用了 ${toPromptMs} 毫秒，超过 2 秒`);

  // 离开后会话还在（终端的常驻会话）。
  await page.click("#term-leave");
  await page.waitForSelector("#device-list li");
  const clientsOf = (name) => (nexus("home", ["session", "ls", "home", "--json"]).sessions || []).find((s) => s.name === name);
  if (!clientsOf("main")) throw new Error("离开后 home 上应当还有会话 main");

  // 页面被冻结 20 秒（超过设备判定断线的 15 秒）后自动恢复（R34，FP-2.5-03）。
  await page.click("#device-list li:has-text('home') button");
  await waitFor("提示符", async () => (await screen()).includes("$"), 20000);
  const cdpPage = await context.newCDPSession(page);
  await cdpPage.send("Page.setWebLifecycleState", { state: "frozen" });
  await new Promise((r) => setTimeout(r, 20000));
  await cdpPage.send("Page.setWebLifecycleState", { state: "active" });
  await page.keyboard.type("echo back-$((40+2))\n");
  await waitFor("冻结后恢复", async () => (await screen()).includes("back-42"), 20000);
  log("冻结 20 秒后恢复");

  // 浏览工作区、预览、下载（R70，FP-8.1-01、02）。
  await page.click("#term-leave");
  await page.waitForSelector("#ws-list li:has-text('work')", { timeout: 20000 });
  await page.click("#ws-list li:has-text('work') button");
  await page.click("#file-list a[data-kind='dir']:has-text('notes')");
  await page.click("#file-list a:has-text('hello.txt')");
  await waitFor("文本预览", async () => (await page.textContent("#preview-text")).includes("hello from the workspace"));
  const [download] = await Promise.all([page.waitForEvent("download"), page.click("#preview-download")]);
  const { readFileSync } = await import("node:fs");
  const saved = join(root, "downloaded.txt");
  await download.saveAs(saved);
  if (readFileSync(saved, "utf8") !== "hello from the workspace\n") throw new Error("下载的内容不对");
  await page.click("#files-back");
  await page.click("#file-list a:has-text('dot.svg')");
  await page.waitForSelector("#preview-image:not([hidden])");
  await waitFor("图片预览", () => page.$eval("#preview-image", (i) => i.complete && i.naturalWidth === 8));
  await page.click("#files-back");
  await page.waitForSelector("#device-list li");
  log("浏览工作区、预览、下载");

  // 硬件能力（R72，FP-8.3-05）：页面上逐项开启；页面显示的命令原样在设备上能用；关闭后立即拒绝。
  await context.grantPermissions(["geolocation", "camera", "microphone"], { origin: pageUrl });
  await context.setGeolocation({ latitude: 31.2304, longitude: 121.4737, accuracy: 12.5 });
  const hwCmd = (await page.textContent("#hw-cmd")).trim().split("\n");
  if (!hwCmd[0].startsWith("nexus hw ls ") || !hwCmd[1].startsWith("nexus hw capture ")) throw new Error(`页面上的命令：${hwCmd}`);
  const run = (cmd) => nexus("home", [...cmd.split(/\s+/).slice(1), "--json"]);
  if (run(hwCmd[0]).capabilities.length !== 0) throw new Error("默认应当一项都不开放");
  await page.click("#hw-location");
  await page.waitForSelector("#hw-location:has-text('关闭')");
  await page.click("#hw-camera");
  await page.waitForSelector("#hw-camera:has-text('关闭')");
  const caps = run(hwCmd[0]).capabilities;
  if ([...caps].sort().join() !== "camera,location") throw new Error(`开放的能力：${caps}`);
  const loc = run(hwCmd[1]);
  if (Math.abs(loc.lat_e7 - 312304000) > 10 || Math.abs(loc.lon_e7 - 1214737000) > 10) throw new Error(`位置：${JSON.stringify(loc)}`);
  const cam = run(hwCmd[1].replace(" location ", " camera "));
  if (cam.mime !== "image/jpeg" || Buffer.from(cam.data_base64, "base64").subarray(0, 2).toString("hex") !== "ffd8") {
    throw new Error(`照片：${cam.mime} ${cam.data_base64?.slice(0, 16)}`);
  }
  await waitFor("页面显示谁用过", async () => (await page.textContent("#hw-active")).length > 0);
  await page.click("#hw-location");
  await page.waitForSelector("#hw-location:has-text('开启')");
  let refused = null;
  try { run(hwCmd[1]); } catch (e) { refused = e.status; }
  if (refused !== 105) throw new Error(`关闭位置后采集应当被拒绝（105），实际：${refused}`);
  await page.click("#hw-camera");
  log("硬件能力：开启、设备上取用、关闭");

  // 打开网页服务（R71，FP-8.2-03）：新标签页，相对路径的图片、绝对路径的脚本、绝对路径的链接都经 Nexus 到 home。
  const link = await page.waitForSelector("#device-list li:has-text('home') a.svc:has-text('web')", { timeout: 20000 });
  const [svcPage] = await Promise.all([context.waitForEvent("page"), link.click()]);
  svcPage.on("pageerror", (e) => console.log("服务页面异常：", e.message));
  await svcPage.waitForSelector("#msg:has-text('script ran')", { timeout: 20000 });
  await waitFor("服务页面的图片", () => svcPage.$eval("#pic", (i) => i.complete && i.naturalWidth === 8));
  if (!new URL(svcPage.url()).pathname.match(/^\/svc\/[0-9a-f]{32}\/$/)) throw new Error(`服务页面的地址：${svcPage.url()}`);
  await svcPage.click("#about");
  await svcPage.waitForSelector("#about-text", { timeout: 20000 });
  if (!svcPage.url().includes("/svc/") || !svcPage.url().endsWith("/about.html")) throw new Error(`绝对路径的链接没有留在服务里：${svcPage.url()}`);
  // 不认得的令牌：不转发。
  const stranger = await context.newPage();
  const resp = await stranger.goto(`${pageUrl}/svc/${"0".repeat(32)}/`);
  if (resp.status() !== 503) throw new Error(`不认得的令牌应当是 503，实际 ${resp.status()}`);
  await stranger.close();
  await svcPage.close();
  log("打开网页服务");
  await page.click("#device-list li:has-text('home') button");
  await waitFor("提示符", async () => (await screen()).includes("$"), 20000);

  // 关闭即离开（R32，FP-2.5-02）：关掉页面，同一个浏览器里不留任何东西，home 上的客户端离开。
  await waitFor("会话有客户端", () => clientsOf("main")?.clients === 1);
  await page.close({ runBeforeUnload: true });
  // 页面来得及时主动离开；来不及时（例如直接被杀掉）设备 15 秒没有心跳就判定断开。
  await waitFor("关闭页面后客户端离开", () => clientsOf("main")?.clients === 0, 30000);
  const after = await context.newPage();
  await after.goto(pageUrl);
  // 页面打开时先清掉残留的（转发程序等），然后才显示登录。
  await after.waitForSelector("#login-go", { state: "visible", timeout: 10000 });
  const stored = await after.evaluate(async () => ({
    local: localStorage.length, session: sessionStorage.length, cookies: document.cookie,
    idb: (await indexedDB.databases()).length, caches: (await caches.keys()).length,
    workers: (await navigator.serviceWorker.getRegistrations()).length,
  }));
  if (Object.values(stored).some((v) => v)) throw new Error(`关闭后浏览器里有存储：${JSON.stringify(stored)}`);
  // 重新打开后是登录界面：没有任何东西能恢复之前的登录。
  if (await after.isVisible("#device-list li")) throw new Error("重新打开页面后不应当直接看到设备");
  log("关闭后不留存储");

  // 临时分享（M9.1，R73，FP-9.1-04）：使用全新无登录浏览器上下文，验证目录/文件预览、页内网页服务、只读终端与撤销失效状态。
  const guestCtx = await browser.newContext();
  try {
    // 1. 文件/目录分享（含剩余次数与过期时间、目录导航与文件预览）
    const driveShare = nexus("laptop", ["share", "create", "--drive", "work:/notes", "--ttl", "1h", "--max-uses", "2", "--json"]);
    const drivePage = await guestCtx.newPage();
    drivePage.on("pageerror", (e) => console.log("分享页异常：", e.message));
    await drivePage.goto(driveShare.url);
    await drivePage.waitForSelector("#share-view:not([hidden])", { timeout: 20000 });
    await drivePage.waitForSelector("#share-body .share-file-list a:has-text('hello.txt')", { timeout: 20000 });
    const driveMeta = await drivePage.textContent("#share-meta");
    if (!driveMeta.includes("剩余 1 次") || !driveMeta.includes("过期时间")) {
      throw new Error(`分享元信息缺少剩余次数或过期时间：${driveMeta}`);
    }
    await drivePage.click("#share-body .share-file-list a:has-text('hello.txt')");
    await waitFor("分享文件预览", async () => (await drivePage.textContent("#share-body .share-preview-text")).includes("hello from the workspace"), 20000);
    await drivePage.close();

    // 2. 网页服务分享（页内 iframe，验证 GuestSession.http_fetch 返回 headers 与 reason）
    const svcShare = nexus("home", ["share", "create", "--service", "web", "--ttl", "1h", "--json"]);
    const svcSharePage = await guestCtx.newPage();
    svcSharePage.on("pageerror", (e) => console.log("服务分享页异常：", e.message));
    await svcSharePage.goto(svcShare.url);
    const frameEl = await svcSharePage.waitForSelector("#share-body iframe.share-service-frame", { timeout: 20000 });
    const frame = await frameEl.contentFrame();
    if (!frame) throw new Error("网页服务分享 iframe 未挂载");
    await frame.waitForSelector("#msg:has-text('script ran')", { timeout: 20000 });
    await svcSharePage.close();

    // 3. 只读终端分享（验证 GuestSession.watch_terminal 输出）
    const termShare = nexus("home", ["share", "create", "--terminal", "main", "--ttl", "1h", "--json"]);
    const termSharePage = await guestCtx.newPage();
    termSharePage.on("pageerror", (e) => console.log("终端分享页异常：", e.message));
    await termSharePage.goto(termShare.url);
    await termSharePage.waitForSelector("#share-body .share-xterm .xterm-rows", { timeout: 20000 });
    await waitFor("只读终端输出", async () => {
      const text = await termSharePage.$eval("#share-body .share-xterm .xterm-rows", (e) => e.innerText);
      return text.includes("back-42") || text.includes("$");
    }, 20000);
    await termSharePage.close();

    // 4. 撤销后打开显示失效状态
    nexus("home", ["share", "revoke", svcShare.id, "--json"]);
    const revokedPage = await guestCtx.newPage();
    await revokedPage.goto(svcShare.url);
    await revokedPage.waitForSelector("#share-body .share-state[data-state='expired']", { timeout: 20000 });
    await revokedPage.close();
    log("临时分享（文件/目录、网页服务、只读终端、撤销失效）");
  } finally {
    await guestCtx.close();
  }

  // 体积预算（FP-2.5-04）。
  const wasm = new Uint8Array(await fetch(`${pageUrl}/pkg/nexus_web_bg.wasm`).then((r) => r.arrayBuffer()));
  const gzipBytes = gzipSync(wasm, { level: 9 }).length;
  if (gzipBytes > 1.2 * 1024 * 1024) throw new Error(`WebAssembly gzip 后 ${gzipBytes} 字节，超过 1.2 MB 预算`);

  console.log(JSON.stringify({
    result: "通过", steps, step_count: steps.length, login_ms: loginMs, login_to_prompt_ms: toPromptMs,
    wasm_bytes: wasm.byteLength, wasm_gzip_bytes: gzipBytes,
  }));
} catch (e) {
  console.error("失败：", e.message);
  process.exitCode = 1;
} finally {
  if (browser) await browser.close();
  for (const c of children) c.kill("SIGTERM");
}
