#!/usr/bin/env python3
"""Android 实测（M3.6）：在 redroid（或接在 adb 上的真机）里装上 Nexus 应用，验证同一个入口
同时承载内网访问和出口流量。

本机（运行脚本的 Linux）当 home：发布网页服务 web，开放出口。另外起一个只有 home 能按名字找到的网站
far.test（在 /etc/hosts 里临时加一行，结束时删掉；需要免密 sudo）。Android 设备 phone 加入网络，
规则 far.test 经 home 出去。然后在 Android 上用普通程序（nc，以 shell 账号运行）访问：

  - http://web.home.internal/      → home 上的服务（内网访问）
  - http://far.test/               → 经 home 出去（出口）；home 的审计里有 net.exit
  - localhost                      → 照常解析，不是 Nexus 的地址（其余流量不经过 Nexus）

用法：tools/android_e2e.py [--serial 127.0.0.1:5555] [--apk target/android/nexus.apk]
      [--cli target/android/x86_64/nexus] [--home-bin target/debug/nexus]
先运行 tools/build-android.sh 和 cargo build -p nexus-cli。
"""
# covers: R46
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
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PKG = "link.bewith.nexus"
APP_DIR = f"/data/data/{PKG}/files"
PHONE_CLI = "/data/local/tmp/nexus"
HOSTS_MARK = "# nexus-android-e2e"


def serve(addr, body):
    class H(BaseHTTPRequestHandler):
        def do_GET(self):
            data = body.encode()
            self.send_response(200)
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(data)

        def log_message(self, *a):
            pass

    srv = ThreadingHTTPServer(addr, H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


class E2E:
    def __init__(self, a):
        self.a = a
        self.tmp = tempfile.mkdtemp(prefix="nexus-android-")
        self.home_env = dict(os.environ, NEXUS_DATA_DIR=os.path.join(self.tmp, "home"),
                             NEXUS_AGENT_SOCKET=os.path.join(self.tmp, "home", "agent.sock"))
        os.makedirs(self.home_env["NEXUS_DATA_DIR"], exist_ok=True)
        self.daemon = None

    # ------------------------------------------------------------ 两边的命令
    def adb(self, *args, check=True, timeout=60):
        out = subprocess.run(["adb", "-s", self.a.serial, *args], capture_output=True, text=True, timeout=timeout)
        if check and out.returncode != 0:
            raise RuntimeError(f"adb {' '.join(args)}：{out.stderr or out.stdout}")
        return out.stdout

    def phone(self, *args, ok=True):
        cmd = f"NEXUS_DATA_DIR={APP_DIR} {PHONE_CLI} " + " ".join(f"'{x}'" for x in args) + " --json; echo EXIT=$?"
        out = self.adb("shell", cmd)
        body, _, code = out.rpartition("EXIT=")
        if ok and code.strip() != "0":
            raise RuntimeError(f"phone: nexus {' '.join(args)} 失败（{code.strip()}）：{body}")
        return json.loads(body) if body.strip() else {}

    def home(self, *args):
        out = subprocess.run([self.a.home_bin, *args, "--json"], env=self.home_env, capture_output=True, text=True, timeout=60)
        if out.returncode != 0:
            raise RuntimeError(f"home: nexus {' '.join(args)} 失败（{out.returncode}）：{out.stderr}")
        return json.loads(out.stdout) if out.stdout.strip() else {}

    def as_app(self, script):
        """以 shell 账号（普通程序，受 VPN 管理）运行一段命令。"""
        # exec-out：不经伪终端，换行不被改写。
        return self.adb("exec-out", f"su shell sh -c \"{script}\"", check=False, timeout=30)

    def http_on_phone(self, host, port=80):
        """以普通程序（nc）发一个 HTTP 请求，返回正文。请求先写成文件，免得在几层 shell 里转义。"""
        req = f"/data/local/tmp/req-{host}"
        self.adb("shell", f"printf 'GET / HTTP/1.0\\r\\nHost: {host}\\r\\n\\r\\n' > {req}; chmod 644 {req}")
        # 发完请求后不立刻关闭写方向：协议栈（ipstack）收到对方的 FIN 就整条关掉，不支持半关闭，
        # 那样回应送不回来。浏览器和常见的 HTTP 客户端不会半关闭；这是已知的限制，见 docs/feature-points/M3.6.md。
        out = self.as_app(f"(cat {req}; sleep 3) | nc -w 10 {host} {port}")
        out = out.replace("\r\n", "\n")
        return out.split("\n\n", 1)[1] if "\n\n" in out else out

    # ------------------------------------------------------------ 准备
    def setup(self):
        self.adb("root", check=False)
        time.sleep(2)
        subprocess.run(["adb", "connect", self.a.serial], capture_output=True)
        # redroid 里没有 /dev/tun（真机的 ueventd 会建它），系统的 VPN 要用它建网卡。
        self.adb("shell", "[ -e /dev/tun ] || { mknod /dev/tun c 10 200 && chmod 666 /dev/tun && chcon u:object_r:tun_device:s0 /dev/tun; }",
                 check=False)
        self.adb("shell", "am", "force-stop", PKG, check=False)
        self.adb("uninstall", PKG, check=False)
        self.adb("install", "-r", "-g", self.a.apk, timeout=180)
        # 测试里不弹 VPN 授权对话框：直接给应用 VPN 授权（等同于用户点了「确定」）。
        self.adb("shell", "appops", "set", PKG, "ACTIVATE_VPN", "allow")
        self.adb("push", self.a.cli, PHONE_CLI)
        self.adb("shell", "chmod", "755", PHONE_CLI)

        # home：本机的常驻程序。
        log = open(os.path.join(self.tmp, "home.log"), "w")
        self.daemon = subprocess.Popen([self.a.home_bin, "daemon", "--name", "home", "--relay", "disabled", "--no-discovery"],
                                       env=self.home_env, stdout=log, stderr=log)
        sock = os.path.join(self.home_env["NEXUS_DATA_DIR"], "ctl.sock")
        deadline = time.time() + 20
        while not os.path.exists(sock):
            if time.time() > deadline:
                raise RuntimeError("home 没有就绪")
            time.sleep(0.05)
        self.home("init", "--name", "home")
        self.web = serve(("127.0.0.1", 0), "hello from home")
        self.home("publish", "web", "--port", str(self.web.server_port), "--proto", "http")
        self.home("exit", "on")
        # far.test：只有 home 能按名字找到的网站。
        self.far = serve(("0.0.0.0", 0), "far away site")
        subprocess.run(["sudo", "-n", "sh", "-c", f"echo '{self.a.far_ip} far.test {HOSTS_MARK}' >> /etc/hosts"], check=True)

        # phone：启动应用（建 VPN、启动核心），加入网络。
        self.adb("shell", "am", "start", "-n", f"{PKG}/.MainActivity",
                 "--es", "name", "phone", "--es", "relay", "disabled", "--ez", "discovery", "false")
        deadline = time.time() + 30
        while "ctl.sock" not in self.adb("shell", "ls", APP_DIR, check=False):
            if time.time() > deadline:
                raise RuntimeError("phone 的核心没有就绪：" + self.adb("shell", "cat", f"{APP_DIR}/daemon.log", check=False)
                                   + self.adb("logcat", "-d", "-s", "nexus", "AndroidRuntime", check=False)[-3000:])
            time.sleep(0.5)
        code = self.phone("join")["code"]
        self.home("device", "approve", code, "--name", "phone")
        deadline = time.time() + 30
        while self.phone("status", "home", ok=False).get("online") is not True:
            if time.time() > deadline:
                raise RuntimeError("phone 连不上 home")
            time.sleep(0.5)
        self.phone("route", "add", "--domain", "far.test", "--via", "home")

    def teardown(self):
        self.adb("shell", "am", "start", "-n", f"{PKG}/.MainActivity", "--es", "action", "stop", check=False)
        self.adb("shell", "am", "force-stop", PKG, check=False)
        if self.daemon:
            self.daemon.send_signal(signal.SIGTERM)
            try:
                self.daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.daemon.kill()
        subprocess.run(["sudo", "-n", "sed", "-i", f"/{HOSTS_MARK}/d", "/etc/hosts"], check=False)
        shutil.rmtree(self.tmp, ignore_errors=True)

    # ------------------------------------------------------------ 检查
    def run(self):
        failures = []

        def check(what, ok, detail=""):
            print(("通过  " if ok else "失败  ") + what + ("" if ok else f"：{detail}"))
            if not ok:
                failures.append(what)

        tun = self.phone("status").get("tun")
        check("phone 是完整节点（VPN 网卡）", bool(tun), json.dumps(tun))

        body = self.http_on_phone("web.home.internal")
        check("内网访问：web.home.internal 到 home 上的服务", body.strip() == "hello from home", body[-300:])

        before = len(self.home("audit")["events"])
        body = self.http_on_phone("far.test", self.far.server_port)
        check("出口：far.test 经 home 出去", body.strip() == "far away site", body[-300:])
        events = self.home("audit")["events"][before:]
        check("home 的审计里有 net.exit",
              any(e.get("capability") == "net.exit" and e.get("outcome") == "allow" for e in events), json.dumps(events)[-500:])

        out = self.as_app("ping -c 1 -W 2 localhost")
        check("其余名字照常解析，不经过 Nexus", "127.0.0.1" in out and "198.18." not in out, out[-300:])
        return failures


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--serial", default="127.0.0.1:5555")
    ap.add_argument("--apk", default=os.path.join(ROOT, "target/android/nexus.apk"))
    ap.add_argument("--cli", default=os.path.join(ROOT, "target/android/x86_64/nexus"))
    ap.add_argument("--home-bin", default=os.path.join(ROOT, "target/debug/nexus"))
    ap.add_argument("--far-ip", default="172.17.0.1", help="far.test 的地址：本机的一个非本机回环地址")
    ap.add_argument("--keep", action="store_true", help="结束后不清理（调试用）")
    a = ap.parse_args()
    if not shutil.which("adb"):
        print("没有 adb")
        return 1
    e = E2E(a)
    try:
        e.setup()
        failures = e.run()
    finally:
        if a.keep:
            print(f"保留现场：home 的数据目录 {e.home_env['NEXUS_DATA_DIR']}")
        else:
            e.teardown()
    print("全部通过" if not failures else f"{len(failures)} 项失败")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
