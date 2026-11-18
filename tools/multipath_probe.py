#!/usr/bin/env python3
"""M5.0 技术验证：两条链路时，现在的传输怎么用它们、断一条时会话停多久。

laptop 和 home 的常驻程序各在一个网络命名空间里，中间有两条独立的链路（link1、link2，不同网段，
可以分别限速）。laptop 上执行 `nexus exec home -- sh -c '每 20 毫秒打印一行'`，记下每行到达的时间；
同时看两条链路上的字节数，判断流量走哪条。然后用 iptables 丢弃正在用的那条链路上的所有包（接口不 down，
就像信号没了），记下输出中断了多久、会话还在不在；恢复后再断另一条。

不是产品代码；结果记在 docs/feature-points/M5.0.md。需要 root、ip、tc、iptables。

用法：tools/multipath_probe.py [--bin target/debug/nexus] [--rate1 0] [--rate2 0]（Mbit/s，0 表示不限速）
"""
import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TICK = "while :; do date +%s.%N; sleep 0.02; done"


def sh(*cmd, check=True):
    return subprocess.run(cmd, check=check, capture_output=True, text=True)


class Probe:
    def __init__(self, args):
        self.args = args
        self.bin = os.path.abspath(args.bin)
        self.tmp = tempfile.mkdtemp(prefix="nexus-mp-")
        tag = os.getpid() % 10000
        self.ns = {"laptop": f"nxma{tag}", "home": f"nxmb{tag}"}
        self.links = {"link1": ("10.81.1", f"m1a{tag}", f"m1b{tag}"), "link2": ("10.81.2", f"m2a{tag}", f"m2b{tag}")}
        self.procs = []

    def setup(self):
        a, b = self.ns["laptop"], self.ns["home"]
        sh("ip", "netns", "add", a)
        sh("ip", "netns", "add", b)
        for name, (net, va, vb) in self.links.items():
            sh("ip", "link", "add", va, "type", "veth", "peer", "name", vb)
            for ns, dev, host in ((a, va, 1), (b, vb, 2)):
                sh("ip", "link", "set", dev, "netns", ns)
                sh("ip", "-n", ns, "addr", "add", f"{net}.{host}/24", "dev", dev)
                sh("ip", "-n", ns, "link", "set", dev, "up")
                rate = self.args.rate1 if name == "link1" else self.args.rate2
                if rate:
                    sh("ip", "netns", "exec", ns, "tc", "qdisc", "add", "dev", dev, "root", "tbf",
                       "rate", f"{rate}mbit", "burst", "32kb", "latency", "100ms")
        for ns in (a, b):
            sh("ip", "-n", ns, "link", "set", "lo", "up")

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

    def env(self, node):
        d = os.path.join(self.tmp, node)
        return dict(os.environ, NEXUS_DATA_DIR=d, NEXUS_AGENT_SOCKET=os.path.join(d, "agent.sock"))

    def start(self, node):
        d = os.path.join(self.tmp, node)
        os.makedirs(d, exist_ok=True)
        log = open(os.path.join(d, "daemon.log"), "a")
        p = subprocess.Popen(["ip", "netns", "exec", self.ns[node], self.bin, "daemon", "--name", node,
                              "--relay", "disabled", "--no-discovery"], env=self.env(node), stdout=log, stderr=log)
        self.procs.append(p)
        sock = os.path.join(d, "ctl.sock")
        deadline = time.time() + 15
        while not os.path.exists(sock):
            if time.time() > deadline or p.poll() is not None:
                raise RuntimeError(open(os.path.join(d, "daemon.log")).read())
            time.sleep(0.02)

    def cli(self, node, *args):
        out = subprocess.run([self.bin, *args, "--json"], env=self.env(node), capture_output=True, text=True, timeout=60)
        if out.returncode != 0:
            raise RuntimeError(f"nexus {' '.join(args)}：{out.stderr}")
        return json.loads(out.stdout) if out.stdout.strip() else {}

    def bytes(self, link):
        _, va, _ = self.links[link]
        st = sh("ip", "netns", "exec", self.ns["laptop"], "cat", f"/sys/class/net/{va}/statistics/rx_bytes",
                f"/sys/class/net/{va}/statistics/tx_bytes").stdout.split()
        return int(st[0]) + int(st[1])

    def cut(self, link, on):
        _, va, _ = self.links[link]
        op = "-A" if on else "-D"
        for chain, flag in (("INPUT", "-i"), ("OUTPUT", "-o")):
            sh("ip", "netns", "exec", self.ns["laptop"], "iptables", op, chain, flag, va, "-j", "DROP")

    def active(self, secs=1.0):
        b0 = {l: self.bytes(l) for l in self.links}
        time.sleep(secs)
        return {l: self.bytes(l) - b0[l] for l in self.links}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.path.join(ROOT, "target", "debug", "nexus"))
    ap.add_argument("--rate1", type=float, default=0)
    ap.add_argument("--rate2", type=float, default=0)
    ap.add_argument("--hold", type=float, default=15, help="断开后观察多少秒")
    args = ap.parse_args()
    if os.geteuid() != 0 or not shutil.which("iptables"):
        print("需要 root 和 iptables")
        return 1

    p = Probe(args)
    try:
        p.setup()
        p.start("home")
        p.start("laptop")
        p.cli("home", "init", "--name", "home")
        code = p.cli("laptop", "join")["code"]
        p.cli("home", "device", "approve", code, "--name", "laptop")

        arrivals = []
        ex = subprocess.Popen([p.bin, "exec", "home", "--", "sh", "-c", TICK], env=p.env("laptop"),
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
        p.procs.append(ex)

        def reader():
            for _ in ex.stdout:
                arrivals.append(time.monotonic())
        threading.Thread(target=reader, daemon=True).start()
        time.sleep(3)
        print(f"会话开始后 3 秒：收到 {len(arrivals)} 行")

        events = []
        for round_ in range(2):
            use = p.active()
            busy = max(use, key=use.get)
            print(f"第 {round_ + 1} 轮：1 秒内各链路字节数 {use} → 正在用 {busy}")
            t_cut = time.monotonic()
            p.cut(busy, True)
            time.sleep(args.hold)
            after = [t for t in arrivals if t > t_cut]
            gap = (after[0] - t_cut) if after else None
            use2 = p.active()
            alive = ex.poll() is None
            print(f"  断开 {busy}：之后第一行在 {gap if gap is None else round(gap, 3)} 秒后到达；"
                  f"{args.hold:g} 秒内收到 {len(after)} 行；会话{'还在' if alive else '已结束：' + ex.stderr.read()}；之后流量 {use2}")
            events.append({"cut": busy, "first_after_s": gap, "lines": len(after), "alive": alive, "bytes_after": use2})
            p.cut(busy, False)
            time.sleep(3)
            if not alive:
                break
        # 最长的停顿
        gaps = [b - a for a, b in zip(arrivals, arrivals[1:])]
        print(f"全程最长停顿 {max(gaps):.3f} 秒（正常间隔约 0.02 秒）" if gaps else "没有收到输出")
        print(json.dumps(events, ensure_ascii=False))
    finally:
        p.teardown()
    return 0


if __name__ == "__main__":
    sys.exit(main())
