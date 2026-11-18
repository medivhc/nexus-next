#!/usr/bin/env python3
"""Drive 的衡量（M4.4）：在一台新设备上打开 nexus-next 的工作区并构建。

两台设备（home、fresh）的常驻程序各在一个网络命名空间里，中间是一条限速、有延迟的链路
（默认 100 Mbit/s、往返 20 毫秒）。fresh 上跑两个常驻程序 fresh-a、fresh-b（两台新设备）。

  1. home 导入 nexus-next 的源码（git 跟踪的文件）为工作区 work 并提交；home 是存储设备。
  2. 本地基线：在源码的普通副本里构建。
  3. fresh-a：同步、挂载（带 --record build），记下打开到可用的时间；在挂载里构建（边用边取）。
  4. fresh-b：同步、按 fresh-a 记录的清单预读，然后在挂载里构建。

构建命令默认 `cargo check --workspace --offline`。依赖来自 cargo 的缓存（不经工作区）。
为了只比较「读工作区源码」的差别，先用一个共享的目标目录把依赖构建好；之后每次构建在不同的路径上，
cargo 会重新构建工作区里的包、复用依赖。

挂载和 cargo 在命名空间外运行（经 Unix 套接字找常驻程序），因为 `ip netns exec` 里的挂载外面看不到。

需要 root、ip、tc、FUSE。条件不满足时说明原因并跳过（退出码 0），除非给出 --require。

用法：tools/drive_bench.py [--bin target/debug/nexus] [--rate-mbit 100] [--rtt-ms 20]
                           [--build "cargo check --workspace"] [--record 文件] [--require]
"""
# covers: R55
import argparse
import json
import os
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def sh(*cmd, check=True, **kw):
    return subprocess.run(cmd, check=check, capture_output=True, text=True, **kw)


class Bench:
    def __init__(self, args):
        self.args = args
        self.bin = os.path.abspath(args.bin)
        self.tmp = tempfile.mkdtemp(prefix="nexus-drive-bench-")
        tag = os.getpid() % 10000
        self.ns = {"home": f"nxdh{tag}", "fresh": f"nxdf{tag}"}
        self.ip = {"home": "10.78.0.1", "fresh": "10.78.0.2"}
        self.node_ns = {"home": "home", "fresh-a": "fresh", "fresh-b": "fresh"}
        self.procs = []
        self.mounts = []
        self.rtt_ms = args.rtt_ms

    # ---------------------------------------------------------------- 网络
    def setup_link(self):
        a, b = self.ns["home"], self.ns["fresh"]
        va, vb = f"{a}v", f"{b}v"
        sh("ip", "netns", "add", a)
        sh("ip", "netns", "add", b)
        sh("ip", "link", "add", va, "type", "veth", "peer", "name", vb)
        delay = f"{self.args.rtt_ms / 2}ms"
        for ns, dev, side in ((a, va, "home"), (b, vb, "fresh")):
            sh("ip", "link", "set", dev, "netns", ns)
            sh("ip", "-n", ns, "addr", "add", f"{self.ip[side]}/24", "dev", dev)
            sh("ip", "-n", ns, "link", "set", dev, "up")
            sh("ip", "-n", ns, "link", "set", "lo", "up")
            netem = sh("ip", "netns", "exec", ns, "tc", "qdisc", "add", "dev", dev, "root", "netem",
                       "delay", delay, "rate", f"{self.args.rate_mbit}mbit", "limit", "100000", check=False)
            if netem.returncode != 0:
                # 内核没有 netem：只限速，没有额外延迟（结果里记下来）。
                self.rtt_ms = 0
                sh("ip", "netns", "exec", ns, "tc", "qdisc", "add", "dev", dev, "root", "tbf",
                   "rate", f"{self.args.rate_mbit}mbit", "burst", "64kb", "latency", "50ms")

    def teardown(self):
        for m in self.mounts:
            sh("fusermount3", "-u", "-z", m, check=False)
        for p in self.procs:
            try:
                p.send_signal(signal.SIGKILL)
                p.wait(timeout=5)
            except Exception:
                pass
        # 命名空间里常驻程序启动的进程（远端命令、终端会话里的 shell）在常驻程序被杀后会留下来：一起杀掉。
        for ns in self.ns.values():
            pids = sh("ip", "netns", "pids", ns, check=False).stdout.split()
            for pid in pids:
                try:
                    os.kill(int(pid), signal.SIGKILL)
                except (ProcessLookupError, ValueError):
                    pass
        for ns in self.ns.values():
            sh("ip", "netns", "del", ns, check=False)
        if not self.args.keep:
            shutil.rmtree(self.tmp, ignore_errors=True)

    # ---------------------------------------------------------------- 设备
    def env(self, node):
        d = os.path.join(self.tmp, node)
        e = dict(os.environ)
        e.update(NEXUS_DATA_DIR=d, NEXUS_AGENT_SOCKET=os.path.join(d, "agent.sock"))
        return e

    def start_daemon(self, node):
        d = os.path.join(self.tmp, node)
        os.makedirs(d, exist_ok=True)
        sock = os.path.join(d, "ctl.sock")
        cmd = ["ip", "netns", "exec", self.ns[self.node_ns[node]], self.bin, "daemon", "--name", node,
               "--relay", "disabled", "--no-discovery"]
        log = open(os.path.join(d, "daemon.log"), "a")
        p = subprocess.Popen(cmd, env=self.env(node), stdout=log, stderr=log)
        self.procs.append(p)
        deadline = time.time() + 15
        while not os.path.exists(sock):
            if time.time() > deadline or p.poll() is not None:
                raise RuntimeError(f"{node} 没有就绪：\n" + open(os.path.join(d, "daemon.log")).read())
            time.sleep(0.02)

    def cli(self, node, *args, timeout=600):
        if self.args.verbose:
            print(f"  [{node}] nexus {' '.join(args)}", flush=True)
        out = subprocess.run([self.bin, *args, "--json"], env=self.env(node), capture_output=True, text=True, timeout=timeout)
        if out.returncode != 0:
            raise RuntimeError(f"[{node}] nexus {' '.join(args)} 失败（{out.returncode}）：{out.stderr}")
        return json.loads(out.stdout) if out.stdout.strip() else {}

    def build_network(self):
        self.cli("home", "init", "--name", "home")
        for node in ("fresh-a", "fresh-b"):
            code = self.cli(node, "join")["code"]
            self.cli("home", "device", "approve", code, "--name", node)
        self.cli("home", "drive", "serve", "on")

    # ---------------------------------------------------------------- 测量
    def export_source(self, dest):
        os.makedirs(dest)
        archive = subprocess.run(["git", "-C", ROOT, "archive", "HEAD"], check=True, capture_output=True).stdout
        subprocess.run(["tar", "-x", "-C", dest], input=archive, check=True)

    def build(self, cwd, label):
        env = dict(os.environ, CARGO_TARGET_DIR=os.path.join(self.tmp, "target"), CARGO_INCREMENTAL="0")
        cmd = shlex.split(self.args.build) + ["--offline", "--quiet"]
        t = time.monotonic()
        out = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True)
        secs = time.monotonic() - t
        if out.returncode != 0:
            raise RuntimeError(f"{label}：构建失败：{out.stderr[-2000:]}")
        print(f"  {label}：构建 {secs:.1f} 秒", flush=True)
        return round(secs, 2)

    def mount(self, node, record=None):
        m = os.path.join(self.tmp, f"mnt-{node}")
        os.makedirs(m)
        cmd = [self.bin, "drive", "mount", "work:/", m] + (["--record", record] if record else [])
        p = subprocess.Popen(cmd, env=self.env(node), stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        self.procs.append(p)
        self.mounts.append(m)
        while sh("mountpoint", "-q", m, check=False).returncode != 0:
            if p.poll() is not None:
                raise RuntimeError(f"挂载失败：{p.stderr.read()}")
            time.sleep(0.01)
        return m, p

    def unmount(self, m, p):
        p.send_signal(signal.SIGTERM)
        p.wait(timeout=60)
        self.mounts.remove(m)

    def open_device(self, node, record=None):
        """同步、挂载，到能列出根目录、读到 Cargo.toml 为止。"""
        t = time.monotonic()
        sync = self.cli(node, "drive", "sync", "work")
        assert sync["state"] == "pulled", sync
        t_sync = time.monotonic() - t
        m, p = self.mount(node, record)
        os.listdir(m)
        with open(os.path.join(m, "Cargo.toml"), "rb") as f:
            f.read()
        usable = time.monotonic() - t
        print(f"  {node}：同步 {t_sync:.2f} 秒，到可用 {usable:.2f} 秒", flush=True)
        return m, p, round(t_sync, 3), round(usable, 3)

    def where(self, node):
        w = self.cli(node, "drive", "where", "work:/")
        return w["local_bytes"], w["total_bytes"]

    def run(self):
        src = os.path.join(self.tmp, "src")
        self.export_source(src)
        files = sum(len(f) for _, _, f in os.walk(src))
        t = time.monotonic()
        self.cli("home", "drive", "create", "work")
        imp = self.cli("home", "drive", "import", src, "work:/")
        self.cli("home", "drive", "commit", "work", "-m", "nexus-next")
        t_import = time.monotonic() - t
        print(f"  home：导入并提交 {imp['files']} 个文件、{imp['bytes']} 字节，{t_import:.1f} 秒", flush=True)

        # 依赖先构建好（共享目标目录）；之后每次构建都在新路径上，只重新构建工作区里的包。
        print("  准备：构建依赖（不计入）", flush=True)
        self.build(src, "准备")
        local = os.path.join(self.tmp, "local")
        # 不保留修改时间：和挂载一样比依赖的产物新，cargo 才会重新构建工作区里的包。
        shutil.copytree(src, local, symlinks=True, copy_function=shutil.copy)
        t_local = self.build(local, "本地目录")

        m, p, a_sync, a_usable = self.open_device("fresh-a", record="build")
        t_a = self.build(m, "fresh-a 挂载（边用边取）")
        a_local, total = self.where("fresh-a")
        self.unmount(m, p)

        b = {}
        t = time.monotonic()
        sync = self.cli("fresh-b", "drive", "sync", "work")
        assert sync["state"] == "pulled", sync
        b["sync_s"] = round(time.monotonic() - t, 3)
        t = time.monotonic()
        pre = self.cli("fresh-b", "drive", "prefetch", "work", "--profile", "build")
        b["prefetch_s"] = round(time.monotonic() - t, 3)
        print(f"  fresh-b：预读 {pre['files']} 个文件、{pre['fetched_bytes']} 字节，{b['prefetch_s']:.2f} 秒", flush=True)
        m, p = self.mount("fresh-b")
        b["usable_s"] = round(time.monotonic() - t + b["sync_s"], 3)
        t_b = self.build(m, "fresh-b 挂载（先预读）")
        b_local, _ = self.where("fresh-b")
        self.unmount(m, p)

        return {
            "workspace": {"files": imp["files"], "bytes": imp["bytes"], "import_s": round(t_import, 2), "files_on_disk": files},
            "build": self.args.build,
            "local_s": t_local,
            "fresh_a": {"sync_s": a_sync, "usable_s": a_usable, "build_s": t_a, "local_bytes_after": a_local, "total_bytes": total},
            "fresh_b": dict(b, build_s=t_b, prefetch_files=pre["files"], prefetch_bytes=pre["fetched_bytes"], local_bytes_after=b_local),
        }


def missing_requirements():
    if os.geteuid() != 0:
        return "需要 root（建网络命名空间和限速）"
    for tool in ("ip", "tc", "fusermount3", "mountpoint", "git", "cargo"):
        if not shutil.which(tool):
            return f"没有 {tool}"
    if not os.path.exists("/dev/fuse"):
        return "没有 /dev/fuse"
    probe = sh("ip", "netns", "add", "nxprobe", check=False)
    sh("ip", "netns", "del", "nxprobe", check=False)
    if probe.returncode != 0:
        return f"不能建网络命名空间：{probe.stderr.strip()}"
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.path.join(ROOT, "target", "debug", "nexus"))
    ap.add_argument("--rate-mbit", type=float, default=100)
    ap.add_argument("--rtt-ms", type=float, default=20)
    ap.add_argument("--build", default="cargo check --workspace", help="构建命令（会加上 --offline --quiet）")
    ap.add_argument("--record", help="把结果写成 JSON")
    ap.add_argument("--require", action="store_true", help="条件不满足时失败而不是跳过")
    ap.add_argument("--keep", action="store_true", help="保留临时目录（调试用）")
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args()

    why = missing_requirements()
    if why:
        print(f"跳过 Drive 衡量：{why}")
        return 1 if args.require else 0

    bench = Bench(args)
    try:
        bench.setup_link()
        for node in ("home", "fresh-a", "fresh-b"):
            bench.start_daemon(node)
        bench.build_network()
        r = bench.run()
    finally:
        bench.teardown()

    r.update(rate_mbit=args.rate_mbit, rtt_ms=bench.rtt_ms)
    a, b = r["fresh_a"], r["fresh_b"]
    print()
    rtt = f"往返 {bench.rtt_ms:g} 毫秒" if bench.rtt_ms else "没有额外延迟（内核没有 netem）"
    print(f"链路 {args.rate_mbit:g} Mbit/s、{rtt}；工作区 {r['workspace']['files']} 个文件、"
          f"{r['workspace']['bytes'] / 1e6:.1f} MB；构建：{args.build}（依赖已构建好）")
    print(f"{'':<22}{'到可用':>10}{'构建':>10}{'相对本地':>10}{'取到内容':>12}")
    print(f"{'本地目录':<22}{'-':>10}{r['local_s']:>9.1f}s{1:>10.2f}{'-':>12}")
    print(f"{'新设备（边用边取）':<18}{a['usable_s']:>9.2f}s{a['build_s']:>9.1f}s{a['build_s'] / r['local_s']:>10.2f}"
          f"{a['local_bytes_after'] / 1e6:>10.1f}MB")
    print(f"{'新设备（先预读）':<19}{b['usable_s']:>9.2f}s{b['build_s']:>9.1f}s{b['build_s'] / r['local_s']:>10.2f}"
          f"{b['prefetch_bytes'] / 1e6:>10.1f}MB")
    if args.record:
        with open(args.record, "w", encoding="utf-8") as f:
            json.dump(r, f, ensure_ascii=False, indent=2)
    return 0


if __name__ == "__main__":
    sys.exit(main())
