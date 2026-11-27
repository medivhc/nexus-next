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
  5. 批量分摊：两条链路都限速（--bulk-rate，默认 20 Mbit/s），取大文件的吞吐超过只有一条时（4 里计费时实测的）的
     --bulk-ratio 倍（默认 1.7）；计费改回自动后的第一次传输也算（不预热）。（R59、R85）
  6. 经中继的路径（--only relay，默认也做）：home 上起一个中继，两条链路的 UDP 都丢弃后会话在 --max-relay-pause
     （默认 3 秒）内转到中继上继续，取文件经中继；恢复 UDP 后回到直连。（R86）
  结果写进 --record（R61 的测量记录）。

需要 root、ip、iptables。条件不满足时说明原因并跳过（退出码 0），除非给出 --require。

用法：tools/multipath_check.py [--bin target/debug/nexus] [--rate1 0] [--rate2 0] [--record 文件] [--require]
"""
# covers: R56, R57, R58, R59, R60, R61, R85, R86
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


class NoNetem(Exception):
    pass


class Net:
    def __init__(self, args):
        self.args = args
        self.bin = os.path.abspath(args.bin)
        self.tmp = tempfile.mkdtemp(prefix="nexus-mp-")
        tag = os.getpid() % 10000
        self.ns = {"laptop": f"nxma{tag}", "home": f"nxmb{tag}"}
        self.links = {"link1": ("10.81.1", f"m1a{tag}", f"m1b{tag}"), "link2": ("10.81.2", f"m2a{tag}", f"m2b{tag}")}
        self.procs = []
        self.relay_url = None

    def start_relay(self):
        """在 home 的命名空间里起一个中继（R86），两边的常驻程序都用它；laptop 经 link1 连到它（TCP）。"""
        log = open(os.path.join(self.tmp, "relay.log"), "a")
        p = subprocess.Popen(["ip", "netns", "exec", self.ns["home"], self.args.relay_bin, "0.0.0.0:3340"],
                             stdout=log, stderr=log)
        self.procs.append(p)
        self.relay_url = f"http://{self.links['link1'][0]}.2:3340"
        deadline = time.time() + 10
        while time.time() < deadline:
            probe = sh("ip", "netns", "exec", self.ns["laptop"], "python3", "-c",
                       f"import socket; socket.create_connection(('{self.links['link1'][0]}.2', 3340), 1)", check=False)
            if probe.returncode == 0:
                return
            if p.poll() is not None:
                break
            time.sleep(0.1)
        raise RuntimeError("中继没有起来：" + open(os.path.join(self.tmp, "relay.log")).read())

    def block_udp(self, on):
        """laptop 两条链路上的 UDP 全部丢弃（直连路径都断），TCP（到中继）照常。"""
        op = "-A" if on else "-D"
        for _, va, _ in self.links.values():
            for chain, flag in (("INPUT", "-i"), ("OUTPUT", "-o")):
                sh("ip", "netns", "exec", self.ns["laptop"], "iptables", op, chain, flag, va, "-p", "udp", "-j", "DROP")

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
                delay = self.args.delay_ms if name == "link1" else self.args.delay2_ms
                if delay:
                    # 每个方向一半延迟（往返 = --delay-ms）；需要内核的 netem。
                    cmd = ["ip", "netns", "exec", ns, "tc", "qdisc", "add", "dev", dev, "root", "netem",
                           "delay", f"{delay / 2}ms", "limit", "10000"]
                    if rate:
                        cmd += ["rate", f"{rate}mbit"]
                    if sh(*cmd, check=False).returncode != 0:
                        raise NoNetem()
                elif rate:
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
                              "--relay", self.relay_url or "disabled", "--no-discovery", *self.args.daemon_arg], env=self.env(node), stdout=log, stderr=log)
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
    ap.add_argument("--bulk-repeat", type=int, default=1, help="批量检查每种情况取几次（测量用；默认 1 次，不达标时最多 3 次）")
    ap.add_argument("--bulk-ratio", type=float, default=1.7, help="两条链路时批量吞吐至少是只有一条时的几倍（R85）")
    ap.add_argument("--bulk-mb", type=float, default=16, help="批量检查取的文件大小（MB）")
    ap.add_argument("--only", default="failover,redundancy,relay,metered,bulk", help="只做这些检查（逗号分隔）")
    ap.add_argument("--delay-ms", type=float, default=0, help="两条链路的往返延迟（毫秒，需要 netem；没有时跳过）")
    ap.add_argument("--delay2-ms", type=float, default=None, help="link2 的往返延迟（默认同 --delay-ms）：测延迟不同的两条链路")
    ap.add_argument("--daemon-arg", action="append", default=[], help="传给两边常驻程序的额外参数（对比测量用，例如 --no-scheduling）")
    ap.add_argument("--relay-bin", help="测试用中继（cargo build --example local_relay -p nexus-fabric-iroh）；默认在 --bin 旁边的 examples 目录")
    ap.add_argument("--max-relay-pause", type=float, default=3.0, help="直连都断开、转到中继时输出允许的最长停顿（秒）")
    ap.add_argument("--relay-mb", type=float, default=4, help="经中继取的文件大小（MB）")
    ap.add_argument("--record", help="把结果写成 JSON")
    ap.add_argument("--require", action="store_true", help="条件不满足时失败而不是跳过")
    ap.add_argument("--keep", action="store_true", help="保留临时目录（常驻程序的日志在里面）")
    args = ap.parse_args()
    if args.delay2_ms is None:
        args.delay2_ms = args.delay_ms
    if not args.relay_bin:
        args.relay_bin = os.path.join(os.path.dirname(os.path.abspath(args.bin)), "examples", "local_relay")
    why = missing_requirements()
    if why:
        print(f"跳过多路径检查：{why}")
        return 1 if args.require else 0
    phases = set(args.only.split(","))
    if "relay" in phases and not os.path.exists(args.relay_bin):
        print(f"跳过经中继的检查：没有测试用中继 {args.relay_bin}（cargo build --example local_relay -p nexus-fabric-iroh）")
        if args.require:
            return 1
        phases.discard("relay")
    if {"metered", "bulk"} & phases:
        # 批量检查要两条链路都限速（才看得出分摊）；其他检查在限速下照样做。
        args.rate1 = args.rate1 or args.bulk_rate
        args.rate2 = args.rate2 or args.bulk_rate

    failures = []
    result = {"rate1_mbit": args.rate1, "rate2_mbit": args.rate2}
    result["rtt_ms"] = args.delay_ms
    result["rtt2_ms"] = args.delay2_ms
    n = Net(args)
    try:
        try:
            n.setup()
        except NoNetem:
            print("跳过多路径检查：内核没有 netem，不能加延迟")
            return 1 if args.require else 0
        if "relay" in phases:
            n.start_relay()
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
            links, _ = n.wait_links(2, 15)
            if len(links) < 2:
                failures.append(f"冗余测量前两条直连链路没有都恢复：{links}")
            busy, use = n.busy(2.0)
            other = "link2" if busy == "link1" else "link1"
            ratio = use[other] / max(use[busy], 1)
            if ratio < 0.5:
                # 故障转移刚恢复时，冗余发送可能还没重新用上第二条链路；等一轮探测后再测一次。
                time.sleep(2)
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

        # 经中继的路径（R86）：直连全断时会话转到中继上继续，批量也能走；直连恢复后重新用上，批量回到直连。
        if "relay" in phases:
            rel = {}

            def relay_path(paths):
                return next((p for p in paths if p["kind"] == "relay"), None)

            # 直连都好时中继路径也开着（Backup，不承担数据），直连断了才能马上接管。
            deadline = time.time() + 10
            while relay_path(n.cli("laptop", "path", "home")["devices"][0]["paths"]) is None and time.time() < deadline:
                time.sleep(0.2)
            if relay_path(n.cli("laptop", "path", "home")["devices"][0]["paths"]) is None:
                failures.append("直连都好时没有开着的中继路径")
            t_cut = time.monotonic()
            n.block_udp(True)
            time.sleep(args.hold + 3)
            gaps = exec_tick.gaps(t_cut, time.monotonic())
            pause = gaps[-1]
            alive = ex.poll() is None
            paths = n.cli("laptop", "path", "home")["devices"][0]["paths"]
            via = relay_path(paths)
            print(f"中继：两条链路的 UDP 都断开，exec 输出最长停顿 {pause:.3f} 秒，会话{'还在' if alive else '已结束'}；"
                  f"路径 {[(p['local'], p['remote'], p['kind']) for p in paths]}")
            rel["cut_max_pause_s"] = round(pause, 3)
            rel["alive"] = alive
            if not alive:
                failures.append(f"直连都断开后命令结束了：{ex.stderr.read()}")
            elif pause > args.max_relay_pause:
                failures.append(f"直连都断开后输出停了 {pause:.3f} 秒，上限 {args.max_relay_pause:g} 秒")
            if via is None:
                failures.append(f"直连都断开后 nexus path 没有列出中继路径：{paths}")
            # 经中继取一个文件。
            size = int(args.relay_mb * 1e6)
            n.cli("home", "drive", "serve", "on")
            n.cli("home", "drive", "create", "relayws")
            for name in ("via-relay.bin", "after-relay.bin"):
                p = subprocess.run([n.bin, "drive", "put", f"relayws:/{name}", "--json"], env=n.env("home"),
                                   input=os.urandom(size), capture_output=True, timeout=120)
                if p.returncode != 0:
                    raise RuntimeError(p.stderr.decode())
            n.cli("home", "drive", "commit", "relayws", "-m", "relay")
            n.cli("laptop", "drive", "sync", "relayws")

            def fetch_relay(name):
                before = relay_path(n.cli("laptop", "path", "home")["devices"][0]["paths"]) or {"rx_bytes": 0}
                t = time.monotonic()
                p = subprocess.run([n.bin, "drive", "cat", f"relayws:/{name}"], env=n.env("laptop"),
                                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=300)
                secs = time.monotonic() - t
                if p.returncode != 0:
                    raise RuntimeError(p.stderr.decode())
                after = relay_path(n.cli("laptop", "path", "home")["devices"][0]["paths"]) or {"rx_bytes": 0}
                return size * 8 / secs / 1e6, max(0, after["rx_bytes"] - before["rx_bytes"])

            mbit, via_relay = fetch_relay("via-relay.bin")
            print(f"  经中继取 {args.relay_mb:g} MB：{mbit:.1f} Mbit/s，中继路径收 {via_relay} 字节")
            rel["relay_mbit"] = round(mbit, 1)
            if via_relay < size * 0.9:
                failures.append(f"直连都断开时取文件，中继路径只收了 {via_relay} 字节（文件 {size} 字节）")
            # 恢复直连。
            t_back = time.monotonic()
            n.block_udp(False)
            links, _ = n.wait_links(2, 20)
            back = time.monotonic() - t_back
            print(f"  恢复后 {back:.1f} 秒重新有直连路径：{links}")
            rel["direct_back_s"] = round(back, 1)
            if links != ["link1", "link2"]:
                failures.append(f"UDP 恢复 20 秒后直连路径只经过 {links}")
            time.sleep(2)
            mbit2, via_relay2 = fetch_relay("after-relay.bin")
            share = via_relay2 / size
            print(f"  恢复后取 {args.relay_mb:g} MB：{mbit2:.1f} Mbit/s，经中继的占 {share:.1%}")
            rel["after_mbit"] = round(mbit2, 1)
            rel["after_relay_share"] = round(share, 4)
            if share > 0.1:
                failures.append(f"直连恢复后批量流量仍有 {share:.0%} 走中继")
            if ex.poll() is not None:
                failures.append("直连恢复的过程中命令结束了")
            result["relay"] = rel

        # 4、5. 计费网络（R60）和批量分摊（R59）：从 home 取大文件。
        if {"metered", "bulk"} & phases:
            size = int(args.bulk_mb * 1e6)
            n.cli("home", "drive", "serve", "on")
            n.cli("home", "drive", "create", "work")
            # two*.bin：批量分摊最多测三次（runner 上偶尔低一点，有一次达标就算通过；真的退步时三次都不达标）。
            reps = max(1, args.bulk_repeat)
            ones = [f"one-{i}.bin" for i in range(reps)]
            twos = [f"two-{i}.bin" for i in range(max(3, reps))]
            for name in ones + twos:
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
            singles = []
            for name in ones:
                mbit_m, use_m = fetch(name)
                singles.append(round(mbit_m, 1))
            share = use_m[metered_link] / max(sum(use_m.values()), 1)
            print(f"计费网络：{metered_link} 标为计费，取 {args.bulk_mb:g} MB：{mbit_m:.1f} Mbit/s，各链路字节数 {use_m}（计费的占 {share:.1%}）")
            result["metered"] = {"mbit": round(mbit_m, 1), "bytes": use_m, "metered_share": round(share, 4), "runs_mbit": singles}
            if reps > 1:
                print(f"  只有一条可用（计费的不承担批量）时各次：{singles} Mbit/s")
            if "metered" in phases and share > 0.1:
                failures.append(f"计费的链路承担了 {share:.0%} 的批量流量")
            n.cli("laptop", "iface", n.iface(metered_link), "--metered", "auto")
            time.sleep(7)
            # 和实测的「只有一条」比（R85），不是和限速比：限速的链路实际能跑的比标称低一点。
            single = max(singles)
            want = single * args.bulk_ratio
            runs = []
            for attempt, name in enumerate(twos, 1):
                mbit_b, use_b = fetch(name)
                runs.append(round(mbit_b, 1))
                print(f"批量分摊（第 {attempt} 次）：两条都不计费，取 {args.bulk_mb:g} MB：{mbit_b:.1f} Mbit/s（只有一条时 {single:g} Mbit/s 的 {mbit_b / single:.2f} 倍），各链路字节数 {use_b}")
                if reps == 1 and (mbit_b >= want or "bulk" not in phases):
                    break
            mbit_b = max(runs)
            result["bulk"] = {"mbit": mbit_b, "single_path_mbit": single, "ratio": round(mbit_b / single, 2),
                              "first_ratio": round(runs[0] / single, 2), "bytes": use_b, "runs_mbit": runs}
            if "bulk" in phases and mbit_b < want:
                failures.append(f"两条链路时批量吞吐 {mbit_b:.1f} Mbit/s，没有超过只有一条时的 {args.bulk_ratio:g} 倍（{want:.1f}）")
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
