#!/usr/bin/env python3
"""Android 应用内加入网络的实测（M11.2）：在 redroid（或接在 adb 上的真机）里装上 Nexus 应用，
只通过应用的界面加入网络，全程不在手机上运行命令行、不用 root 读应用的数据目录。

本机（运行脚本的 Linux）当管理设备 home。步骤：

  1. 装应用、启动（同意 VPN 授权）；界面（uiautomator 读出来）显示短加入码、长加入码和二维码
  2. 点「换一个加入码」：短码变了
  3. 在 home 上 `nexus device approve <界面上的短码>`
  4. 界面自动变成「已加入网络」，设备列表里有 home 和 phone

--no-rendezvous：手机不用碰头服务（没有短码），改用二维码里的文字批准（界面上二维码的说明文字，
有 zbarimg 时从截图里真的扫出来，并检查和说明文字一致）。

用法：tools/android_join_e2e.py [--serial 127.0.0.1:5555] [--apk target/android/nexus.apk]
      [--home-bin target/debug/nexus] [--no-rendezvous]
先运行 tools/build-android.sh 和 cargo build -p nexus-cli。
默认在本机起一个碰头服务（只存取记录；签名由两端自己检查），手机经 --host-ip（容器里看到的本机地址）连它；
--public-rendezvous 改用公共的碰头服务（dns.iroh.link）。
"""
# covers: R79, R80
import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import threading
import xml.etree.ElementTree as ET
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PKG = "link.bewith.nexus"
SHORT = re.compile(r"^[0-9A-HJKMNP-TV-Z]{5}-[0-9A-HJKMNP-TV-Z]{5}$")


def rendezvous_server():
    """最简单的碰头服务：PUT /pkarr/<公钥> 存，GET 取（没有时 404）。"""
    records = {}

    class H(BaseHTTPRequestHandler):
        def do_PUT(self):
            records[self.path] = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            self.send_response(200)
            self.send_header("Content-Length", "0")
            self.end_headers()

        def do_GET(self):
            body = records.get(self.path)
            self.send_response(200 if body is not None else 404)
            self.send_header("Content-Length", str(len(body or b"")))
            self.end_headers()
            self.wfile.write(body or b"")

        def log_message(self, *a):
            pass

    srv = ThreadingHTTPServer(("0.0.0.0", 0), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


class E2E:
    def __init__(self, a):
        self.a = a
        self.tmp = tempfile.mkdtemp(prefix="nexus-android-join-")
        self.home_env = dict(os.environ, NEXUS_DATA_DIR=os.path.join(self.tmp, "home"),
                             NEXUS_AGENT_SOCKET=os.path.join(self.tmp, "home", "agent.sock"))
        os.makedirs(self.home_env["NEXUS_DATA_DIR"], exist_ok=True)
        self.daemon = None

    def adb(self, *args, check=True, timeout=60):
        out = subprocess.run(["adb", "-s", self.a.serial, *args], capture_output=True, text=True, timeout=timeout)
        if check and out.returncode != 0:
            raise RuntimeError(f"adb {' '.join(args)}：{out.stderr or out.stdout}")
        return out.stdout

    def home(self, *args, ok=True):
        out = subprocess.run([self.a.home_bin, *args, "--json"], env=self.home_env, capture_output=True, text=True, timeout=60)
        if ok and out.returncode != 0:
            raise RuntimeError(f"home: nexus {' '.join(args)} 失败（{out.returncode}）：{out.stderr}")
        return out

    def ui(self):
        """界面上的控件：{content-desc: (text, bounds)}。"""
        self.adb("shell", "uiautomator", "dump", "/sdcard/nexus-ui.xml", check=False, timeout=30)
        xml = self.adb("shell", "cat", "/sdcard/nexus-ui.xml", check=False)
        views = {}
        try:
            for node in ET.fromstring(xml[xml.find("<"):]).iter("node"):
                desc = node.get("content-desc") or ""
                if desc:
                    views[desc] = (node.get("text") or "", node.get("bounds") or "")
        except ET.ParseError:
            pass
        return views

    def wait_ui(self, what, pred, limit=60):
        deadline = time.time() + limit
        views = {}
        while time.time() < deadline:
            views = self.ui()
            if pred(views):
                return views
            time.sleep(1)
        raise RuntimeError(f"等不到{what}；界面：{views}\n" + self.adb("logcat", "-d", "-s", "nexus", "AndroidRuntime", check=False)[-3000:])

    def tap(self, bounds):
        x1, y1, x2, y2 = map(int, re.findall(r"\d+", bounds))
        self.adb("shell", "input", "tap", str((x1 + x2) // 2), str((y1 + y2) // 2))

    def setup(self):
        self.adb("root", check=False)
        time.sleep(2)
        subprocess.run(["adb", "connect", self.a.serial], capture_output=True)
        # redroid 里没有 /dev/tun（真机的 ueventd 会建它），系统的 VPN 要用它建网卡。
        self.adb("shell", "[ -e /dev/tun ] || { mknod /dev/tun c 10 200 && chmod 666 /dev/tun && chcon u:object_r:tun_device:s0 /dev/tun; }",
                 check=False)
        self.adb("shell", "am", "force-stop", PKG, check=False)
        self.adb("uninstall", PKG, check=False)  # 数据目录一起清掉：手机是一台全新的、没加入过的设备
        self.adb("install", "-r", "-g", self.a.apk, timeout=180)
        self.adb("shell", "appops", "set", PKG, "ACTIVATE_VPN", "allow")
        log = open(os.path.join(self.tmp, "home.log"), "w")
        args = [self.a.home_bin, "daemon", "--name", "home", "--relay", "disabled"]
        phone_rv = "default"
        if self.a.no_rendezvous:
            args.append("--no-discovery")
            phone_rv = "disabled"
        elif not self.a.public_rendezvous:
            self.rv = rendezvous_server()
            port = self.rv.server_port
            args += ["--no-discovery", "--rendezvous", f"http://127.0.0.1:{port}/pkarr"]
            phone_rv = f"http://{self.a.host_ip}:{port}/pkarr"
        self.daemon = subprocess.Popen(args, env=self.home_env, stdout=log, stderr=log)
        sock = os.path.join(self.home_env["NEXUS_DATA_DIR"], "ctl.sock")
        deadline = time.time() + 20
        while not os.path.exists(sock):
            if time.time() > deadline:
                raise RuntimeError("home 没有就绪")
            time.sleep(0.05)
        self.home("init", "--name", "home")
        start = ["shell", "am", "start", "-n", f"{PKG}/.MainActivity", "--es", "name", "phone", "--es", "relay", "disabled",
                 "--es", "rendezvous", phone_rv]
        if not self.a.public_rendezvous:
            start += ["--ez", "discovery", "false"]
        self.adb(*start)

    def teardown(self):
        self.adb("shell", "am", "start", "-n", f"{PKG}/.MainActivity", "--es", "action", "stop", check=False)
        self.adb("shell", "am", "force-stop", PKG, check=False)
        if self.daemon:
            self.daemon.send_signal(signal.SIGTERM)
            try:
                self.daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.daemon.kill()
        shutil.rmtree(self.tmp, ignore_errors=True)

    def scan_screen(self):
        """有 zbarimg 时从截图里扫出二维码的文字。"""
        if not shutil.which("zbarimg"):
            return None
        png = os.path.join(self.tmp, "screen.png")
        with open(png, "wb") as f:
            f.write(subprocess.run(["adb", "-s", self.a.serial, "exec-out", "screencap", "-p"], capture_output=True, timeout=30).stdout)
        if self.a.shot:
            shutil.copy(png, self.a.shot)
        out = subprocess.run(["zbarimg", "--raw", "-q", png], capture_output=True, text=True)
        return out.stdout.strip() or None

    def run(self):
        failures = []

        def check(what, ok, detail=""):
            print(("通过  " if ok else "失败  ") + what + ("" if ok else f"：{detail}"))
            if not ok:
                failures.append(what)

        has_code = lambda v: v.get("long_code", ("", ""))[0].startswith("nxj2-")
        views = self.wait_ui("加入码", has_code)
        check("界面显示长加入码", has_code(views), views)
        # 二维码图片的说明文字就是它的内容。
        qr_text = next((d for d in views if d.startswith("NXJ2-")), "")
        check("界面显示二维码", bool(qr_text), views)
        check("二维码里是大写的长加入码", qr_text == views["long_code"][0].upper(), qr_text)
        scanned = self.scan_screen()
        if scanned is not None:
            check("从截图里扫出的二维码和说明一致", scanned == qr_text, scanned)
        else:
            print("跳过  从截图扫二维码（没有 zbarimg）")

        if self.a.no_rendezvous:
            check("没有短码时说明原因", "short_code" not in views and "碰头服务" in views.get("message", ("", ""))[0], views)
            code = (scanned or qr_text) + "\n"
        else:
            first = views.get("short_code", ("", ""))[0]
            check("界面显示短加入码", bool(SHORT.match(first)), views)
            self.tap(views["renew"][1])
            views = self.wait_ui("新的短加入码", lambda v: v.get("short_code", ("", ""))[0] not in ("", first))
            code = views["short_code"][0]
            check("换一个加入码之后短码变了", bool(SHORT.match(code)) and code != first, views)
            old = self.home("device", "approve", first, ok=False)
            check("旧短码失效", old.returncode == 109, old.stderr)
            check("屏幕上的命令就是要运行的", f"nexus device approve {code}" in views.get("message", ("", ""))[0], views)
        t0 = time.time()
        approved = self.home("device", "approve", code, "--name", "phone", ok=False)
        check("管理设备用界面上的加入码批准", approved.returncode == 0, approved.stderr)
        views = self.wait_ui("已加入", lambda v: v.get("title", ("", ""))[0] == "已加入网络", limit=30)
        took = time.time() - t0
        check(f"界面自动变成已加入（{took:.1f} 秒）", took < 10, took)
        devices = views.get("devices", ("", ""))[0]
        check("设备列表里有 home 和 phone", "home" in devices and "phone（本机）" in devices, devices)
        ls = json.loads(self.home("ls").stdout)
        check("home 的名册里有 phone", "phone" in [d["name"] for d in ls["devices"]], ls)
        if self.a.shot:
            with open(self.a.shot.replace(".png", "-joined.png"), "wb") as f:
                f.write(subprocess.run(["adb", "-s", self.a.serial, "exec-out", "screencap", "-p"], capture_output=True, timeout=30).stdout)
        return failures


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--serial", default="127.0.0.1:5555")
    ap.add_argument("--apk", default=os.path.join(ROOT, "target/android/nexus.apk"))
    ap.add_argument("--home-bin", default=os.path.join(ROOT, "target/debug/nexus"))
    ap.add_argument("--no-rendezvous", action="store_true", help="不用碰头服务：用二维码里的长加入码批准")
    ap.add_argument("--public-rendezvous", action="store_true", help="用公共的碰头服务，不在本机起一个")
    ap.add_argument("--host-ip", default="172.17.0.1", help="手机那边看到的本机地址（redroid 的 docker 网桥）")
    ap.add_argument("--shot", help="把加入码和已加入的截图存到这里（.png）")
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
        if not a.keep:
            e.teardown()
    print("全部通过" if not failures else f"{len(failures)} 项失败")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
