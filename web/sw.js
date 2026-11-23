// 网页服务的转发程序（M8.2，R71）。只在登录的页面打开网页服务时注册，离开时注销。不保存任何东西。
//
// 网页服务在新标签页里打开，地址是 /svc/<令牌>/<路径>。令牌是登录的页面随机生成的，只有它知道令牌对应哪个设备和服务：
// 这里收到请求后把令牌和请求交给登录的页面（那里有网页节点核心），由它经 Nexus 发出，回应交还浏览器。
// 一个服务的页面拿不到别的服务的令牌，所以只能访问它自己。
// 服务的页面里用绝对路径（/static/app.js）发的请求，按发出它的标签页（或 Referer）归到同一个令牌。

const PREFIX = "/svc/";

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));

// 地址里的令牌和剩下的路径（含查询）。
function split(url) {
  const u = new URL(url);
  if (u.origin !== self.location.origin || !u.pathname.startsWith(PREFIX)) return null;
  const rest = u.pathname.slice(PREFIX.length);
  const slash = rest.indexOf("/");
  const token = slash < 0 ? rest : rest.slice(0, slash);
  if (!/^[0-9a-f]{32}$/.test(token)) return null;
  return { token, path: (slash < 0 ? "/" : rest.slice(slash)) + u.search };
}

async function targetOf(event) {
  const direct = split(event.request.url);
  if (direct) return direct;
  const u = new URL(event.request.url);
  if (u.origin !== self.location.origin) return null;
  // 绝对路径：看是哪个标签页发的。
  const from = event.clientId ? await self.clients.get(event.clientId) : null;
  const owner = (from && split(from.url)) || (event.request.referrer && split(event.request.referrer));
  return owner ? { token: owner.token, path: u.pathname + u.search } : null;
}

// 登录的页面：不在 /svc/ 下面的窗口。可能有几个，问到认得这个令牌的那个为止。
async function ask(message, body) {
  const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
  const apps = windows.filter((c) => c.url.startsWith(self.location.origin + "/") && !new URL(c.url).pathname.startsWith(PREFIX));
  for (const app of apps) {
    const reply = await new Promise((resolve) => {
      const ch = new MessageChannel();
      ch.port1.onmessage = (m) => resolve(m.data);
      app.postMessage({ ...message, body }, [ch.port2]);
      setTimeout(() => resolve({ unknown: true }), 60000);
    });
    if (!reply.unknown) return reply;
  }
  return { error: "登录的页面已经关闭或退出了：回到 Nexus 的页面重新登录，再打开这个服务", status: 503 };
}

function plain(status, text) {
  return new Response(text, { status, headers: { "Content-Type": "text/plain; charset=utf-8" } });
}

async function forward(event, target) {
  const req = event.request;
  const body = ["GET", "HEAD"].includes(req.method) ? new ArrayBuffer(0) : await req.arrayBuffer();
  const headers = [];
  for (const [k, v] of req.headers) {
    if (!["cookie", "host", "connection"].includes(k)) headers.push([k, v]);
  }
  const reply = await ask({ type: "nexus-http", token: target.token, method: req.method, path: target.path, headers }, body);
  if (reply.error) return plain(reply.status || 502, reply.error);
  const nullBody = [101, 204, 205, 304].includes(reply.status) || req.method === "HEAD";
  const h = new Headers();
  for (const [k, v] of reply.headers) {
    const lower = k.toLowerCase();
    if (lower === "set-cookie" || lower === "content-encoding") continue;
    if (lower === "location") { h.append(k, relocate(v, target.token)); continue; }
    try { h.append(k, v); } catch { /* 浏览器不接受的头：跳过 */ }
  }
  return new Response(nullBody ? null : reply.body, { status: reply.status, statusText: reply.reason || "", headers: h });
}

// 跳转留在这个服务里：本服务的绝对路径、http://localhost… 改写到 /svc/<令牌>/…
function relocate(location, token) {
  if (location.startsWith("/") && !location.startsWith("//")) return `${PREFIX}${token}${location}`;
  try {
    const u = new URL(location);
    if (["localhost", "127.0.0.1"].includes(u.hostname)) return `${PREFIX}${token}${u.pathname}${u.search}${u.hash}`;
  } catch { /* 相对路径：原样 */ }
  return location;
}

self.addEventListener("fetch", (event) => {
  const u = new URL(event.request.url);
  if (u.origin !== self.location.origin) return;
  // 登录的页面自己的文件：不管。只有 /svc/ 下面的地址，或来自服务页面的请求才转发。
  if (!u.pathname.startsWith(PREFIX) && !event.clientId && !event.request.referrer) return;
  event.respondWith((async () => {
    const target = await targetOf(event);
    if (!target) return fetch(event.request);
    try { return await forward(event, target); } catch (e) { return plain(502, String(e && e.message ? e.message : e)); }
  })());
});
