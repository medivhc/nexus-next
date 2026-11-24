// 网页节点的页面：通行密钥（浏览器的 WebAuthn）+ 网页节点核心（WebAssembly）+ 终端显示（xterm.js）。
// 不使用任何浏览器存储：临时身份和登录凭证只在内存里，关掉页面即消失。
import init, { Browser } from "./pkg/nexus_web.js";

const $ = (id) => document.getElementById(id);
const show = (id) => { for (const s of ["enroll", "login", "share-view", "devices", "files", "terminal"]) $(s).hidden = s !== id; };
const status = (text) => { $("status").textContent = text; };
const fail = (e) => { $("error").textContent = String(e && e.message ? e.message : e); };
const bytes = (buf) => new Uint8Array(buf);

// 中继来自登记链接或通行密钥里的入口信息；都没有时是页面所在的站点（锚点在同一台机器上提供页面和中继）。
const relay = location.origin;
const rpId = location.hostname;

async function enroll(link) {
  show("enroll");
  $("enroll-go").onclick = async () => {
    $("error").textContent = "";
    try {
      const browser = new Browser(relay);
      const cred = await navigator.credentials.create({
        publicKey: {
          rp: { id: rpId, name: "Nexus" },
          user: { id: Browser.user_handle(link), name: "Nexus", displayName: "Nexus" },
          challenge: crypto.getRandomValues(new Uint8Array(32)),
          pubKeyCredParams: [{ type: "public-key", alg: -7 }],
          authenticatorSelection: { residentKey: "required", userVerification: "required" },
        },
      });
      const spki = cred.response.getPublicKey();
      if (!spki) throw new Error("这个浏览器不支持读取通行密钥的公钥");
      status("连接中…");
      const label = await browser.enroll(link, bytes(cred.rawId), bytes(spki), rpId);
      history.replaceState(null, "", location.pathname + location.search);
      status("");
      $("enroll").innerHTML = `<h1>已登记为 ${escapeHtml(label)}</h1><p>现在可以 <a href="${location.pathname + location.search}">登录</a>。</p>`;
    } catch (e) { status(""); fail(e); }
  };
}

async function login() {
  show("login");
  $("login-go").onclick = async () => {
    $("error").textContent = "";
    try {
      // 每次尝试都用新的临时身份：一个身份只能登录一次。
      const browser = new Browser(relay);
      const issuedAt = Date.now();
      const got = await navigator.credentials.get({
        publicKey: { challenge: browser.challenge(issuedAt), rpId, userVerification: "required" },
      });
      const r = got.response;
      if (!r.userHandle) throw new Error("这个通行密钥不是 Nexus 的");
      status("连接中…");
      const session = await browser.login(
        bytes(r.userHandle), issuedAt, bytes(got.rawId),
        bytes(r.clientDataJSON), bytes(r.authenticatorData), bytes(r.signature),
      );
      status("");
      $("who").textContent = `${session.label()} · ${new Date(session.expires_at_ms()).toLocaleTimeString()} 前有效`;
      currentSession = session;
      devices(session);
    } catch (e) { status(""); fail(e); }
  };
}

function devices(session) {
  show("devices");
  const list = $("device-list");
  list.innerHTML = "";
  for (const name of JSON.parse(session.devices())) {
    const li = document.createElement("li");
    li.innerHTML = `<span class="name">${escapeHtml(name)}</span>`;
    const open = document.createElement("button");
    open.textContent = "终端";
    open.onclick = () => terminal(session, name, "main");
    li.append(open);
    list.append(li);
    webServices(session, name, li);
  }
  hardwareControls(session);
  workspaces(session);
}

// ---- 本机硬件能力发布（M8.3）：默认关闭、逐项在浏览器中明确同意、页面显示谁在用、关闭即停止。 ----

const hwStreams = { camera: null, microphone: null };

function stopHardwareAll(session) {
  for (const key of ["camera", "microphone"]) {
    if (hwStreams[key]) {
      for (const t of hwStreams[key].getTracks()) t.stop();
      hwStreams[key] = null;
    }
  }
  try { session.hardware_clear(); } catch {}
}

function renderHardwareStatus(session) {
  const enabled = new Set(JSON.parse(session.hardware_enabled()));
  for (const [kind, id] of [["camera", "hw-camera"], ["microphone", "hw-microphone"], ["location", "hw-location"]]) {
    const btn = $(id);
    if (btn) btn.textContent = enabled.has(kind) ? "关闭" : "开启";
  }
  const active = JSON.parse(session.hardware_active());
  const banner = $("hw-active");
  if (banner) {
    banner.textContent = active.length
      ? `正在使用：${active.map((a) => `${a.caller}（${a.kind}）`).join("、")}`
      : "当前无人使用本机硬件。";
  }
}

function hardwareControls(session) {
  renderHardwareStatus(session);
  const toggle = async (kind) => {
    $("error").textContent = "";
    const enabled = new Set(JSON.parse(session.hardware_enabled()));
    try {
      if (enabled.has(kind)) {
        if (hwStreams[kind]) {
          for (const t of hwStreams[kind].getTracks()) t.stop();
          hwStreams[kind] = null;
        }
        session.hardware_disable(kind);
      } else {
        if (kind === "camera") {
          hwStreams.camera = await navigator.mediaDevices.getUserMedia({ video: true });
        } else if (kind === "microphone") {
          hwStreams.microphone = await navigator.mediaDevices.getUserMedia({ audio: true });
        } else if (kind === "location") {
          await new Promise((resolve, reject) =>
            navigator.geolocation.getCurrentPosition(resolve, reject, { timeout: 10000 }),
          );
        }
        session.hardware_enable(kind);
      }
      renderHardwareStatus(session);
    } catch (e) { fail(e); }
  };
  $("hw-camera").onclick = () => toggle("camera");
  $("hw-microphone").onclick = () => toggle("microphone");
  $("hw-location").onclick = () => toggle("location");
  addEventListener("pagehide", () => stopHardwareAll(session), { once: true });
}

// ---- 网页服务（M8.2）：在新标签页里打开，请求经转发程序（sw.js）交回这里，由网页节点核心发出。 ----
// 令牌 → { device, service, jar }：令牌只在内存里，cookie 也只在内存里。

const services = new Map();
let currentSession = null;

function randomToken() {
  return Array.from(crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, "0")).join("");
}

// 注册转发程序（只在有网页服务可开时）；离开页面时注销。
async function ensureForwarder() {
  if (!("serviceWorker" in navigator)) throw new Error("这个浏览器不支持打开网页服务");
  await navigator.serviceWorker.register("sw.js", { scope: "./" });
  await navigator.serviceWorker.ready;
}

// 清掉这个站点下的一切浏览器存储和转发程序：页面自己不用存储，网页服务的页面和它同源，可能用了。
async function wipe() {
  try { localStorage.clear(); sessionStorage.clear(); } catch { /* 不可用 */ }
  for (const c of document.cookie.split(";")) {
    const name = c.split("=")[0].trim();
    if (name) document.cookie = `${name}=; Max-Age=0; path=/`;
  }
  const jobs = [];
  if (navigator.serviceWorker) jobs.push(navigator.serviceWorker.getRegistrations().then((rs) => Promise.all(rs.map((r) => r.unregister()))));
  if (self.indexedDB && indexedDB.databases) jobs.push(indexedDB.databases().then((dbs) => dbs.forEach((d) => indexedDB.deleteDatabase(d.name))));
  if (self.caches) jobs.push(caches.keys().then((ks) => Promise.all(ks.map((k) => caches.delete(k)))));
  await Promise.allSettled(jobs);
}

function remember(jar, setCookie) {
  const [pair, ...attrs] = setCookie.split(";");
  const eq = pair.indexOf("=");
  if (eq <= 0) return;
  const name = pair.slice(0, eq).trim();
  const value = pair.slice(eq + 1).trim();
  const gone = attrs.some((a) => {
    const [k, v = ""] = a.split("=").map((x) => x.trim());
    if (k.toLowerCase() === "max-age") return Number(v) <= 0;
    if (k.toLowerCase() === "expires") return Date.parse(v) < Date.now();
    return false;
  });
  if (gone) jar.delete(name); else jar.set(name, value);
}

async function onForward(event) {
  const m = event.data;
  const port = event.ports[0];
  if (!m || m.type !== "nexus-http" || !port) return;
  const svc = services.get(m.token);
  if (!svc || !currentSession) { port.postMessage({ unknown: true }); return; }
  const headers = m.headers.filter(([k]) => k.toLowerCase() !== "cookie");
  if (svc.jar.size) headers.push(["Cookie", [...svc.jar].map(([k, v]) => `${k}=${v}`).join("; ")]);
  try {
    const r = await currentSession.http(svc.device, svc.service, m.method, m.path, JSON.stringify(headers), new Uint8Array(m.body));
    const replyHeaders = JSON.parse(r.headers());
    for (const [k, v] of replyHeaders) if (k.toLowerCase() === "set-cookie") remember(svc.jar, v);
    const body = r.body().buffer;
    port.postMessage({ status: r.status(), reason: r.reason(), headers: replyHeaders, body }, [body]);
  } catch (e) {
    port.postMessage({ error: `${svc.device}/${svc.service}：${e && e.message ? e.message : e}`, status: 502 });
  }
}
if (navigator.serviceWorker) navigator.serviceWorker.onmessage = onForward;

// 设备后面列出它上面的网页服务：每个是一个在新标签页打开的链接。
async function webServices(session, device, li) {
  let list;
  try { list = JSON.parse(await session.services(device)).filter((s) => s.proto === "http"); } catch { return; }
  if (list.length === 0) return;
  try { await ensureForwarder(); } catch (e) { return fail(e); }
  for (const s of list) {
    const token = randomToken();
    services.set(token, { device, service: s.name, jar: new Map() });
    const a = document.createElement("a");
    a.className = "svc";
    a.href = `svc/${token}/`;
    a.target = "_blank";
    a.rel = "noopener noreferrer";
    a.textContent = `${s.name} ↗`;
    a.title = `在新标签页打开 ${device}/${s.name}（这个页面要一直开着）`;
    li.append(a);
  }
}

// ---- 工作区（M8.1）：只读浏览、预览、下载。内容只在内存里。 ----

const PREVIEW_MAX = 1 << 20;
const IMAGE_TYPES = { png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif", webp: "image/webp", svg: "image/svg+xml" };

function humanSize(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1 << 20) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1 << 30) return `${(n / (1 << 20)).toFixed(1)} MB`;
  return `${(n / (1 << 30)).toFixed(1)} GB`;
}

function typeOf(name) {
  const ext = name.includes(".") ? name.split(".").pop().toLowerCase() : "";
  return IMAGE_TYPES[ext] || "application/octet-stream";
}

async function workspaces(session) {
  const list = $("ws-list");
  list.innerHTML = "";
  $("ws-none").hidden = true;
  try {
    const all = JSON.parse(await session.workspaces());
    $("ws-none").hidden = all.length > 0;
    for (const w of all) {
      const li = document.createElement("li");
      li.innerHTML = `<span class="name">${escapeHtml(w.name)}</span><span class="muted">${escapeHtml(w.device)}</span>`;
      const open = document.createElement("button");
      open.textContent = "浏览";
      open.onclick = () => browse(session, w.name, "");
      li.append(open);
      list.append(li);
    }
  } catch (e) { fail(e); }
}

async function browse(session, ws, path) {
  $("error").textContent = "";
  show("files");
  $("preview").hidden = true;
  $("files-path").textContent = `${ws}:/${path}`;
  const parent = path.includes("/") ? path.slice(0, path.lastIndexOf("/")) : "";
  $("files-back").onclick = () => (path ? browse(session, ws, parent) : devices(session));
  const list = $("file-list");
  list.innerHTML = "";
  let entries;
  try { entries = JSON.parse(await session.list(ws, path)); } catch (e) { return fail(e); }
  if (entries.length === 0) list.innerHTML = `<li class="muted">（空）</li>`;
  for (const e of entries) {
    const li = document.createElement("li");
    const full = path ? `${path}/${e.name}` : e.name;
    const a = document.createElement("a");
    a.href = "#";
    a.dataset.kind = e.kind;
    a.textContent = e.kind === "dir" ? `${e.name}/` : e.name;
    const info = document.createElement("span");
    info.className = "muted";
    info.textContent = e.kind === "file" ? humanSize(e.size) : e.kind === "symlink" ? `→ ${e.target}` : "";
    if (e.kind === "dir") a.onclick = (ev) => { ev.preventDefault(); browse(session, ws, full); };
    else if (e.kind === "file") a.onclick = (ev) => { ev.preventDefault(); preview(session, ws, full, e.name); };
    else a.onclick = (ev) => ev.preventDefault();
    li.append(a, info);
    list.append(li);
  }
}

let previewUrl = null;
async function preview(session, ws, path, name) {
  $("error").textContent = "";
  if (previewUrl) { URL.revokeObjectURL(previewUrl); previewUrl = null; }
  const [text, img, note] = [$("preview-text"), $("preview-image"), $("preview-note")];
  text.hidden = img.hidden = true;
  text.textContent = note.textContent = "";
  $("preview").hidden = false;
  $("preview-name").textContent = name;
  $("preview-size").textContent = "";
  $("preview-download").onclick = () => download(session, ws, path, name);
  status("读取中…");
  let f;
  try { f = await session.read(ws, path, PREVIEW_MAX); } catch (e) { status(""); return fail(e); }
  status("");
  const data = f.bytes();
  $("preview-size").textContent = humanSize(f.size());
  const type = typeOf(name);
  if (type.startsWith("image/")) {
    if (f.truncated()) { note.textContent = "图片太大，没有预览；可以下载。"; return; }
    previewUrl = URL.createObjectURL(new Blob([data], { type }));
    img.src = previewUrl;
    img.hidden = false;
    return;
  }
  let decoded = null;
  if (!data.includes(0)) {
    // 只读了开头时，最后一个字符可能被截断：那时不要求严格。
    try { decoded = new TextDecoder("utf-8", { fatal: !f.truncated() }).decode(data); } catch { decoded = null; }
  }
  if (decoded === null) { note.textContent = "不是文本或图片，没有预览；可以下载。"; return; }
  text.textContent = decoded;
  text.hidden = false;
  if (f.truncated()) note.textContent = `只显示了开头的 ${humanSize(data.length)}；完整的文件可以下载。`;
}

async function download(session, ws, path, name) {
  $("error").textContent = "";
  status("下载中…");
  try {
    const f = await session.read(ws, path, Number.MAX_SAFE_INTEGER);
    const url = URL.createObjectURL(new Blob([f.bytes()], { type: typeOf(name) }));
    const a = document.createElement("a");
    a.href = url;
    a.download = name;
    document.body.append(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 10000);
    status("");
  } catch (e) { status(""); fail(e); }
}

function terminal(session, device, name) {
  show("terminal");
  $("term-title").textContent = `${device} / ${name}`;
  const host = $("xterm");
  host.innerHTML = "";
  const term = new window.Terminal({ cursorBlink: true, fontSize: 14, scrollback: 2000 });
  const fit = new window.FitAddon.FitAddon();
  term.loadAddon(fit);
  term.open(host);
  fit.fit();
  const enc = new TextEncoder();
  const t = session.open(device, name, term.cols, term.rows,
    (data) => term.write(data),
    (notice) => status(notice === "reconnecting" ? "连接中断，正在重连…" : ""),
    (code, message) => {
      removeEventListener("resize", onResize);
      removeEventListener("pagehide", leave);
      term.dispose();
      status("");
      if (code !== 0 || message) fail(message || `会话已结束（退出码 ${code}）`);
      devices(session);
    });
  term.onData((d) => t.keys(enc.encode(d)));
  term.onBinary((d) => t.keys(Uint8Array.from(d, (c) => c.charCodeAt(0))));
  term.onResize(({ cols, rows }) => t.resize(cols, rows));
  const onResize = () => fit.fit();
  const leave = () => t.leave();
  addEventListener("resize", onResize);
  addEventListener("pagehide", leave);
  $("term-leave").onclick = leave;
  term.focus();
}

function escapeHtml(s) {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

// 关闭页面：注销转发程序，清掉网页服务的页面可能留下的存储（R32）。
addEventListener("pagehide", () => { currentSession = null; services.clear(); wipe(); });
await wipe();
await init();
async function openShare(link) {
  show("share-view");
  status("正在打开分享…");
  try {
    const browser = new Browser(relay);
    const guest = await browser.open_share(link);
    status("");
    const info = JSON.parse(guest.info());
    $("share-meta").textContent = `设备 ${info.device} · 编号 ${info.id} · 只读分享`;
  } catch (e) { status(""); fail(e); }
}

const hash = location.hash.slice(1);
if (hash.startsWith("enroll=")) enroll(hash);
else if (hash.startsWith("share=")) openShare(hash);
else login();
