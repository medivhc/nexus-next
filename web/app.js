// 网页节点的页面：通行密钥（浏览器的 WebAuthn）+ 网页节点核心（WebAssembly）+ 终端显示（xterm.js）。
// 不使用任何浏览器存储：临时身份和登录凭证只在内存里，关掉页面即消失。
import init, { Browser } from "./pkg/nexus_web.js";

const $ = (id) => document.getElementById(id);
const show = (id) => { for (const s of ["enroll", "login", "devices", "terminal"]) $(s).hidden = s !== id; };
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
  }
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

await init();
const hash = location.hash.slice(1);
if (hash.startsWith("enroll=")) enroll(hash); else login();
