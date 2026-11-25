#!/usr/bin/env python3
"""Android 上的多路径和计费网络识别（M11.4）：在 redroid（Android 13，x86_64）里装上 Nexus 应用。

redroid 的网络命名空间（容器和宿主共用网络时就是宿主的）里加两条 veth 链路（nxl1、nxl2），另一端在一个
新的网络命名空间里，那里运行 home 的常驻程序。Android 上的核心（应用里的同一份多路径代码）因此有两个接口连到 home：

  1. `nexus path home`（在 phone 上）列出两条直连路径，本机地址分别在两条链路上。（R87）
  2. phone 上 `nexus exec home -- 每 20 毫秒输出一行`，丢弃正在用的链路上的所有包，输出在 --max-pause（默认 1 秒）
     内恢复，命令不结束。（R87）
  3. 应用把系统的网络（ConnectivityManager）交给核心：logcat 里有「网络：…」，`nexus iface` 里系统报告过的接口
     的自动识别结果和系统一致（redroid 的 eth0 不计费）；两条 veth 系统不知道，按名字识别为不计费。（R88）

veth 不是 Android 的网络，Android 的策略路由不知道它们：脚本在 phone 的命名空间里加一条 `to 10.82.0.0/16 lookup main`
的规则（只为测试；真机上 Wi-Fi 和蜂窝网络都有系统建的路由表）。

需要 root（或免密 sudo）、adb、ip、iptables、nsenter；先运行 tools/build-android.sh 和 cargo build -p nexus-cli。
用法：tools/android_multipath.py [--serial 127.0.0.1:5555] [--container redroid] [--home-bin target/debug/nexus]
"""
# covers: R87, R88
import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PKG = "link.bewith.nexus"
APP_DIR = f"/data/data/{PKG}/files"
PHONE_CLI = "/data/local/tmp/nexus"
NET = {"nxl1": "10.82.1", "nxl2": "10.82.2"}
HOME_NS = "nxmphome"
TICK = "while :; do date +%s.%N; sleep 0.02; done"


def run(*cmd, check=True, timeout=60, **kw):
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, **kw)
    if check and out.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)}：{out.stderr or out.stdout}")
    return out.stdout


class MP:
    def __init__(self, a):
        self.a = a
        self.sudo = [] if os.geteuid() == 0 else ["sudo", "-n"]
        self.tmp = tempfile.mkdtemp(prefix="nexus-android-mp-")
        self.home_dir = os.path.join(self.tmp, "home")
        os.makedirs(self.home_dir)
        self.home_env = dict(os.environ, NEXUS_DATA_DIR=self.home_dir, NEXUS_AGENT_SOCKET=os.path.join(self.home_dir, "agent.sock"))
        self.daemon = None
        self.ticker = None
        self.pid = None

    # ------------------------------------------------------------ 两边的命令
    def root(self, *cmd, check=True):
        return run(*self.sudo, *cmd, check=check)

    def in_phone(self, *cmd, check=True):
        """在 phone（redroid）的网络命名空间里运行宿主上的命令。"""
        if self.pid:
            return self.root("nsenter", "-t", self.pid, "-n", *cmd, check=check)
        return self.root(*cmd, check=check)

    def adb(self, *args, check=True, timeout=60):
        return run("adb", "-s", self.a.serial, *args, check=check, timeout=timeout)

    def phone(self, *args, ok=True):
        cmd = f"NEXUS_DATA_DIR={APP_DIR} {PHONE_CLI} " + " ".join(f"'{x}'" for x in args) + " --json; echo EXIT=$?"
        body, _, code = self.adb("shell", cmd).rpartition("EXIT=")
        if ok and code.strip() != "0":
            raise RuntimeError(f"phone: nexus {' '.join(args)} 失败（{code.strip()}）：{body}")
        return json.loads(body) if body.strip() else {}

    def home(self, *args):
        out = subprocess.run([self.a.home_bin, *args, "--json"], env=self.home_env, capture_output=True, text=True, timeout=60)
        if out.returncode != 0:
            raise RuntimeError(f"home: nexus {' '.join(args)} 失败（{out.returncode}）：{out.stderr}")
        return json.loads(out.stdout) if out.stdout.strip() else {}

    # ------------------------------------------------------------ 准备
    def setup(self):
        if self.a.container:
            self.pid = run("docker", "inspect", "-f", "{{.State.Pid}}", self.a.container, check=False).strip() or \
                self.root("docker", "inspect", "-f", "{{.State.Pid}}", self.a.container).strip()
        # 链路：phone 一端 10.82.x.1，home 一端 10.82.x.2。
        self.root("ip", "netns", "add", HOME_NS)
        self.root("ip", "-n", HOME_NS, "link", "set", "lo", "up")
        for dev, net in NET.items():
            peer = dev + "h"
            self.root("ip", "link", "add", dev, "type", "veth", "peer", "name", peer)
            self.root("ip", "link", "set", peer, "netns", HOME_NS)
            if self.pid:
                self.root("ip", "link", "set", dev, "netns", self.pid)
            self.in_phone("ip", "addr", "add", f"{net}.1/24", "dev", dev)
            self.in_phone("ip", "link", "set", dev, "up")
            self.root("ip", "-n", HOME_NS, "addr", "add", f"{net}.2/24", "dev", peer)
            self.root("ip", "-n", HOME_NS, "link", "set", peer, "up")
        self.in_phone("ip", "rule", "add", "pref", "50", "to", "10.82.0.0/16", "lookup", "main")

        # phone：装应用、启动（建 VPN、启动核心）。
        self.adb("root", check=False)
        time.sleep(2)
        run("adb", "connect", self.a.serial, check=False)
        self.adb("shell", "[ -e /dev/tun ] || { mknod /dev/tun c 10 200 && chmod 666 /dev/tun && chcon u:object_r:tun_device:s0 /dev/tun; }",
                 check=False)
        self.adb("shell", "am", "force-stop", PKG, check=False)
        self.adb("uninstall", PKG, check=False)
        self.adb("install", "-r", "-g", self.a.apk, timeout=180)
        self.adb("shell", "appops", "set", PKG, "ACTIVATE_VPN", "allow")
        self.adb("push", self.a.cli, PHONE_CLI)
        self.adb("shell", "chmod", "755", PHONE_CLI)
        self.adb("logcat", "-c", check=False)

        # home：在自己的命名空间里。
        log = open(os.path.join(self.tmp, "home.log"), "w")
        self.daemon = subprocess.Popen([*self.sudo, "env", f"NEXUS_DATA_DIR={self.home_dir}",
                                        f"NEXUS_AGENT_SOCKET={self.home_env['NEXUS_AGENT_SOCKET']}",
                                        "ip", "netns", "exec", HOME_NS, self.a.home_bin, "daemon", "--name", "home",
                                        "--relay", "disabled", "--no-discovery"], stdout=log, stderr=log)
        sock = os.path.join(self.home_dir, "ctl.sock")
        deadline = time.time() + 20
        while not os.path.exists(sock):
            if time.time() > deadline or self.daemon.poll() is not None:
                raise RuntimeError("home 没有就绪：" + open(os.path.join(self.tmp, "home.log")).read())
            time.sleep(0.05)
        if self.sudo:
            self.root("chmod", "-R", "a+rwX", self.home_dir)
        self.home("init", "--name", "home")

        self.adb("shell", "am", "start", "-n", f"{PKG}/.MainActivity",
                 "--es", "name", "phone", "--es", "relay", "disabled", "--ez", "discovery", "false")
        deadline = time.time() + 30
        while "ctl.sock" not in self.adb("shell", "ls", APP_DIR, check=False):
            if time.time() > deadline:
                raise RuntimeError("phone 的核心没有就绪：" + self.adb("shell", "cat", f"{APP_DIR}/daemon.log", check=False))
            time.sleep(0.5)
        code = self.phone("join")["code"]
        self.home("device", "approve", code, "--name", "phone")
        deadline = time.time() + 30
        while self.phone("status", "home", ok=False).get("online") is not True:
            if time.time() > deadline:
                raise RuntimeError("phone 连不上 home")
            time.sleep(0.5)

    def teardown(self):
        if self.ticker:
            self.ticker.kill()
        self.adb("shell", "pkill -f 'nexus.*exec home'", check=False)
        self.adb("shell", "am", "start", "-n", f"{PKG}/.MainActivity", "--es", "action", "stop", check=False)
        self.adb("shell", "am", "force-stop", PKG, check=False)
        if self.daemon:
            self.root("pkill", "-f", f"daemon --name home --relay disabled", check=False)
            try:
                self.daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.daemon.kill()
        for dev in NET:
            self.cut(dev, False, check=False)
            self.in_phone("ip", "link", "del", dev, check=False)
        self.in_phone("ip", "rule", "del", "pref", "50", "to", "10.82.0.0/16", "lookup", "main", check=False)
        self.root("ip", "netns", "del", HOME_NS, check=False)
        if not self.a.keep:
            self.root("rm", "-rf", self.tmp, check=False)

    # ------------------------------------------------------------ 链路
    def link_bytes(self, dev):
        st = self.in_phone("cat", f"/sys/class/net/{dev}/statistics/rx_bytes", f"/sys/class/net/{dev}/statistics/tx_bytes").split()
        return int(st[0]) + int(st[1])

    def busy(self, secs=1.0):
        b0 = {d: self.link_bytes(d) for d in NET}
        time.sleep(secs)
        use = {d: self.link_bytes(d) - b0[d] for d in NET}
        return max(use, key=use.get), use

    def cut(self, dev, on, check=True):
        op = "-I" if on else "-D"
        for chain, flag in (("INPUT", "-i"), ("OUTPUT", "-o")):
            self.in_phone("iptables", op, chain, flag, dev, "-j", "DROP", check=check)

    def direct_links(self):
        paths = self.phone("path", "home")["devices"][0]["paths"]
        links = sorted({d for p in paths if p["kind"] == "direct" for d, net in NET.items() if p["local"].startswith(net + ".")})
        return links, paths

    # ------------------------------------------------------------ 检查
    def run(self):
        failures = []

        def check(what, ok, detail=""):
            print(("通过  " if ok else "失败  ") + what + ("" if ok else f"：{detail}"))
            if not ok:
                failures.append(what)

        # 1. 两条链路都有直连路径（R87）。
        deadline = time.time() + 30
        links, paths = self.direct_links()
        while links != sorted(NET) and time.time() < deadline:
            time.sleep(0.5)
            links, paths = self.direct_links()
        print(f"路径：{[(p['local'], p['remote'], p['kind']) for p in paths]}")
        check("phone 到 home 两条链路上都有直连路径", links == sorted(NET), json.dumps(paths))

        # 2. 断一条，会话不断（R87）。
        cmd = f"NEXUS_DATA_DIR={APP_DIR} {PHONE_CLI} exec home -- sh -c '{TICK}'"
        self.ticker = subprocess.Popen(["adb", "-s", self.a.serial, "exec-out", cmd], stdout=subprocess.PIPE, text=True)
        arrivals = []

        def read():
            for _ in self.ticker.stdout:
                arrivals.append(time.monotonic())

        threading.Thread(target=read, daemon=True).start()
        time.sleep(3)
        check("phone 上的 exec 有输出", len(arrivals) > 20, f"{len(arrivals)} 行")
        busy, use = self.busy(1.0)
        t_cut = time.monotonic()
        self.cut(busy, True)
        time.sleep(self.a.hold)
        now = time.monotonic()
        ts = [t_cut] + [t for t in arrivals if t > t_cut] + [now]
        pause = max(b - a for a, b in zip(ts, ts[1:]))
        alive = self.ticker.poll() is None
        print(f"故障转移：断开 {busy}（断前一秒的字节数 {use}），最长停顿 {pause:.3f} 秒，会话{'还在' if alive else '已结束'}")
        check(f"断开 {busy} 后输出在 {self.a.max_pause:g} 秒内恢复、命令不结束", alive and pause <= self.a.max_pause,
              f"停顿 {pause:.3f} 秒")
        self.cut(busy, False)

        # 3. 系统报告的网络（R88）。
        log = self.adb("logcat", "-d", "-s", "nexus", check=False)
        reported = re.findall(r"网络：(.*)", log)
        print(f"系统报告的网络：{reported[-1] if reported else '（没有）'}")
        check("应用把系统的网络交给核心（logcat 里有「网络：」）", bool(reported), log[-1000:])
        system = {}
        for item in (reported[-1].split() if reported else []):
            system[item.replace("（计费）", "")] = item.endswith("（计费）")
        time.sleep(6)  # 核心每 5 秒查看一次接口
        ifaces = {i["name"]: i for i in self.phone("iface")["interfaces"]}
        print("接口：" + "、".join(f"{n}（{'计费' if i['auto_metered'] else '不计费'}）" for n, i in ifaces.items()))
        for name, metered in system.items():
            if name in ifaces:
                check(f"{name} 的自动识别和系统一致（{'计费' if metered else '不计费'}）", ifaces[name]["auto_metered"] == metered,
                      json.dumps(ifaces[name]))
        for dev in NET:
            check(f"{dev}（系统不知道的接口）按名字识别为不计费", dev in ifaces and not ifaces[dev]["auto_metered"],
                  json.dumps(ifaces.get(dev)))
        return failures


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--serial", default="127.0.0.1:5555")
    ap.add_argument("--container", default="", help="redroid 的容器名；容器和宿主共用网络时留空")
    ap.add_argument("--apk", default=os.path.join(ROOT, "target/android/nexus.apk"))
    ap.add_argument("--cli", default=os.path.join(ROOT, "target/android/x86_64/nexus"))
    ap.add_argument("--home-bin", default=os.path.join(ROOT, "target/debug/nexus"))
    ap.add_argument("--hold", type=float, default=5, help="断开后观察多少秒")
    ap.add_argument("--max-pause", type=float, default=1.0)
    ap.add_argument("--keep", action="store_true", help="保留临时目录（home 的日志在里面）")
    a = ap.parse_args()
    for tool in ("adb", "ip", "iptables", "nsenter"):
        if not shutil.which(tool) and not os.path.exists(f"/usr/sbin/{tool}"):
            print(f"跳过：没有 {tool}")
            return 0
    t = MP(a)
    try:
        t.setup()
        failures = t.run()
    finally:
        t.teardown()
    print("全部通过" if not failures else f"{len(failures)} 项失败")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
