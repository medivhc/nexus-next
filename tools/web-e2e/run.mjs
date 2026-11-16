// 浏览器端到端测试（测试方案 L4）：真实的 Chromium + 虚拟认证器 + 两台设备的常驻程序。
// laptop 同时是开发用的锚点（--anchor-dev）：在同一个端口上提供页面和中继，浏览器的一切流量都经它。
// 只覆盖关键路径：登记、登录、进会话、输入输出、离开后会话还在、浏览器里不留存储。
//
// 用法：node run.mjs --nexus <nexus 可执行文件> --dist <web/dist> [--chrome <路径>] [--page-port 8080]
import { chromium } from "playwright-core";
import { spawn, execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parseArgs } from "node:util";

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
function nexus(node, args) {
  const out = execFileSync(opt.nexus, ["--data-dir", dataDir(node), ...args], {
    env: { ...process.env, NEXUS_AGENT_SOCKET: join(root, node, "agent.sock"), HOME: join(root, node) },
    encoding: "utf8",
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
  const { url } = nexus("laptop", ["web", "enroll", "--label", "phone", "--json"]);
  if (!url.startsWith(`${pageUrl}/#enroll=`)) throw new Error(`登记链接没有指向锚点：${url}`);
  await page.goto(url);
  await page.click("#enroll-go");
  await page.waitForSelector("text=已登记为 phone", { timeout: 20000 });
  const keys = nexus("home", ["web", "ls", "--json"]).passkeys.map((k) => k.label);
  if (keys.join() !== "phone") throw new Error(`home 上的通行密钥：${keys}`);
  log("登记成功");

  // 登录（R28）并进入 home 的会话（R30）。
  await page.goto(pageUrl);
  const loginStart = Date.now();
  await page.click("#login-go");
  await page.waitForSelector("#device-list li", { timeout: 20000 });
  const devices = await page.$$eval("#device-list .name", (els) => els.map((e) => e.textContent));
  if ([...devices].sort().join() !== "home,laptop") throw new Error(`设备列表：${devices}`);
  const loginMs = Date.now() - loginStart;
  await page.click("#device-list li:has-text('home') button");
  const screen = () => page.$eval(".xterm-rows", (e) => e.innerText);
  await waitFor("提示符", async () => (await screen()).includes("$"), 20000);
  const toPromptMs = Date.now() - loginStart;
  await page.keyboard.type("echo hello-$((6*7))\n");
  await waitFor("命令输出", async () => (await screen()).includes("hello-42"));
  log(`登录 ${loginMs} 毫秒；从点登录到出现提示符 ${toPromptMs} 毫秒`);

  // 离开后会话还在（终端的常驻会话）。
  await page.click("#term-leave");
  await page.waitForSelector("#device-list li");
  const sessions = nexus("home", ["session", "ls", "home", "--json"]);
  if (!JSON.stringify(sessions).includes("main")) throw new Error(`home 上的会话：${JSON.stringify(sessions)}`);

  // 浏览器里不留存储（R32 的一部分；关闭后的检查在 M2.5）。
  const stored = await page.evaluate(async () => ({
    local: localStorage.length, session: sessionStorage.length, cookies: document.cookie,
    idb: (await indexedDB.databases()).length,
  }));
  if (stored.local || stored.session || stored.cookies || stored.idb) throw new Error(`浏览器里有存储：${JSON.stringify(stored)}`);

  const wasm = await fetch(`${pageUrl}/pkg/nexus_web_bg.wasm`).then((r) => r.arrayBuffer());
  console.log(JSON.stringify({ result: "通过", login_ms: loginMs, login_to_prompt_ms: toPromptMs, wasm_bytes: wasm.byteLength }));
} catch (e) {
  console.error("失败：", e.message);
  process.exitCode = 1;
} finally {
  if (browser) await browser.close();
  for (const c of children) c.kill("SIGTERM");
}
