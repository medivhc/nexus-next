// M9.1 临时分享前端逻辑与视图渲染（纯 ES 模块，可直接在 Node 里单测）。

export const SHARE_PREVIEW_MAX = 1 << 20;

const IMAGE_TYPES = {
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  webp: "image/webp",
  svg: "image/svg+xml",
};

export function humanSize(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1 << 20) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1 << 30) return `${(n / (1 << 20)).toFixed(1)} MB`;
  return `${(n / (1 << 30)).toFixed(1)} GB`;
}

export function mimeTypeOf(name) {
  const ext = name.includes(".") ? name.split(".").pop().toLowerCase() : "";
  return IMAGE_TYPES[ext] || "application/octet-stream";
}

export function normalizeReplyHeaders(raw) {
  if (Array.isArray(raw)) return raw;
  if (typeof raw === "string") {
    try {
      const parsed = JSON.parse(raw);
      return Array.isArray(parsed) ? parsed : [];
    } catch {
      return [];
    }
  }
  return [];
}

export function parseShareTarget(target) {
  if (target && typeof target === "object") {
    if (target.Drive) {
      const ws = String(target.Drive.ws || "");
      const path = String(target.Drive.path || "/");
      return { kind: "drive", ws, path, label: `${ws}:${path}`, title: "文件与目录分享（只读）" };
    }
    if (target.Service) {
      const name = String(target.Service.name || "");
      return { kind: "service", name, label: name, title: `网页服务分享：${name}` };
    }
    if (target.Terminal) {
      const session = String(target.Terminal.session || "");
      return { kind: "terminal", session, label: session, title: `只读终端分享：${session}` };
    }
  }
  return { kind: "unknown", label: "未知目标", title: "临时分享（只读）" };
}

export function formatRemainingUses(remaining) {
  if (remaining === null || remaining === undefined) return "不限次数";
  const count = Number(remaining);
  if (!Number.isFinite(count) || count <= 0) return "剩余 0 次（本次为最后一次）";
  return `剩余 ${count} 次`;
}

export function formatExpiry(expiresAtMs, nowMs = Date.now()) {
  const ts = Number(expiresAtMs);
  if (!Number.isFinite(ts) || ts <= 0) return { text: "未知", expired: false };
  const dateText = new Date(ts).toLocaleString();
  const diff = ts - nowMs;
  if (diff <= 0) return { text: `${dateText}（已过期）`, expired: true };
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return { text: `${dateText}（不足 1 分钟后过期）`, expired: false };
  if (mins < 60) return { text: `${dateText}（约 ${mins} 分钟后过期）`, expired: false };
  const hours = Math.floor(mins / 60);
  if (hours < 24) return { text: `${dateText}（约 ${hours} 小时后过期）`, expired: false };
  const days = Math.floor(hours / 24);
  return { text: `${dateText}（约 ${days} 天后过期）`, expired: false };
}

export function formatShareMeta(info, nowMs = Date.now()) {
  const target = parseShareTarget(info && info.target);
  const expiry = formatExpiry(info && info.expires_at, nowMs);
  const usesText = formatRemainingUses(info && info.remaining_uses);
  const device = info && info.device ? String(info.device) : "未知设备";
  const id = info && info.id ? String(info.id) : "-";
  return {
    target,
    title: target.title,
    expired: expiry.expired,
    items: [
      { label: "目标", value: target.label },
      { label: "设备", value: device },
      { label: "编号", value: id },
      { label: "剩余次数", value: usesText },
      { label: "过期时间", value: expiry.text },
    ],
  };
}

export function classifyShareError(err) {
  const message = String(err && err.message ? err.message : err || "未知错误");
  const lower = message.toLowerCase();
  if (
    /已过期|已被撤销|已撤销|使用次数已用完|分享链接无效|分享链接里缺少|分享码格式不对|设备标识格式不对|分享的路径已不存在|已取消发布|expired|revoked/.test(
      message,
    ) ||
    lower.includes("expired") ||
    lower.includes("revoked")
  ) {
    return { kind: "expired", title: "分享已失效或过期", detail: message };
  }
  if (/不能越界|无权限|没有权限|权限|拒绝/.test(message) || lower.includes("denied") || lower.includes("forbidden")) {
    return { kind: "forbidden", title: "无权限访问", detail: message };
  }
  return { kind: "error", title: "无法访问分享内容", detail: message };
}

export function isSingleFileShare(targetPath, entries) {
  if (!Array.isArray(entries) || entries.length !== 1 || entries[0].kind !== "file") return false;
  const parts = String(targetPath || "").split("/").filter(Boolean);
  return parts.length > 0 && parts[parts.length - 1] === entries[0].name;
}

export function renderStateBox(container, { kind, title, detail = "" }, doc = document) {
  container.innerHTML = "";
  const box = doc.createElement("div");
  box.className = `share-state share-state-${kind}`;
  box.dataset.state = kind;
  const heading = doc.createElement("p");
  heading.className = "share-state-title";
  heading.textContent = title;
  box.append(heading);
  if (detail) {
    const p = doc.createElement("p");
    p.className = "share-state-detail muted";
    p.textContent = detail;
    box.append(p);
  }
  container.append(box);
  return box;
}

export function renderShareMeta(metaEl, info, nowMs = Date.now(), doc = document) {
  const meta = formatShareMeta(info, nowMs);
  metaEl.innerHTML = "";
  const wrap = doc.createElement("div");
  wrap.className = "share-meta-list";
  for (const item of meta.items) {
    const pill = doc.createElement("span");
    pill.className = "share-pill";
    const k = doc.createElement("span");
    k.className = "share-pill-label";
    k.textContent = `${item.label}：`;
    const v = doc.createElement("span");
    v.className = "share-pill-value";
    v.textContent = item.value;
    pill.append(k, v);
    wrap.append(pill);
  }
  metaEl.append(wrap);
  return meta;
}

export async function mountShareView({ guest, info, titleEl, metaEl, bodyEl, helpers, doc = document }) {
  const meta = renderShareMeta(metaEl, info, Date.now(), doc);
  titleEl.textContent = meta.title;
  if (meta.target.kind === "drive") {
    return mountDriveShare({ guest, target: meta.target, bodyEl, helpers, doc });
  }
  if (meta.target.kind === "service") {
    return mountServiceShare({ guest, target: meta.target, bodyEl, helpers, doc });
  }
  if (meta.target.kind === "terminal") {
    return mountTerminalShare({ guest, target: meta.target, bodyEl, helpers, doc });
  }
  renderStateBox(bodyEl, { kind: "error", title: "不支持的分享类型", detail: "当前页面无法识别这个分享目标。" }, doc);
  return () => {};
}

async function mountDriveShare({ guest, target, bodyEl, helpers, doc }) {
  let previewUrl = null;
  const revokePreview = () => {
    if (previewUrl && helpers.revokeObjectURL) {
      helpers.revokeObjectURL(previewUrl);
      previewUrl = null;
    }
  };

  bodyEl.innerHTML = "";
  const bar = doc.createElement("div");
  bar.className = "share-toolbar";
  const backBtn = doc.createElement("button");
  backBtn.className = "share-btn-secondary";
  backBtn.textContent = "返回上级";
  backBtn.hidden = true;
  const pathEl = doc.createElement("span");
  pathEl.className = "share-path";
  const refreshBtn = doc.createElement("button");
  refreshBtn.className = "share-btn-secondary";
  refreshBtn.textContent = "刷新";
  bar.append(backBtn, pathEl, refreshBtn);

  const listWrap = doc.createElement("div");
  listWrap.className = "share-list-wrap";

  const previewWrap = doc.createElement("div");
  previewWrap.className = "share-preview";
  previewWrap.hidden = true;
  const previewBar = doc.createElement("div");
  previewBar.className = "share-toolbar";
  const previewName = doc.createElement("span");
  previewName.className = "share-preview-name";
  const previewSize = doc.createElement("span");
  previewSize.className = "muted";
  const downloadBtn = doc.createElement("button");
  downloadBtn.className = "share-btn-secondary";
  downloadBtn.textContent = "下载";
  previewBar.append(previewName, previewSize, downloadBtn);
  const previewNote = doc.createElement("p");
  previewNote.className = "muted share-preview-note";
  const previewText = doc.createElement("pre");
  previewText.className = "share-preview-text";
  previewText.hidden = true;
  const previewImg = doc.createElement("img");
  previewImg.className = "share-preview-image";
  previewImg.alt = "";
  previewImg.hidden = true;
  previewWrap.append(previewBar, previewNote, previewText, previewImg);

  bodyEl.append(bar, listWrap, previewWrap);

  let currentPath = "";

  async function downloadFile(subPath, name) {
    helpers.setStatus("下载中…");
    try {
      const f = await guest.drive_read(subPath, Number.MAX_SAFE_INTEGER);
      helpers.triggerDownload(f.bytes, mimeTypeOf(name), name);
      helpers.setStatus("");
    } catch (e) {
      helpers.setStatus("");
      const classified = classifyShareError(e);
      renderStateBox(listWrap, classified, doc);
      previewWrap.hidden = true;
    }
  }

  async function openPreview(subPath, name) {
    revokePreview();
    previewWrap.hidden = false;
    previewName.textContent = name;
    previewSize.textContent = "";
    previewText.hidden = true;
    previewImg.hidden = true;
    previewText.textContent = "";
    previewNote.textContent = "正在读取文件预览…";
    downloadBtn.onclick = () => downloadFile(subPath, name);
    helpers.setStatus("读取中…");
    let f;
    try {
      f = await guest.drive_read(subPath, SHARE_PREVIEW_MAX);
    } catch (e) {
      helpers.setStatus("");
      previewWrap.hidden = true;
      renderStateBox(listWrap, classifyShareError(e), doc);
      return;
    }
    helpers.setStatus("");
    const data = f.bytes;
    previewSize.textContent = humanSize(f.size);
    previewNote.textContent = "";
    const mime = mimeTypeOf(name);
    if (mime.startsWith("image/")) {
      if (f.truncated) {
        previewNote.textContent = "图片太大，无法在线预览，可点击下载查看。";
        return;
      }
      previewUrl = helpers.createObjectURL(data, mime);
      previewImg.src = previewUrl;
      previewImg.hidden = false;
      return;
    }
    let decoded = null;
    if (!data.includes(0)) {
      try {
        decoded = new TextDecoder("utf-8", { fatal: !f.truncated }).decode(data);
      } catch {
        decoded = null;
      }
    }
    if (decoded === null) {
      previewNote.textContent = "该文件不是文本或常见图片，无法预览，可点击下载。";
      return;
    }
    previewText.textContent = decoded;
    previewText.hidden = false;
    if (f.truncated) {
      previewNote.textContent = `仅显示前 ${humanSize(data.length)}，点击下载可获取完整文件。`;
    }
  }

  async function loadDir(subPath) {
    currentPath = subPath;
    revokePreview();
    previewWrap.hidden = true;
    const base = target.path.endsWith("/") ? target.path.slice(0, -1) : target.path;
    pathEl.textContent = subPath ? `${target.ws}:${base}/${subPath}` : target.label;
    const parent = subPath.includes("/") ? subPath.slice(0, subPath.lastIndexOf("/")) : "";
    backBtn.hidden = !subPath;
    backBtn.onclick = () => loadDir(parent);
    refreshBtn.onclick = () => loadDir(currentPath);

    renderStateBox(listWrap, { kind: "loading", title: "加载目录中…", detail: "正在获取只读文件列表。" }, doc);
    let entries;
    try {
      entries = JSON.parse(await guest.drive_list(subPath));
    } catch (e) {
      renderStateBox(listWrap, classifyShareError(e), doc);
      return;
    }
    if (!entries.length) {
      renderStateBox(listWrap, { kind: "empty", title: "空目录", detail: "这个分享目录下暂时没有文件或子目录。" }, doc);
      return;
    }
    listWrap.innerHTML = "";
    const ul = doc.createElement("ul");
    ul.className = "share-file-list";
    for (const entry of entries) {
      const li = doc.createElement("li");
      const full = subPath ? `${subPath}/${entry.name}` : entry.name;
      const a = doc.createElement("a");
      a.href = "#";
      a.dataset.kind = entry.kind;
      a.textContent = entry.kind === "dir" ? `${entry.name}/` : entry.name;
      const meta = doc.createElement("span");
      meta.className = "muted";
      meta.textContent =
        entry.kind === "file" ? humanSize(entry.size) : entry.kind === "symlink" ? `→ ${entry.target || ""}` : "目录";
      if (entry.kind === "dir") {
        a.onclick = (ev) => {
          ev.preventDefault();
          loadDir(full);
        };
      } else if (entry.kind === "file") {
        a.onclick = (ev) => {
          ev.preventDefault();
          openPreview(full, entry.name);
        };
      } else {
        a.onclick = (ev) => ev.preventDefault();
      }
      li.append(a, meta);
      ul.append(li);
    }
    listWrap.append(ul);

    if (!subPath && isSingleFileShare(target.path, entries)) {
      await openPreview(entries[0].name, entries[0].name);
    }
  }

  await loadDir("");
  return () => revokePreview();
}

async function mountServiceShare({ guest, target, bodyEl, helpers, doc }) {
  renderStateBox(bodyEl, { kind: "loading", title: "正在准备网页服务…", detail: `正在初始化 ${target.name} 的转发通道。` }, doc);
  let token;
  try {
    await helpers.ensureForwarder();
    token = helpers.registerGuestService(guest, target.name);
  } catch (e) {
    renderStateBox(bodyEl, classifyShareError(e), doc);
    return () => {};
  }

  const serviceUrl = `svc/${token}/`;
  bodyEl.innerHTML = "";
  const bar = doc.createElement("div");
  bar.className = "share-toolbar";
  const label = doc.createElement("span");
  label.className = "share-path";
  label.textContent = `网页服务：${target.name}`;
  const reloadBtn = doc.createElement("button");
  reloadBtn.className = "share-btn-secondary";
  reloadBtn.textContent = "重新加载";
  const openTab = doc.createElement("a");
  openTab.className = "svc share-open-tab";
  openTab.href = serviceUrl;
  openTab.target = "_blank";
  openTab.rel = "noopener noreferrer";
  openTab.textContent = "新标签页打开 ↗";
  bar.append(label, reloadBtn, openTab);

  const frame = doc.createElement("iframe");
  frame.className = "share-service-frame";
  frame.src = serviceUrl;
  frame.title = `网页服务：${target.name}`;
  reloadBtn.onclick = () => {
    frame.src = serviceUrl;
  };
  bodyEl.append(bar, frame);

  return () => {
    helpers.unregisterGuestService(token);
  };
}

function mountTerminalShare({ guest, target, bodyEl, helpers, doc }) {
  bodyEl.innerHTML = "";
  const wrap = doc.createElement("div");
  wrap.className = "share-terminal-wrap";

  const bar = doc.createElement("div");
  bar.className = "share-toolbar";
  const label = doc.createElement("span");
  label.className = "share-path";
  label.textContent = `终端会话：${target.session}`;
  const badge = doc.createElement("span");
  badge.className = "share-badge";
  badge.textContent = "只读观看（按键输入已禁用）";
  const stateEl = doc.createElement("span");
  stateEl.className = "muted share-term-state";
  stateEl.textContent = "连接中…";
  const leaveBtn = doc.createElement("button");
  leaveBtn.className = "share-btn-secondary";
  leaveBtn.textContent = "断开";
  bar.append(label, badge, stateEl, leaveBtn);

  const host = doc.createElement("div");
  host.className = "share-xterm";
  const noticeBox = doc.createElement("div");
  noticeBox.className = "share-term-notice";
  noticeBox.hidden = true;

  wrap.append(bar, noticeBox, host);
  bodyEl.append(wrap);

  const term = helpers.createTerminal(host);
  let closed = false;
  let handle = null;

  const cleanup = () => {
    if (closed) return;
    closed = true;
    helpers.removeResizeListener(onResize);
    if (handle) {
      try {
        handle.leave();
      } catch {
        // 已断开
      }
      handle = null;
    }
  };

  const onResize = () => term.fit();
  helpers.addResizeListener(onResize);

  let receivedOutput = false;
  handle = guest.watch_terminal(
    term.cols,
    term.rows,
    (data) => {
      if (!receivedOutput) {
        receivedOutput = true;
        stateEl.textContent = "已连接（只读）";
      }
      term.write(data);
    },
    (notice) => {
      stateEl.textContent = notice === "reconnecting" ? "连接中断，正在重连…" : "已连接（只读）";
    },
    (code, message) => {
      helpers.removeResizeListener(onResize);
      handle = null;
      closed = true;
      leaveBtn.disabled = true;
      if (code === -1 && message) {
        stateEl.textContent = "连接已断开";
        noticeBox.hidden = false;
        renderStateBox(noticeBox, classifyShareError(message), doc);
      } else if (code !== 0) {
        stateEl.textContent = `会话已结束（退出码 ${code}）`;
      } else {
        stateEl.textContent = "已断开观看";
      }
    },
  );

  leaveBtn.onclick = () => {
    stateEl.textContent = "已断开观看";
    leaveBtn.disabled = true;
    cleanup();
  };

  return () => {
    cleanup();
    term.dispose();
  };
}
