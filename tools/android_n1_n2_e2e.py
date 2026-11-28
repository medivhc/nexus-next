#!/usr/bin/env python3
"""N1/N2 Android 端到端实测（redroid / 真机）：覆盖 U01、U02、U06、U07、U10、U12。

验证内容：
  1. N2 默认启动为 balanced 省电模式（不主动请求蜂窝），支持热切换 realtime <-> balanced
  2. U01/N1 通用公网 UDP 经出口节点转发（Request::ExitDatagram），且 home 审计记录 net.exit
  3. U07/U12 Clash 规则子集导入（DOMAIN 精确匹配 + REJECT 拦截 + 忽略 SCRIPT）、静态审计与 LKG 一键回滚
  4. U02 Android app_bypass.txt 应用绕过 VPN 真实流量生效（含防环路自排除）
  5. U10/N2 5 秒内快速停止 VPN 回退

用法：python3 tools/android_n1_n2_e2e.py [--serial emulator-5554]
"""
# covers: R41, R42, R43, R46, R87, R91, R101, R102, R103, R104
import argparse
import json
import os
import shutil
import socket
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
HOSTS_MARK = "# nexus-android-n1-n2-e2e"


def serve_http(addr, body):
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


def serve_udp_echo(addr):
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(addr)

    def loop():
        while True:
            try:
                data, peer = sock.recvfrom(2048)
            except OSError:
                return
            sock.sendto(b"echo:" + data, peer)

    threading.Thread(target=loop, daemon=True).start()
    return sock


def host_non_loopback_ip():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("8.8.8.8", 80))
        return s.getsockname()[0]
    finally:
        s.close()


class Runner:
    def __init__(self, args):
        self.a = args
        self.tmp = tempfile.mkdtemp(prefix="nexus-n1n2-e2e-")
        self.home_dir = os.path.join(self.tmp, "home")
        os.makedirs(self.home_dir, exist_ok=True)
        self.daemon = None
        self.servers = []
        self.hosts_added = False

    def adb(self, *args, check=True, timeout=60):
        cmd = ["adb", "-s", self.a.serial, *args]
        for attempt in range(3):
            out = subprocess.run(cmd, text=True, capture_output=True, timeout=timeout)
            if out.returncode == 0 or not check:
                return out.stdout.strip()
            err = out.stderr or out.stdout
            if attempt < 2 and ("offline" in err or "not found" in err):
                subprocess.run(["adb", "-s", self.a.serial, "wait-for-device"], timeout=15, capture_output=True)
                time.sleep(1)
                continue
            raise RuntimeError(f"adb {' '.join(args)} 失败：{err}")
        return ""

    def home(self, *args, parse_json=True):
        cmd = [self.a.home_bin, "--data-dir", self.home_dir, *args]
        if parse_json:
            cmd.append("--json")
        out = subprocess.run(cmd, text=True, capture_output=True, timeout=30)
        if out.returncode != 0:
            raise RuntimeError(f"home {' '.join(args)} 失败：{out.stderr or out.stdout}")
        return json.loads(out.stdout) if parse_json else out.stdout

    def phone(self, *args, parse_json=True):
        quoted = " ".join(f"'{x}'" for x in args)
        suffix = " --json" if parse_json else ""
        out = self.adb("shell", f"su 0 sh -c \"NEXUS_DATA_DIR={APP_DIR} {PHONE_CLI} {quoted}{suffix}\"")
        return json.loads(out) if parse_json else out

    def shell_nc_http(self, host, port):
        # 以普通 shell 账号（com.android.shell）发 HTTP 请求
        cmd = f"su 2000 sh -c \"timeout 6 sh -c 'printf \\\"GET / HTTP/1.0\\r\\nHost: {host}\\r\\n\\r\\n\\\" | nc -w 4 {host} {port}'\""
        return self.adb("shell", cmd, check=False)

    def shell_nc_udp(self, host, port, payload):
        cmd = f"su 2000 sh -c \"echo {payload} | nc -u -q 3 {host} {port}\""
        return self.adb("shell", cmd, check=False)

    def add_hosts(self, ip, names):
        lines = "\n".join(f"{ip} {n} {HOSTS_MARK}" for n in names) + "\n"
        subprocess.run(
            ["sudo", "tee", "-a", "/etc/hosts"],
            input=lines,
            text=True,
            capture_output=True,
            check=True,
        )
        self.hosts_added = True

    def clean_hosts(self):
        if self.hosts_added:
            subprocess.run(
                ["sudo", "sed", "-i", f"/{HOSTS_MARK}/d", "/etc/hosts"],
                capture_output=True,
            )
            self.hosts_added = False

    def start_phone_vpn(self):
        self.adb("wait-for-device")
        self.adb("shell", "am", "start", "-f", "0x20000000", "-n", f"{PKG}/.MainActivity", "--es", "action", "stop", check=False)
        time.sleep(1.5)
        self.adb("logcat", "-c")
        self.adb("shell", "appops", "set", PKG, "ACTIVATE_VPN", "allow", check=False)
        self.adb("shell", "test -c /dev/tun || (mknod /dev/tun c 10 200 && chmod 666 /dev/tun)", check=False)
        self.adb(
            "shell", "am", "start", "-S", "-n", f"{PKG}/.MainActivity",
            "--es", "name", "phone", "--es", "relay", "disabled", "--ez", "discovery", "false",
        )
        deadline = time.time() + 25
        while time.time() < deadline:
            ls = self.adb("shell", f"su 0 ls {APP_DIR}", check=False)
            if "ctl.sock" in ls:
                time.sleep(1)
                return
            time.sleep(0.5)
        raise RuntimeError("等待 phone ctl.sock 超时")

    def wait_home_online(self, timeout=20):
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                st = self.phone("status", "home")
                if st.get("online"):
                    return
            except Exception:
                pass
            time.sleep(0.5)

    def setup(self):
        self.adb("wait-for-device")
        if not self.a.skip_install:
            self.adb("install", "-r", "--no-incremental", self.a.apk, timeout=180)
        self.adb("push", self.a.cli, PHONE_CLI)
        self.adb("shell", "chmod", "+x", PHONE_CLI)
        self.adb("shell", "am", "force-stop", PKG, check=False)
        time.sleep(1)
        self.adb("shell", f"su 0 rm -rf {APP_DIR}/*", check=False)

        # 启动 home 守护进程
        self.daemon = subprocess.Popen(
            [self.a.home_bin, "--data-dir", self.home_dir, "daemon", "--name", "home", "--relay", "disabled", "--no-discovery"],
            stdout=subprocess.DEVNULL,
            stderr=open(os.path.join(self.tmp, "home.log"), "w"),
        )
        deadline = time.time() + 20
        while not os.path.exists(os.path.join(self.home_dir, "ctl.sock")):
            if time.time() > deadline:
                raise RuntimeError("home daemon 启动超时")
            time.sleep(0.2)
        self.home("init", "--name", "home")
        self.home("exit", "on")

        # 启动宿主本地 HTTP 与非回环公网模拟服务（TCP + UDP）
        local_http = serve_http(("127.0.0.1", 0), "hello-internal-web")
        self.servers.append(local_http)
        self.web_port = local_http.server_address[1]
        self.home("publish", "web", "--port", str(self.web_port), "--proto", "http")

        self.pub_ip = host_non_loopback_ip()
        far_http = serve_http((self.pub_ip, 0), "hello-far-http")
        self.servers.append(far_http)
        self.far_http_port = far_http.server_address[1]

        far_udp = serve_udp_echo((self.pub_ip, 0))
        self.servers.append(far_udp)
        self.far_udp_port = far_udp.getsockname()[1]

        self.add_hosts(self.pub_ip, ["udp-far.test", "exact-far.test", "blocked-ad.test"])

        # 启动 phone VPN 并加入网络
        self.start_phone_vpn()
        join = self.phone("join")
        self.home("device", "approve", join["code"], "--name", "phone")
        self.wait_home_online()

    def check(self, cond, label, detail=""):
        if cond:
            print(f"通过  {label}")
        else:
            print(f"失败  {label}：{detail}")
            self.failures += 1

    def run(self):
        self.failures = 0

        # 1. N2/U10：默认启动为 balanced 省电模式，不主动请求蜂窝
        log = self.adb("logcat", "-d", "-s", "nexus:V")
        self.check(
            "电源模式：balanced（不主动保持蜂窝）" in log,
            "U10/N2 默认 balanced 省电模式（不主动保持蜂窝）",
            log[-400:],
        )

        # 2. U01/N1：通用公网 UDP 经 home 出口转发（Request::ExitDatagram）
        self.phone("route", "add", "--domain", "udp-far.test", "--via", "home")
        udp_resp = self.shell_nc_udp("udp-far.test", self.far_udp_port, "hello-udp-exit")
        self.check(
            "echo:hello-udp-exit" in udp_resp,
            "U01/N1 通用公网 UDP 经 home 出口往返成功（ExitDatagram）",
            repr(udp_resp),
        )
        audit = self.home("audit")
        has_exit_audit = any(
            e.get("capability") == "net.exit" and e.get("resource") == "home/exit"
            for e in audit.get("events", [])
        )
        self.check(has_exit_audit, "U01/N1 home 审计日志记录 net.exit", str(audit))

        # 3. U07/U12/N1：Clash 规则子集导入（精确域名 + 广告拦截 + 跳过 SCRIPT）、审计与 LKG 一键回滚
        clash_yaml = (
            "rules:\n"
            "  - DOMAIN,exact-far.test,PROXY\n"
            "  - DOMAIN-SUFFIX,blocked-ad.test,REJECT\n"
            "  - SCRIPT,unsafe,PROXY\n"
        )
        remote_yaml = "/data/local/tmp/clash-n1.yaml"
        self.adb("shell", f"cat << 'EOF' > {remote_yaml}\n{clash_yaml}EOF")

        dry = self.phone("route", "import", remote_yaml, "--via", "home", "--dry-run")
        self.check(
            dry.get("imported") == 2 and len(dry.get("skipped", [])) == 1,
            "U07/N1 Clash 规则 dry-run 审计报告准确（导入 2 条、跳过 SCRIPT 1 条）",
            str(dry),
        )

        imp = self.phone("route", "import", remote_yaml, "--via", "home", "--replace")
        self.check(imp.get("total") == 2, "U07/N1 Clash 规则原子替换导入生效", str(imp))

        exact_resp = self.shell_nc_http("exact-far.test", self.far_http_port)
        self.check("hello-far-http" in exact_resp, "U07/N1 导入的精确域名经 home 出口访问成功", repr(exact_resp))

        blocked_resp = self.shell_nc_http("blocked-ad.test", self.far_http_port)
        self.check("hello-far-http" not in blocked_resp, "U07/N1 导入的 REJECT 广告域名被成功拦截", repr(blocked_resp))

        route_audit = self.phone("route", "audit")
        self.check(route_audit.get("clean") is True, "U07/N1 规则静态审计 clean=true", str(route_audit))

        rb = self.phone("route", "rollback")
        self.check(rb.get("rolled_back") is True, "U12/N1 LKG 一键回滚成功", str(rb))
        rules_after = self.phone("route", "ls")["rules"]
        self.check(
            len(rules_after) == 1 and rules_after[0]["match"] == "udp-far.test",
            "U12/N1 回滚后完整恢复导入前的规则表",
            str(rules_after),
        )

        # 4. U02/N1：Android app_bypass.txt 绕过 VPN（以 com.android.shell 验证真实绕过，且自身防环路）
        before_bypass = self.shell_nc_http("web.home.internal", 80)
        self.check("hello-internal-web" in before_bypass, "U06/U02 未绕过前 shell 正常经 VPN 访问 web.home.internal", repr(before_bypass))

        self.adb("shell", f"su 0 sh -c 'echo com.android.shell > {APP_DIR}/app_bypass.txt && chmod 644 {APP_DIR}/app_bypass.txt'")
        self.start_phone_vpn()
        after_bypass = self.shell_nc_http("web.home.internal", 80)
        self.check(
            "hello-internal-web" not in after_bypass,
            "U02/N1 加入 app_bypass.txt 后 com.android.shell 真实流量绕过 VPN TUN",
            repr(after_bypass),
        )
        # 清理绕过文件并验证恢复（重启 VPN 后等待核心重新连上 home）
        self.adb("shell", f"su 0 rm -f {APP_DIR}/app_bypass.txt")
        self.start_phone_vpn()
        self.wait_home_online()
        restored = ""
        for _ in range(5):
            restored = self.shell_nc_http("web.home.internal", 80)
            if "hello-internal-web" in restored:
                break
            time.sleep(1)
        self.check("hello-internal-web" in restored, "U02/N1 移除绕过后恢复正常访问内网服务", repr(restored))

        # 5. N2：热切换 realtime <-> balanced 与 5 秒内快速停止回退
        self.adb("shell", "am", "start", "-f", "0x20000000", "-n", f"{PKG}/.MainActivity", "--es", "power_mode", "realtime")
        time.sleep(1.5)
        self.adb("shell", "am", "start", "-f", "0x20000000", "-n", f"{PKG}/.MainActivity", "--es", "power_mode", "balanced")
        time.sleep(1.5)
        log2 = self.adb("logcat", "-d", "-s", "nexus:V")
        self.check(
            "电源模式：realtime（已请求保持蜂窝）" in log2 and log2.rfind("电源模式：balanced（不主动保持蜂窝）") > log2.rfind("电源模式：realtime"),
            "U10/N2 运行时热切换 realtime -> balanced 成功",
            log2[-400:],
        )

        self.adb("shell", "am", "start", "-f", "0x20000000", "-n", f"{PKG}/.MainActivity", "--es", "action", "stop")
        time.sleep(2.5)
        dlog = self.adb("shell", f"su 0 tail -n 5 {APP_DIR}/daemon.log", check=False)
        self.check("nexus: 已停止" in dlog, "U12/N2 快速停止 VPN 回退成功（<5 秒）", repr(dlog))

        print("全部通过" if self.failures == 0 else f"共 {self.failures} 项失败")
        return self.failures

    def cleanup(self):
        self.clean_hosts()
        self.adb("shell", "am", "start", "-f", "0x20000000", "-n", f"{PKG}/.MainActivity", "--es", "action", "stop", check=False)
        self.adb("shell", "am", "force-stop", PKG, check=False)
        if self.daemon:
            self.daemon.terminate()
            try:
                self.daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.daemon.kill()
        for s in self.servers:
            try:
                s.close()
            except Exception:
                pass
        shutil.rmtree(self.tmp, ignore_errors=True)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--serial", default="emulator-5554")
    p.add_argument("--apk", default=os.path.join(ROOT, "target/android/nexus.apk"))
    p.add_argument("--cli", default=os.path.join(ROOT, "target/android/x86_64/nexus"))
    p.add_argument("--home-bin", default=os.path.join(ROOT, "target/debug/nexus"))
    p.add_argument("--skip-install", action="store_true")
    args = p.parse_args()
    r = Runner(args)
    try:
        r.setup()
        return 0 if r.run() == 0 else 1
    finally:
        r.cleanup()


if __name__ == "__main__":
    sys.exit(main())
