#!/usr/bin/env python3
"""多路径的检查和测量（M5.1 起）。

laptop 和 home 的常驻程序各在一个网络命名空间里，中间有两条独立的链路（link1、link2，不同网段；
可以分别限速）。检查：

  1. 两条链路都用上：`nexus path home` 列出两条直连路径，本机地址分别在两条链路上。（R56）
  2. 断一条，会话不断：laptop 上 `nexus exec home -- 每 20 毫秒输出一行`，用 iptables 丢弃正在用的链路上的
     所有包（接口不 down，像信号没了），输出的最长停顿不超过 --max-pause（默认 1 秒），命令不结束；
     恢复这条链路，等它重新有路径，再断另一条，同样检查。（R57）
  3. 交互冗余：终端会话（`nexus ssh home`，交互流量）每 20 毫秒输出一行时，两条链路上的流量相当；
     一条链路随机丢 30% 的包时，输出的最长间隔不超过 --max-gap（默认 0.2 秒）。同样的丢包下也测一遍
     `nexus exec`（普通流量，不冗余）作对比。（R58）
  4. 计费网络：把 laptop 在 link2 上的接口标为计费，从 home 取一个大文件（批量流量），link2 上几乎没有这部分流量；
     终端输出照样两条都走。（R60）
  5. 批量分摊：两条链路都限速（--bulk-rate，默认 20 Mbit/s），取大文件的吞吐超过一条链路的 1.3 倍。（R59）
  结果写进 --record（R61 的测量记录）。

需要 root、ip、iptables。条件不满足时说明原因并跳过（退出码 0），除非给出 --require。

用法：tools/multipath_check.py [--bin target/debug/nexus] [--rate1 0] [--rate2 0] [--record 文件] [--require]
"""
# covers: R56, R57, R58, R59, R60, R61
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
MARK = "NXT"  # 终端会话里每行输出的前缀（区分回显和提示符）


def sh(*cmd, check=True):
    return subprocess.run(cmd, check=check, capture_output=True, text=True)


class Net:
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
        if self.args.keep:
            print(f"临时目录：{self.tmp}")
        else:
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

    def link_of(self, ip):
        for name, (net, _, _) in self.links.items():
            if ip.startswith(net + "."):
                return name
        return None

    def direct_links(self):
        """laptop 到 home 的直连路径经过哪些链路。"""
        paths = self.cli("laptop", "path", "home")["devices"][0]["paths"]
        return sorted({self.link_of(p["local"]) for p in paths if p["kind"] == "direct"} - {None}), paths

    def wait_links(self, want, secs):
        deadline = time.time() + secs
        while time.time() < deadline:
            links, paths = self.direct_links()
            if len(links) >= want:
                return links, paths
            time.sleep(0.2)
        return self.direct_links()

    def bytes(self, link):
        _, va, _ = self.links[link]
        st = sh("ip", "netns", "exec", self.ns["laptop"], "cat", f"/sys/class/net/{va}/statistics/rx_bytes",
                f"/sys/class/net/{va}/statistics/tx_bytes").stdout.split()
        return int(st[0]) + int(st[1])

    def busy(self, secs=1.0):
        b0 = {l: self.bytes(l) for l in self.links}
        time.sleep(secs)
        use = {l: self.bytes(l) - b0[l] for l in self.links}
        return max(use, key=use.get), use

    def lossy(self, link, prob, on):
        """laptop 收到的、经过这条链路的包随机丢掉 `prob`。"""
        _, va, _ = self.links[link]
        op = "-A" if on else "-D"
        sh("ip", "netns", "exec", self.ns["laptop"], "iptables", op, "INPUT", "-i", va, "-m", "statistic",
           "--mode", "random", "--probability", str(prob), "-j", "DROP")

    def iface(self, link):
        return self.links[link][1]

    def cut(self, link, on):
        _, va, _ = self.links[link]
        op = "-A" if on else "-D"
        for chain, flag in (("INPUT", "-i"), ("OUTPUT", "-o")):
            sh("ip", "netns", "exec", self.ns["laptop"], "iptables", op, chain, flag, va, "-j", "DROP")


def missing_requirements():
    if os.geteuid() != 0:
        return "需要 root（建网络命名空间、iptables）"
    for tool in ("ip", "iptables"):
        if not shutil.which(tool):
            return f"没有 {tool}"
    probe = sh("ip", "netns", "add", "nxprobe", check=False)
    sh("ip", "netns", "del", "nxprobe", check=False)
    if probe.returncode != 0:
        return f"不能建网络命名空间：{probe.stderr.strip()}"
    return None


class Ticker:
    """收集一个持续输出的命令每行到达的时间。"""

    def __init__(self, proc, marker=None):
        self.proc = proc
        self.arrivals = []
        self.marker = marker
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            if self.marker is None or line.startswith(self.marker):
                self.arrivals.append(time.monotonic())

    def gaps(self, since, until):
        ts = [since] + [t for t in self.arrivals if since < t <= until] + [until]
        return sorted(b - a for a, b in zip(ts, ts[1:]))


def pct(xs, p):
    return xs[min(len(xs) - 1, int(len(xs) * p))] if xs else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.path.join(ROOT, "target", "debug", "nexus"))
    ap.add_argument("--rate1", type=float, default=0, help="link1 限速（Mbit/s，0 不限）")
    ap.add_argument("--rate2", type=float, default=0)
    ap.add_argument("--hold", type=float, default=5, help="断开后观察多少秒")
    ap.add_argument("--max-pause", type=float, default=1.0, help="断开一条时输出允许的最长停顿（秒）")
    ap.add_argument("--loss", type=float, default=0.3, help="交互冗余检查时一条链路的随机丢包率")
    ap.add_argument("--max-gap", type=float, default=0.2, help="有丢包时终端输出允许的最长间隔（秒）")
    ap.add_argument("--bulk-rate", type=float, default=20, help="批量检查时两条链路的限速（Mbit/s）")
    ap.add_argument("--bulk-mb", type=float, default=16, help="批量检查取的文件大小（MB）")
    ap.add_argument("--only", default="failover,redundancy,metered,bulk", help="只做这些检查（逗号分隔）")
    ap.add_argument("--record", help="把结果写成 JSON")
    ap.add_argument("--require", action="store_true", help="条件不满足时失败而不是跳过")
    ap.add_argument("--keep", action="store_true", help="保留临时目录（常驻程序的日志在里面）")
    args = ap.parse_args()
    why = missing_requirements()
    if why:
        print(f"跳过多路径检查：{why}")
        return 1 if args.require else 0
    phases = set(args.only.split(","))
    if {"metered", "bulk"} & phases:
        # 批量检查要两条链路都限速（才看得出分摊）；其他检查在限速下照样做。
        args.rate1 = args.rate1 or args.bulk_rate
        args.rate2 = args.rate2 or args.bulk_rate

    failures = []
    result = {"rate1_mbit": args.rate1, "rate2_mbit": args.rate2}
    n = Net(args)
    try:
        n.setup()
        n.start("home")
        n.start("laptop")
        n.cli("home", "init", "--name", "home")
        code = n.cli("laptop", "join")["code"]
        n.cli("home", "device", "approve", code, "--name", "laptop")

        ex = subprocess.Popen([n.bin, "exec", "home", "--", "sh", "-c", TICK], env=n.env("laptop"),
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
        n.procs.append(ex)
        exec_tick = Ticker(ex)

        # 1. 两条链路都用上（R56）。
        links, paths = n.wait_links(2, 10)
        result["paths"] = paths
        print(f"路径：{[(p['local'], p['remote'], p['kind']) for p in paths]}")
        if links != ["link1", "link2"]:
            failures.append(f"直连路径只经过 {links}，应该两条链路都有")
        _, use = n.busy(2.0)
        print(f"两秒内各链路的字节数：{use}")
        if min(use.values()) == 0:
            failures.append(f"有一条链路上没有流量：{use}")

        # 2. 断一条，会话不断；恢复后再断另一条（R57）。
        if "failover" in phases:
            result["failover"] = []
            cut_before = None
            for round_ in range(2):
                if round_ == 1:
                    n.cut(cut_before, False)
                    links, _ = n.wait_links(2, 15)
                    if len(links) < 2:
                        failures.append(f"{cut_before} 恢复 15 秒后还没有重新用上")
                        break
                busy, _ = n.busy(1.0)
                if round_ == 1 and busy == cut_before:
                    busy = "link2" if cut_before == "link1" else "link1"
                t_cut = time.monotonic()
                n.cut(busy, True)
                time.sleep(args.hold)
                gaps = exec_tick.gaps(t_cut, time.monotonic())
                pause = gaps[-1]
                alive = ex.poll() is None
                print(f"故障转移 {round_ + 1}：断开 {busy}，最长停顿 {pause:.3f} 秒，会话{'还在' if alive else '已结束'}")
                result["failover"].append({"cut": busy, "max_pause_s": round(pause, 3), "alive": alive})
                if not alive:
                    failures.append(f"断开 {busy} 后命令结束了：{ex.stderr.read()}")
                    break
                if pause > args.max_pause:
                    failures.append(f"断开 {busy} 后输出停了 {pause:.3f} 秒，上限 {args.max_pause:g} 秒")
                cut_before = busy
            if cut_before:
                n.cut(cut_before, False)
                n.wait_links(2, 15)

        # 3. 交互冗余（R58）：终端会话的输出两条链路都走；一条丢包时间隔不变大。
        term = None
        if {"redundancy", "metered"} & phases:
            term = subprocess.Popen([n.bin, "ssh", "home"], env=n.env("laptop"), stdin=subprocess.PIPE,
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
            n.procs.append(term)
            term_tick = Ticker(term, MARK)
            term.stdin.write(f"while :; do echo {MARK}$(date +%s%N); sleep 0.02; done\n")
            term.stdin.flush()
            deadline = time.time() + 10
            while len(term_tick.arrivals) < 20 and time.time() < deadline:
                time.sleep(0.1)
            if len(term_tick.arrivals) < 20:
                failures.append(f"终端会话没有输出：{term.stderr.read() if term.poll() is not None else ''}")
        if "redundancy" in phases and term and len(term_tick.arrivals) >= 20:
            busy, use = n.busy(2.0)
            other = "link2" if busy == "link1" else "link1"
            ratio = use[other] / max(use[busy], 1)
            print(f"交互冗余：终端输出时两秒内各链路的字节数 {use}（少的一条是多的一条的 {ratio:.0%}）")
            red = {"bytes": use}
            if ratio < 0.5:
                failures.append(f"终端输出没有两条链路都走：{use}")
            for label, tick in (("终端会话（冗余）", term_tick), ("exec（普通流量，不冗余）", exec_tick)):
                t0 = time.monotonic()
                n.lossy(busy, args.loss, True)
                time.sleep(6)
                n.lossy(busy, args.loss, False)
                g = tick.gaps(t0, time.monotonic())
                p99, mx = pct(g, 0.99), g[-1]
                print(f"  {busy} 随机丢 {args.loss:.0%}：{label} 输出间隔 p99 {p99 * 1000:.0f} 毫秒、最长 {mx * 1000:.0f} 毫秒")
                red["term" if tick is term_tick else "exec"] = {"p99_gap_ms": round(p99 * 1000, 1), "max_gap_ms": round(mx * 1000, 1)}
                if tick is term_tick and mx > args.max_gap:
                    failures.append(f"一条链路丢包时终端输出最长间隔 {mx:.3f} 秒，上限 {args.max_gap:g} 秒")
                time.sleep(1)
            result["redundancy"] = red

        # 4、5. 计费网络（R60）和批量分摊（R59）：从 home 取大文件。
        if {"metered", "bulk"} & phases:
            size = int(args.bulk_mb * 1e6)
            n.cli("home", "drive", "serve", "on")
            n.cli("home", "drive", "create", "work")
            for name in ("one.bin", "two.bin"):
                p = subprocess.run([n.bin, "drive", "put", f"work:/{name}", "--json"], env=n.env("home"),
                                   input=os.urandom(size), capture_output=True, timeout=120)
                if p.returncode != 0:
                    raise RuntimeError(p.stderr.decode())
            n.cli("home", "drive", "commit", "work", "-m", "bulk")
            n.cli("laptop", "drive", "sync", "work")

            def fetch(name):
                b0 = {l: n.bytes(l) for l in n.links}
                t = time.monotonic()
                p = subprocess.run([n.bin, "drive", "cat", f"work:/{name}"], env=n.env("laptop"),
                                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=300)
                secs = time.monotonic() - t
                if p.returncode != 0:
                    raise RuntimeError(p.stderr.decode())
                use = {l: n.bytes(l) - b0[l] for l in n.links}
                return size * 8 / secs / 1e6, use

            metered_link = "link2"
            n.cli("laptop", "iface", n.iface(metered_link), "--metered", "on")
            time.sleep(7)  # 传输在下一次检查连接时（5 秒内）按新的设置调度
            mbit_m, use_m = fetch("one.bin")
            share = use_m[metered_link] / max(sum(use_m.values()), 1)
            print(f"计费网络：{metered_link} 标为计费，取 {args.bulk_mb:g} MB：{mbit_m:.1f} Mbit/s，各链路字节数 {use_m}（计费的占 {share:.1%}）")
            result["metered"] = {"mbit": round(mbit_m, 1), "bytes": use_m, "metered_share": round(share, 4)}
            if "metered" in phases and share > 0.1:
                failures.append(f"计费的链路承担了 {share:.0%} 的批量流量")
            n.cli("laptop", "iface", n.iface(metered_link), "--metered", "auto")
            time.sleep(7)
            mbit_b, use_b = fetch("two.bin")
            single = min(args.rate1, args.rate2)
            print(f"批量分摊：两条都不计费，取 {args.bulk_mb:g} MB：{mbit_b:.1f} Mbit/s（一条链路 {single:g} Mbit/s），各链路字节数 {use_b}")
            result["bulk"] = {"mbit": round(mbit_b, 1), "single_link_mbit": single, "bytes": use_b}
            if "bulk" in phases and mbit_b < single * 1.3:
                failures.append(f"两条链路时批量吞吐 {mbit_b:.1f} Mbit/s，没有超过一条链路的 1.3 倍（{single * 1.3:.1f}）")
    finally:
        n.teardown()

    if args.record:
        with open(args.record, "w", encoding="utf-8") as f:
            json.dump(result, f, ensure_ascii=False, indent=2)
    for f in failures:
        print("失败：" + f)
    if not failures:
        print("通过：" + "、".join(sorted(phases)))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
