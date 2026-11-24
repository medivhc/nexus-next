// 浏览器端到端测试（测试方案 L4）：真实的 Chromium + 虚拟认证器 + 两台设备的常驻程序。
// laptop 同时是开发用的锚点（--anchor-dev）：在同一个端口上提供页面和中继，浏览器的一切流量都经它。
// 只覆盖关键路径：登记、登录、进会话、输入输出、离开后会话还在、浏览工作区和下载、打开网页服务、
// 访客打开临时分享（文件、网页服务、只读终端）、浏览器里不留存储。
//
// 用法：node run.mjs --nexus <nexus 可执行文件> --dist <web/dist> [--chrome <路径>] [--page-port 8080]
//
// covers: R28, R29, R30, R32, R34, R35, R36, R70, R71, R73
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

  browser = await chromium.launch({ executablePath: opt.chrome, args: ["--no-sandbox"] });
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

  // 临时分享（R73，FP-9.1-01、02、03）：访客不登录，凭链接在页面上只读使用。每个访客一个新的浏览器上下文。
  const guestPage = async (link, viewport) => {
    const ctx = await browser.newContext(viewport ? { viewport } : {});
    const g = await ctx.newPage();
    g.on("pageerror", (e) => console.log("访客页面异常：", e.message));
    await g.goto(link);
    return { ctx, g };
  };
  {
    // 目录：只看得到分享的子目录；分享码不留在地址栏里。
    const { url: dirLink } = nexus("laptop", ["share", "create", "--drive", "work:/notes", "--ttl", "1h", "--json"]);
    if (!dirLink.startsWith(`${pageUrl}/#share=`)) throw new Error(`分享链接没有指向锚点：${dirLink}`);
    const { ctx, g } = await guestPage(dirLink);
    await g.waitForSelector("#file-list a:has-text('hello.txt')", { timeout: 20000 });
    const names = await g.$$eval("#file-list a", (as) => as.map((a) => a.textContent));
    if (names.join() !== "hello.txt") throw new Error(`分享目录里应当只有 hello.txt：${names}`);
    if (g.url().includes("share=")) throw new Error(`分享码留在了地址栏：${g.url()}`);
    if (!(await g.textContent("#who")).includes("只读分享")) throw new Error("页面应当标明这是只读分享");
    await g.click("#file-list a:has-text('hello.txt')");
    await waitFor("分享文件预览", async () => (await g.textContent("#preview-text")).includes("hello from the workspace"));
    await ctx.close();
  }
  {
    // 网页服务：限 1 次，第二个访客被拒绝。
    const { url: svcLink } = nexus("home", ["share", "create", "--service", "web", "--ttl", "1h", "--max-uses", "1", "--json"]);
    const { ctx, g } = await guestPage(svcLink);
    const open = await g.waitForSelector("#share-open", { timeout: 20000 });
    const [svc] = await Promise.all([ctx.waitForEvent("page"), open.click()]);
    await svc.waitForSelector("#msg:has-text('script ran')", { timeout: 20000 });
    await waitFor("分享服务的图片", () => svc.$eval("#pic", (i) => i.complete && i.naturalWidth === 8));
    const second = await guestPage(svcLink);
    await waitFor("次数用完", async () => (await second.g.textContent("#error")).includes("次数"), 20000);
    await second.ctx.close();
    await ctx.close();
  }
  {
    // 只读终端：看得到主人的输出；访客的按键不进终端；访客窗口小也不改变主人的终端尺寸；撤销后立即断开。
    // 尺寸跟着最后打字的客户端：主人先打好一条延后执行的命令，访客在这期间连上，命令量到的就是只有访客在时的尺寸。
    const sizeLater = async (tag, delay) => {
      await page.keyboard.type(`sleep ${delay}; echo ${tag}-$(stty size | tr ' ' x)\n`);
      return () => waitFor("终端尺寸", async () => (await screen()).match(new RegExp(`${tag}-(\\d+x\\d+)`))?.[1], 20000);
    };
    const before = await (await sizeLater("size1", 0))();
    const { id, url: termLink } = nexus("home", ["share", "create", "--terminal", "main", "--ttl", "1h", "--json"]);
    const pending = await sizeLater("size2", 5);
    const { ctx, g } = await guestPage(termLink, { width: 480, height: 320 });
    const guestScreen = () => g.$eval(".xterm-rows", (e) => e.innerText);
    await g.waitForSelector(".xterm-rows", { timeout: 20000 });
    await g.click("#xterm");
    await g.keyboard.type("echo guest-typed\n");
    const after = await pending();
    await waitFor("访客看到主人的输出", async () => (await guestScreen()).includes("size2-"), 20000);
    if (after !== before) throw new Error(`访客改变了主人的终端尺寸：${before} → ${after}`);
    if ((await screen()).includes("guest-typed")) throw new Error("访客的按键进了终端");
    nexus("home", ["share", "revoke", id, "--json"]);
    await waitFor("撤销后断开", async () => (await g.textContent("#share-title")).includes("观看已结束"), 20000);
    await ctx.close();
  }
  log("临时分享：文件、网页服务、只读终端");

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
