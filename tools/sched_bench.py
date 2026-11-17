#!/usr/bin/env python3
"""流量调度的满载测量（M3.5）。

两台设备各在一个网络命名空间里，中间是一条限速的链路（tbf：默认 8 Mbit/s，队列约半秒，
就像家里上行带宽小、路由器缓冲大的宽带）。home 发布一个一直写数据的服务，laptop 经统一入口
不停地下载它，占满链路；同时反复执行 `nexus status home`（一次交互请求），测它的耗时。

分别在开启和关闭流量调度（`--no-scheduling`）时各测一遍，打印对比；开启时检查：
  - 满载时交互请求耗时的 p95 比空载时多出的不超过 --max-added-ms（默认 100 毫秒）
  - 下载吞吐不低于链路速率的一半（调度不是靠少发数据换来的）

需要 root、ip、tc。条件不满足时说明原因并跳过（退出码 0），除非给出 --require。

用法：tools/sched_bench.py [--bin target/debug/nexus] [--rate-mbit 8] [--record 文件] [--require]
"""
# covers: R44
import argparse
import json
import os
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

SOURCE = r"""
import socket, sys
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", int(sys.argv[1]))); s.listen(8)
chunk = b"\0" * 65536
import threading
def serve(c):
    try:
        while True: c.sendall(chunk)
    except OSError: pass
while True:
    c, _ = s.accept(); threading.Thread(target=serve, args=(c,), daemon=True).start()
"""

# 经 SOCKS5 连到 bulk.home.internal，读 argv[2] 秒，打印读到的字节数。
DOWNLOAD = r"""
import socket, struct, sys, time
s = socket.create_connection(("127.0.0.1", int(sys.argv[1])))
s.sendall(b"\x05\x01\x00"); assert s.recv(2) == b"\x05\x00"
host = b"bulk.home.internal"
s.sendall(b"\x05\x01\x00\x03" + bytes([len(host)]) + host + struct.pack(">H", 80))
reply = s.recv(10); assert reply[1] == 0, reply
end = time.time() + float(sys.argv[2]); total = 0; first = None
s.settimeout(1)
while time.time() < end:
    try: n = len(s.recv(1 << 16))
    except socket.timeout: continue
    if n == 0: break
    total += n
print(total)
"""


def sh(*cmd, check=True, **kw):
    return subprocess.run(cmd, check=check, capture_output=True, text=True, **kw)


class Bench:
    def __init__(self, args):
        self.args = args
        self.bin = os.path.abspath(args.bin)
        self.tmp = tempfile.mkdtemp(prefix="nexus-sched-")
        tag = os.getpid() % 10000
        self.ns = {"laptop": f"nxsa{tag}", "home": f"nxsb{tag}"}
        self.ip = {"laptop": "10.77.0.1", "home": "10.77.0.2"}
        self.procs = []
        self.daemons = {}

    # ---------------------------------------------------------------- 网络
    def setup_link(self):
        a, b = self.ns["laptop"], self.ns["home"]
        va, vb = f"{a}v", f"{b}v"
        sh("ip", "netns", "add", a)
        sh("ip", "netns", "add", b)
        sh("ip", "link", "add", va, "type", "veth", "peer", "name", vb)
        for ns, dev, node in ((a, va, "laptop"), (b, vb, "home")):
            sh("ip", "link", "set", dev, "netns", ns)
            sh("ip", "-n", ns, "addr", "add", f"{self.ip[node]}/24", "dev", dev)
            sh("ip", "-n", ns, "link", "set", dev, "up")
            sh("ip", "-n", ns, "link", "set", "lo", "up")
            # 限速，队列约半秒：瓶颈处的缓冲很大。
            limit = int(self.args.rate_mbit * 1e6 / 8 * 0.5)
            sh("ip", "netns", "exec", ns, "tc", "qdisc", "add", "dev", dev, "root", "tbf",
               "rate", f"{self.args.rate_mbit}mbit", "burst", "32kb", "limit", str(limit))

    def teardown(self):
        for p in self.procs:
            try:
                p.send_signal(signal.SIGKILL)
                p.wait(timeout=5)
            except Exception:
                pass
        for ns in self.ns.values():
            sh("ip", "netns", "del", ns, check=False)
        shutil.rmtree(self.tmp, ignore_errors=True)

    # ---------------------------------------------------------------- 节点
    def env(self, node):
        d = os.path.join(self.tmp, node)
        os.makedirs(os.path.join(d, "home"), exist_ok=True)
        e = dict(os.environ)
        e.update(NEXUS_DATA_DIR=d, NEXUS_AGENT_SOCKET=os.path.join(d, "agent.sock"), HOME=os.path.join(d, "home"))
        return e

    def start_daemon(self, node, scheduling):
        d = os.path.join(self.tmp, node)
        os.makedirs(d, exist_ok=True)
        sock = os.path.join(d, "ctl.sock")
        if os.path.exists(sock):
            os.remove(sock)
        cmd = ["ip", "netns", "exec", self.ns[node], self.bin, "daemon", "--name", node,
               "--relay", "disabled", "--no-discovery"]
        if node == "laptop":
            cmd += ["--proxy", "127.0.0.1:1080"]
        if not scheduling:
            cmd.append("--no-scheduling")
        log = open(os.path.join(d, "daemon.log"), "a")
        p = subprocess.Popen(cmd, env=self.env(node), stdout=log, stderr=log)
        self.procs.append(p)
        self.daemons[node] = p
        deadline = time.time() + 15
        while not os.path.exists(sock):
            if time.time() > deadline or p.poll() is not None:
                raise RuntimeError(f"{node} 没有就绪：\n" + open(os.path.join(d, "daemon.log")).read())
            time.sleep(0.02)

    def stop_daemon(self, node):
        p = self.daemons.pop(node)
        p.send_signal(signal.SIGKILL)
        p.wait()

    def cli(self, node, *args):
        if self.args.verbose and args[0] != "status":
            print(f"  [{node}] nexus {' '.join(args)}", flush=True)
        out = subprocess.run([self.bin, *args, "--json"], env=self.env(node), capture_output=True, text=True, timeout=60)
        if out.returncode != 0:
            raise RuntimeError(f"nexus {' '.join(args)} 失败（{out.returncode}）：{out.stderr}")
        return json.loads(out.stdout) if out.stdout.strip() else {}

    def in_ns(self, node, code, *args, **kw):
        p = subprocess.Popen(["ip", "netns", "exec", self.ns[node], sys.executable, "-c", code, *map(str, args)],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, **kw)
        self.procs.append(p)
        return p

    def build_network(self):
        self.cli("home", "init", "--name", "home")
        code = self.cli("laptop", "join")["code"]
        self.cli("home", "device", "approve", code, "--name", "laptop")
        self.in_ns("home", SOURCE, 9100)
        time.sleep(0.3)
        self.cli("home", "publish", "bulk", "--port", "9100")

    # ---------------------------------------------------------------- 测量
    def probe(self):
        t = time.monotonic()
        self.cli("laptop", "status", "home")
        ms = (time.monotonic() - t) * 1000
        if self.args.verbose:
            print(f"    {ms:.0f} 毫秒", flush=True)
        return ms

    def probes(self, n, gap):
        out = []
        for _ in range(n):
            out.append(self.probe())
            time.sleep(gap)
        return out

    def run(self, scheduling):
        # 等连接建好、各自的名册一致。
        for _ in range(3):
            self.probe()
        idle = self.probes(15, 0.1)
        seconds = self.args.seconds
        dl = self.in_ns("laptop", DOWNLOAD, 1080, seconds)
        time.sleep(3)  # 让下载先占满链路
        loaded = self.probes(int((seconds - 5) / 0.25), 0.15)
        out, err = dl.communicate(timeout=seconds + 30)
        if dl.returncode != 0:
            raise RuntimeError(f"下载失败：{err}")
        mbit = int(out.strip()) * 8 / seconds / 1e6
        self.stop_daemon("laptop")
        self.stop_daemon("home")
        return {"scheduling": scheduling, "idle": stats(idle), "loaded": stats(loaded), "throughput_mbit": round(mbit, 2)}


def stats(xs):
    xs = sorted(xs)
    return {
        "n": len(xs),
        "p50_ms": round(statistics.median(xs), 1),
        "p95_ms": round(xs[min(len(xs) - 1, int(len(xs) * 0.95))], 1),
        "max_ms": round(xs[-1], 1),
    }


def missing_requirements():
    if os.geteuid() != 0:
        return "需要 root（建网络命名空间和限速）"
    for tool in ("ip", "tc"):
        if not shutil.which(tool):
            return f"没有 {tool}（iproute2）"
    probe = sh("ip", "netns", "add", "nxprobe", check=False)
    sh("ip", "netns", "del", "nxprobe", check=False)
    if probe.returncode != 0:
        return f"不能建网络命名空间：{probe.stderr.strip()}"
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.path.join(ROOT, "target", "debug", "nexus"))
    ap.add_argument("--rate-mbit", type=float, default=8)
    ap.add_argument("--seconds", type=float, default=15)
    ap.add_argument("--max-added-ms", type=float, default=100)
    ap.add_argument("--record", help="把结果写成 JSON")
    ap.add_argument("--require", action="store_true", help="条件不满足时失败而不是跳过")
    ap.add_argument("--verbose", action="store_true")
    ap.add_argument("--only-scheduled", action="store_true", help="只测开启调度的情况")
    args = ap.parse_args()

    why = missing_requirements()
    if why:
        print(f"跳过满载测量：{why}")
        return 1 if args.require else 0

    results = []
    for scheduling in ([True] if args.only_scheduled else [True, False]):
        bench = Bench(args)
        try:
            bench.setup_link()
            # 每一轮从头建网络（重启后端口会变，没有地址发现时彼此找不到）。
            bench.start_daemon("home", scheduling)
            bench.start_daemon("laptop", scheduling)
            bench.build_network()
            results.append(bench.run(scheduling))
        finally:
            bench.teardown()

    print(f"链路 {args.rate_mbit} Mbit/s，瓶颈队列约 500 毫秒；交互请求为 nexus status home（含命令行启动）")
    print(f"{'调度':<6}{'空载 p50':>10}{'空载 p95':>10}{'满载 p50':>10}{'满载 p95':>10}{'满载最大':>10}{'吞吐 Mbit/s':>14}")
    for r in results:
        print(f"{'开' if r['scheduling'] else '关':<6}{r['idle']['p50_ms']:>10}{r['idle']['p95_ms']:>10}"
              f"{r['loaded']['p50_ms']:>10}{r['loaded']['p95_ms']:>10}{r['loaded']['max_ms']:>10}{r['throughput_mbit']:>14}")
    record = {"rate_mbit": args.rate_mbit, "seconds": args.seconds, "results": results}
    if args.record:
        with open(args.record, "w", encoding="utf-8") as f:
            json.dump(record, f, ensure_ascii=False, indent=2)

    on = next(r for r in results if r["scheduling"])
    added = on["loaded"]["p95_ms"] - on["idle"]["p95_ms"]
    failures = []
    if added > args.max_added_ms:
        failures.append(f"满载时交互请求的 p95 多了 {added:.0f} 毫秒，上限 {args.max_added_ms:.0f}")
    if on["throughput_mbit"] < args.rate_mbit * 0.5:
        failures.append(f"下载只有 {on['throughput_mbit']} Mbit/s，不到链路速率的一半")
    for f in failures:
        print("失败：" + f)
    if not failures:
        print(f"通过：满载时交互请求的 p95 多了 {added:.0f} 毫秒（上限 {args.max_added_ms:.0f}）")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
