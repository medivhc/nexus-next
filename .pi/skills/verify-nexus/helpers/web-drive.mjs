// Web verification drive for nexus — Playwright + virtual WebAuthn
// Adapted from tools/web-e2e/run.mjs
// Usage: node web-drive.mjs --nexus <bin> --dist <dist> --run-root <root> [--chrome <path>] [--headless] [--port 8080]
import { chromium } from "playwright-core";
import { spawn, execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parseArgs } from "node:util";
import { gzipSync } from "node:zlib";

const { values: opt } = parseArgs({
  options: {
    nexus: { type: "string", short: "n" },
    dist: { type: "string", short: "d" },
    "run-root": { type: "string", short: "r" },
    chrome: { type: "string", default: "/usr/bin/google-chrome" },
    headless: { type: "boolean", default: false },
    port: { type: "string", default: "8080" },
  },
  allowPositionals: true,
});

for (const k of ["nexus", "dist", "run-root"]) {
  if (!opt[k]) {
    console.error(`Missing required option: --${k}`);
    process.exit(1);
  }
}

const pageUrl = `http://127.0.0.1:${opt.port}`;
const { nexus, dist, "run-root": runRoot, chrome, headless, port } = opt;

const t0 = Date.now();
const log = (...a) => console.error(`[${((Date.now() - t0) / 1000).toFixed(1)}s]`, ...a);

function nexusCmd(node, args) {
  const out = execFileSync(nexus, ["--data-dir", join(runRoot, node, "data"), ...args], {
    env: { ...process.env, NEXUS_AGENT_SOCKET: join(runRoot, node, "agent.sock"), HOME: join(runRoot, node) },
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
      if (Date.now() > end) throw new Error(`${what}: ${e.message}`);
    }
    if (Date.now() > end) throw new Error(`${what}: ${ms}ms timeout`);
    await new Promise((r) => setTimeout(r, 200));
  }
}

let browser;
try {
  // Ensure network is set up (cli-drive.sh should have run already)
  // But we verify and run anchor set if needed
  const laptopStatus = nexusCmd("laptop", ["status", "--json"]);
  const homeStatus = nexusCmd("home", ["status", "--json"]);
  if (!laptopStatus.online || !homeStatus.online) throw new Error("Daemons not online");

  // Ensure anchor is set (idempotent)
  nexusCmd("laptop", ["anchor", "set", pageUrl, "--json"]);

  // Verify anchor serves page
  await waitFor("anchor HTTP", () => fetch(pageUrl).then((r) => r.ok));

  // Verify daemons ready
  await waitFor("daemons", () => nexusCmd("laptop", ["status", "--json"]) && nexusCmd("home", ["status", "--json"]));

  log("Network ready, launching browser");

  browser = await chromium.launch({
    executablePath: chrome,
    headless,
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });

  const context = await browser.newContext();
  const page = await context.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.error("Page error:", m.text()); });
  page.on("pageerror", (e) => console.error("Page exception:", e.message));

  const cdp = await context.newCDPSession(page);
  await cdp.send("WebAuthn.enable");
  await cdp.send("WebAuthn.addVirtualAuthenticator", {
    options: {
      protocol: "ctap2",
      transport: "internal",
      hasResidentKey: true,
      hasUserVerification: true,
      isUserVerified: true,
      automaticPresenceSimulation: true,
    },
  });

  const steps = [];
  let mark = Date.now();
  const step = (name) => {
    const now = Date.now();
    steps.push({ step: name, ms: now - mark });
    mark = now;
  };

  // 1. Enrollment (R29)
  const { url } = nexusCmd("laptop", ["web", "enroll", "--label", "phone", "--json"]);
  if (!url.startsWith(`${pageUrl}/#enroll=`)) throw new Error(`Enrollment URL doesn't point to anchor: ${url}`);
  await page.goto(url);
  await page.click("#enroll-go");
  await page.waitForSelector("text=已登记为 phone", { timeout: 20000 });
  log("Enrollment done");

  // 2. Login + terminal (R30, R36)
  step("打开页面");
  await page.goto(pageUrl);
  await page.waitForSelector("#login-go");
  const loginStart = Date.now();

  await page.click("#login-go");
  await page.waitForSelector("#device-list li", { timeout: 20000 });
  step("点「用通行密钥登录」并选通行密钥");

  const devices = await page.$$eval("#device-list .name", (els) => els.map((e) => e.textContent));
  if ([...devices].sort().join() !== "home,laptop") throw new Error(`Device list: ${devices}`);

  const loginMs = Date.now() - loginStart;
  await page.click("#device-list li:has-text('home') button");

  const screen = () => page.$eval(".xterm-rows", (e) => e.innerText);
  await waitFor("提示符", async () => (await screen()).includes("$"), 20000);
  step("点 home 的「终端」");
  const toPromptMs = Date.now() - loginStart;

  // Type test command
  await page.keyboard.type("echo hello-$((6*7))\n");
  await waitFor("命令输出", async () => (await screen()).includes("hello-42"));

  // Screenshot terminal
  await page.screenshot({ path: join(runRoot, "evidence", "web", "screenshot-terminal.png") });

  // 3. Leave — session persists (R32)
  await page.click("#term-leave");
  await page.waitForSelector("#device-list li");
  const clientsOf = (name) => (nexusCmd("home", ["session", "ls", "home", "--json"]).sessions || []).find((s) => s.name === name);
  if (!clientsOf("main")) throw new Error("After leave, home should still have session main");

  // 4. Freeze-thaw 20s (R34, FP-2.5-03)
  await page.click("#device-list li:has-text('home') button");
  await waitFor("提示符", async () => (await screen()).includes("$"), 20000);
  const cdpPage = await context.newCDPSession(page);
  await cdpPage.send("Page.setWebLifecycleState", { state: "frozen" });
  await new Promise((r) => setTimeout(r, 20000));
  await cdpPage.send("Page.setWebLifecycleState", { state: "active" });
  await page.keyboard.type("echo back-$((40+2))\n");
  await waitFor("冻结后恢复", async () => (await screen()).includes("back-42"), 20000);
  log("Freeze-thaw passed");

  // Screenshot after thaw
  await page.screenshot({ path: join(runRoot, "evidence", "web", "screenshot-frozen.png") });

  // 5. Close page — no storage left (R32, FP-2.5-02)
  await waitFor("会话有客户端", () => clientsOf("main")?.clients === 1);
  await page.close({ runBeforeUnload: true });
  await waitFor("关闭页面后客户端离开", () => clientsOf("main")?.clients === 0, 30000);

  const after = await context.newPage();
  await after.goto(pageUrl);
  const stored = await after.evaluate(async () => ({
    local: localStorage.length,
    session: sessionStorage.length,
    cookies: document.cookie,
    idb: (await indexedDB.databases()).length,
    caches: (await caches.keys()).length,
    workers: (await navigator.serviceWorker.getRegistrations()).length,
  }));
  if (Object.values(stored).some((v) => v)) throw new Error(`Storage after close: ${JSON.stringify(stored)}`);

  // Verify login page shown again
  await after.waitForSelector("#login-go", { state: "visible", timeout: 10000 });
  if (await after.isVisible("#device-list li")) throw new Error("Reopened page shows device list (should be login)");

  // 6. WASM size budget (FP-2.5-04)
  const wasm = new Uint8Array(await fetch(`${pageUrl}/pkg/nexus_web_bg.wasm`).then((r) => r.arrayBuffer()));
  const gzipBytes = gzipSync(wasm, { level: 9 }).length;
  if (gzipBytes > 1.2 * 1024 * 1024) throw new Error(`WASM gzip ${gzipBytes} bytes > 1.2 MB budget`);

  // Success evidence
  const evidence = {
    ok: true,
    enrollment_url: url,
    login_ms: loginMs,
    login_to_prompt_ms: toPromptMs,
    wasm_bytes: wasm.byteLength,
    wasm_gzip_bytes: gzipBytes,
    steps,
  };

  console.log(JSON.stringify(evidence));
  process.exitCode = 0;
} catch (e) {
  console.error("FAIL:", e.message);
  const evidence = { ok: false, error: e.message };
  console.log(JSON.stringify(evidence));
  process.exitCode = 1;
} finally {
  if (browser) await browser.close();
}