// M9.1 临时分享前端逻辑与渲染路径单元测试（Node 内置 test runner，零外部依赖）。
// 运行方式：node --test web/share.test.mjs
import test from "node:test";
import assert from "node:assert/strict";
import {
  classifyShareError,
  formatExpiry,
  formatRemainingUses,
  formatShareMeta,
  isSingleFileShare,
  mountShareView,
  normalizeReplyHeaders,
  parseShareTarget,
  renderStateBox,
} from "./share.mjs";

function createFakeDoc() {
  class FakeElement {
    constructor(tag) {
      this.tagName = tag.toUpperCase();
      this.className = "";
      this.dataset = {};
      this.children = [];
      this.hidden = false;
      this.disabled = false;
      this.textContent = "";
      this.href = "";
      this.src = "";
      this.title = "";
      this.target = "";
      this.rel = "";
      this.alt = "";
      this.onclick = null;
    }
    set innerHTML(v) {
      if (v === "") this.children = [];
    }
    append(...nodes) {
      for (const n of nodes) this.children.push(n);
    }
    find(predicate) {
      if (predicate(this)) return this;
      for (const c of this.children) {
        const hit = c.find(predicate);
        if (hit) return hit;
      }
      return null;
    }
    findAll(predicate, acc = []) {
      if (predicate(this)) acc.push(this);
      for (const c of this.children) c.findAll(predicate, acc);
      return acc;
    }
  }
  return {
    createElement: (tag) => new FakeElement(tag),
  };
}

function baseHelpers(overrides = {}) {
  return {
    setStatus: () => {},
    ensureForwarder: async () => {},
    registerGuestService: () => "0123456789abcdef0123456789abcdef",
    unregisterGuestService: () => {},
    createObjectURL: () => "blob:mock",
    revokeObjectURL: () => {},
    triggerDownload: () => {},
    createTerminal: () => ({
      cols: 80,
      rows: 24,
      written: [],
      write(chunk) { this.written.push(chunk); },
      fit() {},
      dispose() {},
    }),
    addResizeListener: () => {},
    removeResizeListener: () => {},
    ...overrides,
  };
}

test("parseShareTarget parses Drive, Service, Terminal, and unknown targets", () => {
  assert.deepEqual(parseShareTarget({ Drive: { ws: "work", path: "/public" } }), {
    kind: "drive",
    ws: "work",
    path: "/public",
    label: "work:/public",
    title: "文件与目录分享（只读）",
  });
  assert.deepEqual(parseShareTarget({ Service: { name: "web" } }), {
    kind: "service",
    name: "web",
    label: "web",
    title: "网页服务分享：web",
  });
  assert.deepEqual(parseShareTarget({ Terminal: { session: "main" } }), {
    kind: "terminal",
    session: "main",
    label: "main",
    title: "只读终端分享：main",
  });
  assert.equal(parseShareTarget(null).kind, "unknown");
});

test("formatRemainingUses and formatExpiry render human-readable metadata", () => {
  assert.equal(formatRemainingUses(null), "不限次数");
  assert.equal(formatRemainingUses(undefined), "不限次数");
  assert.equal(formatRemainingUses(0), "剩余 0 次（本次为最后一次）");
  assert.equal(formatRemainingUses(3), "剩余 3 次");

  const now = 1_700_000_000_000;
  const future = formatExpiry(now + 45 * 60 * 1000, now);
  assert.equal(future.expired, false);
  assert.match(future.text, /约 45 分钟后过期/);

  const past = formatExpiry(now - 1000, now);
  assert.equal(past.expired, true);
  assert.match(past.text, /已过期/);

  const meta = formatShareMeta(
    {
      id: "abcd1234",
      device: "home",
      target: { Service: { name: "web" } },
      expires_at: now + 2 * 3600 * 1000,
      remaining_uses: 1,
    },
    now,
  );
  assert.equal(meta.items.find((i) => i.label === "剩余次数").value, "剩余 1 次");
  assert.match(meta.items.find((i) => i.label === "过期时间").value, /约 2 小时后过期/);
});

test("classifyShareError distinguishes expired/revoked, forbidden, and generic errors", () => {
  assert.equal(classifyShareError(new Error("这项分享已过期")).kind, "expired");
  assert.equal(classifyShareError(new Error("这项分享已被撤销")).kind, "expired");
  assert.equal(classifyShareError(new Error("这项分享的使用次数已用完")).kind, "expired");
  assert.equal(classifyShareError(new Error("分享链接无效")).kind, "expired");
  assert.equal(classifyShareError(new Error("不能越界访问单文件分享之外的路径")).kind, "forbidden");
  assert.equal(classifyShareError(new Error("Access denied")).kind, "forbidden");
  assert.equal(classifyShareError(new Error("连不上中继：timeout")).kind, "error");
});

test("normalizeReplyHeaders accepts both array and JSON string", () => {
  assert.deepEqual(normalizeReplyHeaders([["Content-Type", "text/html"]]), [["Content-Type", "text/html"]]);
  assert.deepEqual(normalizeReplyHeaders('[["X-A","1"]]'), [["X-A", "1"]]);
  assert.deepEqual(normalizeReplyHeaders("invalid"), []);
});

test("isSingleFileShare detects single-file shares accurately", () => {
  assert.equal(isSingleFileShare("/public/hello.txt", [{ name: "hello.txt", kind: "file" }]), true);
  assert.equal(isSingleFileShare("/public", [{ name: "hello.txt", kind: "file" }]), false);
  assert.equal(isSingleFileShare("/public", [{ name: "sub", kind: "dir" }]), false);
});

test("mountShareView renders Drive directory, empty state, and file preview", async () => {
  const doc = createFakeDoc();
  const titleEl = doc.createElement("h1");
  const metaEl = doc.createElement("p");
  const bodyEl = doc.createElement("div");

  const guest = {
    async drive_list(subPath) {
      if (subPath === "") {
        return JSON.stringify([
          { name: "empty", kind: "dir", size: 0, target: null },
          { name: "hello.txt", kind: "file", size: 11, target: null },
        ]);
      }
      if (subPath === "empty") return JSON.stringify([]);
      throw new Error("不能越界访问");
    },
    async drive_read(subPath) {
      assert.equal(subPath, "hello.txt");
      return { size: 11, truncated: false, bytes: new TextEncoder().encode("hello guest") };
    },
  };

  const cleanup = await mountShareView({
    guest,
    info: {
      id: "11223344",
      device: "laptop",
      target: { Drive: { ws: "work", path: "/public" } },
      expires_at: Date.now() + 3600_000,
      remaining_uses: 2,
    },
    titleEl,
    metaEl,
    bodyEl,
    helpers: baseHelpers(),
    doc,
  });

  assert.equal(titleEl.textContent, "文件与目录分享（只读）");
  const links = bodyEl.findAll((el) => el.tagName === "A");
  assert.deepEqual(links.map((a) => a.textContent), ["empty/", "hello.txt"]);

  await links[1].onclick({ preventDefault() {} });
  const pre = bodyEl.find((el) => el.className === "share-preview-text");
  assert.equal(pre.hidden, false);
  assert.equal(pre.textContent, "hello guest");

  await links[0].onclick({ preventDefault() {} });
  const emptyBox = bodyEl.find((el) => el.dataset.state === "empty");
  assert.ok(emptyBox, "空目录应渲染 empty 状态");
  cleanup();
});

test("mountShareView renders Service share in-page iframe and cleans up token", async () => {
  const doc = createFakeDoc();
  const titleEl = doc.createElement("h1");
  const metaEl = doc.createElement("p");
  const bodyEl = doc.createElement("div");
  let unregistered = null;

  const cleanup = await mountShareView({
    guest: {},
    info: {
      id: "55667788",
      device: "home",
      target: { Service: { name: "web" } },
      expires_at: Date.now() + 3600_000,
      remaining_uses: null,
    },
    titleEl,
    metaEl,
    bodyEl,
    helpers: baseHelpers({
      registerGuestService: () => "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      unregisterGuestService: (token) => { unregistered = token; },
    }),
    doc,
  });

  const iframe = bodyEl.find((el) => el.tagName === "IFRAME");
  assert.ok(iframe, "网页服务分享应在页内渲染 iframe");
  assert.equal(iframe.src, "svc/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/");
  cleanup();
  assert.equal(unregistered, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
});

test("mountShareView renders Terminal share via watch_terminal and surfaces revocation error", async () => {
  const doc = createFakeDoc();
  const titleEl = doc.createElement("h1");
  const metaEl = doc.createElement("p");
  const bodyEl = doc.createElement("div");
  const fakeTerm = {
    cols: 80,
    rows: 24,
    written: [],
    disposed: false,
    write(chunk) { this.written.push(chunk); },
    fit() {},
    dispose() { this.disposed = true; },
  };
  let callbacks = null;
  let left = false;

  const guest = {
    watch_terminal(cols, rows, onOutput, onNotice, onEnd) {
      assert.equal(cols, 80);
      assert.equal(rows, 24);
      callbacks = { onOutput, onNotice, onEnd };
      return { leave() { left = true; } };
    },
  };

  const cleanup = await mountShareView({
    guest,
    info: {
      id: "99aabbcc",
      device: "home",
      target: { Terminal: { session: "main" } },
      expires_at: Date.now() + 3600_000,
      remaining_uses: 0,
    },
    titleEl,
    metaEl,
    bodyEl,
    helpers: baseHelpers({ createTerminal: () => fakeTerm }),
    doc,
  });

  callbacks.onOutput(new TextEncoder().encode("SHARED_OUTPUT"));
  assert.equal(fakeTerm.written.length, 1);

  callbacks.onEnd(-1, "这项分享已被撤销");
  const expiredBox = bodyEl.find((el) => el.dataset.state === "expired");
  assert.ok(expiredBox, "终端分享撤销后应显示失效状态");

  cleanup();
  assert.equal(fakeTerm.disposed, true);
  assert.equal(left, false);
});

test("renderStateBox sets semantic data-state attribute for loading/empty/expired/forbidden", () => {
  const doc = createFakeDoc();
  const container = doc.createElement("div");
  for (const kind of ["loading", "empty", "expired", "forbidden"]) {
    const box = renderStateBox(container, { kind, title: kind, detail: "detail" }, doc);
    assert.equal(box.dataset.state, kind);
  }
});
