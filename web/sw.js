// Nexus M8.2 按需注册的零存储 Service Worker：
// 不使用 CacheStorage / IndexedDB，不持有任何密钥，只将 /_svc/<device>/<service>/*
// 以及已绑定服务 iframe（按 clientId 追踪）的请求经 MessageChannel 转发给主页面 WASM Session。

const SVC_PREFIX = "/_svc/";
const NEXUS_OWN_PREFIXES = ["/pkg/", "/vendor/", "/_svc/"];
const NEXUS_OWN_EXACT = new Set(["/", "/index.html", "/app.js", "/app.css", "/sw.js"]);

// clientId -> { device, service }
const clientBindings = new Map();
let hostClientId = null;

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

self.addEventListener("message", (event) => {
  const data = event.data || {};
  if (data.type === "nexus-host-ready" && event.source && event.source.id) {
    hostClientId = event.source.id;
  } else if (data.type === "nexus-unregister") {
    hostClientId = null;
    clientBindings.clear();
    event.waitUntil(self.registration.unregister());
  }
});

function parseSvcPath(pathname) {
  if (!pathname.startsWith(SVC_PREFIX)) return null;
  const rest = pathname.slice(SVC_PREFIX.length);
  const parts = rest.split("/");
  const device = parts[0];
  const service = parts[1];
  if (!device || !service) return null;
  const sub = "/" + parts.slice(2).join("/");
  return { device: decodeURIComponent(device), service: decodeURIComponent(service), path: sub };
}

function isNexusAsset(pathname) {
  if (NEXUS_OWN_EXACT.has(pathname)) return true;
  return NEXUS_OWN_PREFIXES.some((p) => pathname.startsWith(p));
}

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  if (url.origin !== self.location.origin) return;

  let target = parseSvcPath(url.pathname);
  if (target) {
    const cid = event.resultingClientId || event.clientId;
    if (cid) clientBindings.set(cid, { device: target.device, service: target.service });
  } else if (event.clientId && clientBindings.has(event.clientId) && !isNexusAsset(url.pathname)) {
    const bound = clientBindings.get(event.clientId);
    target = { device: bound.device, service: bound.service, path: url.pathname };
  } else {
    return;
  }

  const fullPath = target.path + url.search;
  event.respondWith(forwardToHost(event.request, target.device, target.service, fullPath));
});

async function forwardToHost(request, device, service, path) {
  const host = hostClientId ? await self.clients.get(hostClientId) : null;
  if (!host) {
    return new Response("Nexus 会话已关闭，请返回主页面重新登录。", {
      status: 503,
      headers: { "Content-Type": "text/plain; charset=utf-8" },
    });
  }
  const headers = [];
  for (const [k, v] of request.headers.entries()) {
    headers.push([k, v]);
  }
  const body = request.method === "GET" || request.method === "HEAD"
    ? new Uint8Array(0)
    : new Uint8Array(await request.arrayBuffer());

  return new Promise((resolve) => {
    const channel = new MessageChannel();
    channel.port1.onmessage = (ev) => {
      const msg = ev.data || {};
      if (msg.error) {
        resolve(new Response(msg.error, {
          status: 502,
          headers: { "Content-Type": "text/plain; charset=utf-8" },
        }));
        return;
      }
      const respHeaders = new Headers();
      for (const [k, v] of msg.headers || []) {
        try { respHeaders.append(k, v); } catch (_) { /* 忽略浏览器禁止的响应头 */ }
      }
      const noBody = msg.status === 204 || msg.status === 304;
      resolve(new Response(noBody ? null : msg.body, {
        status: msg.status || 200,
        statusText: msg.status_text || "",
        headers: respHeaders,
      }));
    };
    host.postMessage(
      { type: "nexus-http-fetch", device, service, method: request.method, path, headers, body },
      [channel.port2],
    );
  });
}
