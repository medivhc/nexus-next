#!/usr/bin/env python3
"""在 macOS 和 Windows 上代替真机的检查（M11.3，docs/feature-points/M11.3.md 的 FP-11.3-02 到 05）。

用户没有 macOS 和 Windows 设备，所以 CI 在 GitHub 的 macOS、Windows runner 上用编译出的 nexus 跑这个脚本：
本机起两个常驻程序（laptop、home，--local-only），走一遍组网、exec、服务、工作区、终端会话在常驻程序重启后
还在、升级与换回；加 --tun 时（要管理员权限）laptop 建虚拟网卡，检查名字服务、按名字访问服务和系统的名字解析。
Linux 上也能跑（不加 --tun），用来检查脚本本身。

用法：tools/platform_check.py --bin target/release/nexus[.exe] [--tun] [--keep]
"""
# covers: R81, R82, R83, R84
import argparse
import json
import os
import random
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import zipfile

WINDOWS = sys.platform == "win32"
MACOS = sys.platform == "darwin"
EXE = "nexus.exe" if WINDOWS else "nexus"


def step(msg):
    print(f"── {msg}", flush=True)


class Dev:
    def __init__(self, tmp, name, binary):
        self.name = name
        self.home = os.path.join(tmp, name)
        self.data = os.path.join(self.home, "data")
        os.makedirs(self.data)
        self.bin = os.path.abspath(binary)
        self.args = []
        self.proc = None

    def own_binary(self):
        """用自己的一份二进制（升级会换掉它）。wintun.dll 一起复制。"""
        d = os.path.join(self.home, "bin")
        os.makedirs(d)
        src = self.bin
        self.bin = os.path.join(d, EXE)
        shutil.copy2(src, self.bin)
        dll = os.path.join(os.path.dirname(src), "wintun.dll")
        if os.path.exists(dll):
            shutil.copy2(dll, d)

    def env(self):
        e = dict(os.environ)
        e.update(HOME=self.home, USERPROFILE=self.home, NEXUS_DATA_DIR=self.data)
        e.pop("NEXUS_AGENT", None)
        return e

    def start(self, *extra):
        self.args = list(extra)
        sock = os.path.join(self.data, "ctl.sock")
        if os.path.exists(sock):
            os.remove(sock)
        log = open(os.path.join(self.home, "daemon.log"), "ab")
        kw = {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP} if WINDOWS else {"start_new_session": True}
        self.proc = subprocess.Popen(
            [self.bin, "daemon", "--name", self.name, "--local-only", "--rendezvous", "disabled", *self.args],
            env=self.env(), cwd=self.home, stdin=subprocess.DEVNULL, stdout=log, stderr=log, **kw,
        )
        deadline = time.time() + 30
        while True:
            if os.path.exists(sock) and self.run("status", check=False).returncode == 0:
                return
            if self.proc.poll() is not None or time.time() > deadline:
                raise SystemExit(f"{self.name}：常驻程序没能启动\n{self.log()}")
            time.sleep(0.1)

    def log(self):
        try:
            with open(os.path.join(self.home, "daemon.log"), errors="replace") as f:
                return f.read()[-4000:]
        except OSError:
            return ""

    def run(self, *args, json_out=True, stdin=None, check=True, timeout=60):
        argv = [self.bin, *args] + (["--json"] if json_out else [])
        p = subprocess.run(argv, env=self.env(), cwd=self.home, input=stdin, capture_output=True, text=True,
                           encoding="utf-8", errors="replace", timeout=timeout)
        if check and p.returncode != 0:
            raise SystemExit(f"{self.name}$ nexus {' '.join(args)} 退出码 {p.returncode}\n{p.stdout}{p.stderr}")
        if json_out and check:
            return json.loads(p.stdout)
        return p

    def stop(self):
        """结束常驻程序（不是正常退出）。Windows 上升级后进程号变了，按二进制的路径找。"""
        if self.proc and self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait(timeout=10)
        if WINDOWS:
            kill_under(os.path.dirname(self.bin), daemon_only=True)


def kill_under(prefix, daemon_only=False):
    """Windows：结束路径在 prefix 下的 nexus 进程（升级后重新启动的常驻程序、会话的持有进程）。"""
    if not WINDOWS:
        return
    cond = f"$_.Path -and $_.Path.StartsWith('{prefix}', 'OrdinalIgnoreCase')"
    if daemon_only:
        cond += " -and ((Get-CimInstance Win32_Process -Filter \"ProcessId=$($_.Id)\").CommandLine -notmatch 'session-hold')"
    subprocess.run(["powershell", "-NoProfile", "-Command",
                    f"Get-Process | Where-Object {{ {cond} }} | Stop-Process -Force -ErrorAction SilentlyContinue"],
                   capture_output=True)


def wait(what, fn, within=30, every=0.25):
    deadline = time.time() + within
    last = None
    while time.time() < deadline:
        try:
            last = fn()
            if last:
                return last
        except Exception as e:  # noqa: BLE001 — 记下最后一次的错误
            last = e
        time.sleep(every)
    raise SystemExit(f"等不到：{what}（最后一次：{last!r}）")


def py_print(text):
    """在任何 shell（sh、cmd、PowerShell）里都能运行、输出 text 的命令；回显的命令本身不含完整的 text。"""
    a, b = text[: len(text) // 2], text[len(text) // 2:]
    exe = sys.executable
    if " " in exe:
        exe = "python"
    return f"{exe} -c \"print('{a}'+'{b}')\"\r"


# ---------------------------------------------------------------- FP-11.3-02

def network(a, b):
    step("组网：init、join、approve")
    a.run("init")
    code = b.run("join")["code"]
    a.run("device", "approve", code, "--name", "home")
    for d in (a, b):
        def other_online(d=d):
            ls = d.run("ls")
            other = [x for x in ls["devices"] if not x["self"]]
            return other and other[0]["online"]
        wait(f"{d.name} 看到对方在线", other_online)
    print("   两边 ls 都看到对方在线")


def daily(a, b):
    step("日常操作：status、exec、服务、工作区")
    for d, peer in ((a, "home"), (b, "laptop")):
        assert d.run("status", peer)["online"]
        out = d.run("exec", peer, "--", sys.executable, "-c", "print('hi')", json_out=False)
        assert out.stdout.strip() == "hi", out.stdout + out.stderr
    print("   status、exec：两个方向都通")
    port = free_port()
    srv = subprocess.Popen([sys.executable, "-m", "http.server", str(port), "--bind", "127.0.0.1"],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, cwd=b.home)
    try:
        wait("本机 HTTP 服务", lambda: socket.create_connection(("127.0.0.1", port), timeout=1).close() or True)
        b.run("publish", "files", "--port", str(port), "--proto", "http")
        code = ("import os,urllib.request;"
                "print(urllib.request.urlopen('http://'+os.environ['NEXUS_SVC_HOME_FILES']).status)")
        got = a.run("run", "--use", "home/files", "--", sys.executable, "-c", code, json_out=False)
        assert got.stdout.strip() == "200", got.stdout + got.stderr
    finally:
        srv.terminate()
    print("   发布和使用服务：通")
    b.run("drive", "serve", "on")
    a.run("drive", "create", "notes")
    a.run("drive", "put", "notes:/todo.txt", json_out=False, stdin="买牛奶\n")
    a.run("drive", "commit", "notes", "-m", "第一条")
    b.run("drive", "sync", "notes")
    got = b.run("drive", "cat", "notes:/todo.txt", json_out=False)
    assert got.stdout.replace("\r\n", "\n") == "买牛奶\n", repr(got.stdout)
    print("   工作区同步：通")


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


# ---------------------------------------------------------------- FP-11.3-04

class Term:
    """laptop 上的 nexus ssh home，标准输入输出是管道。"""

    def __init__(self, dev, *args):
        self.p = subprocess.Popen([dev.bin, "ssh", *args], env=dev.env(), cwd=dev.home,
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.buf = bytearray()
        self.lock = threading.Lock()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        while True:
            chunk = self.p.stdout.read1(65536)
            if not chunk:
                return
            with self.lock:
                self.buf += chunk

    def send(self, text):
        self.p.stdin.write(text.encode())
        self.p.stdin.flush()

    def text(self):
        with self.lock:
            return self.buf.decode("utf-8", errors="replace")

    def expect(self, marker, within=30):
        wait(f"终端里出现 {marker}", lambda: marker in self.text(), within=within)


def sessions(a, b):
    step("终端会话在常驻程序重启后还在")
    t = Term(a, "home")
    time.sleep(2)
    t.send(py_print("before-restart"))
    try:
        t.expect("before-restart", within=40)
    except SystemExit:
        print(f"终端输出：{t.text()[-2000:]!r}")
        raise
    created = a.run("session", "ls", "home")["sessions"][0]["created_at_ms"]
    b.stop()
    b.start(*b.args)
    wait("会话还在", lambda: [s for s in a.run("session", "ls", "home")["sessions"] if s["name"] == "main"])
    ls = a.run("session", "ls", "home")
    assert ls["sessions"][0]["created_at_ms"] == created, ls
    t.send(py_print("after-restart"))
    try:
        t.expect("after-restart", within=40)
    except SystemExit:
        print(f"终端输出：{t.text()[-2000:]!r}\n{b.log()}")
        raise
    print("   重启后会话还在，created_at 不变，输入照常")
    a.run("session", "kill", "home", "main")
    try:
        t.p.wait(timeout=30)
    except subprocess.TimeoutExpired:
        t.p.kill()
    wait("持有进程删掉端点", lambda: not [f for f in os.listdir(os.path.join(b.data, "sessions")) if f.endswith(".sock")])
    print("   结束会话：持有进程退出，端点删掉")


# ---------------------------------------------------------------- FP-11.3-05

def upgrade_state(d, want):
    """`upgrade --status` 是 want 时返回它；常驻程序正在重新启动时（连不上）返回 None。"""
    p = d.run("upgrade", "--status", check=False)
    if p.returncode != 0:
        return None
    v = json.loads(p.stdout)
    return v if v["state"] == want else None


def started_at(d):
    p = d.run("status", check=False)
    return json.loads(p.stdout)["started_at_ms"] if p.returncode == 0 else None

def upgrade(b, tmp):
    step("升级与换回")
    started = b.run("status")["started_at_ms"]
    original = open(b.bin, "rb").read()
    # 不是 nexus 的文件：107，不重启
    bad = os.path.join(tmp, "not-nexus.zip")
    with zipfile.ZipFile(bad, "w") as z:
        z.writestr("readme.txt", "x")
    p = b.run("upgrade", "--file", bad, check=False)
    assert p.returncode == 107, (p.returncode, p.stdout, p.stderr)
    assert b.run("status")["started_at_ms"] == started
    # 同一版本打成发布包的样子
    pkg = os.path.join(tmp, "nexus-new.zip")
    with zipfile.ZipFile(pkg, "w", zipfile.ZIP_DEFLATED) as z:
        z.write(b.bin, f"nexus-new/{EXE}")
    r = b.run("upgrade", "--file", pkg)
    assert r["state"] == "pending", r
    st = wait("升级确认", lambda: upgrade_state(b, "confirmed"), within=60, every=0.5)
    assert b.run("status")["started_at_ms"] != started, "常驻程序应当重新启动过"
    prev = os.path.join(os.path.dirname(b.bin), "nexus.prev.exe" if WINDOWS else "nexus.prev")
    assert os.path.exists(prev), os.listdir(os.path.dirname(b.bin))
    print(f"   换上并确认：{st['message']}；原来的留作 {os.path.basename(prev)}")
    started = b.run("status")["started_at_ms"]
    b.run("upgrade", "--rollback")
    wait("换回后重新启动", lambda: started_at(b) not in (None, started), within=60, every=0.5)
    st = b.run("upgrade", "--status")
    assert st["state"] == "rolled_back", st
    assert open(b.bin, "rb").read() == original, "换回后的二进制应当和升级前逐字节相同"
    print("   手动换回：rolled_back，二进制和升级前相同")


# ---------------------------------------------------------------- FP-11.3-03

def dns_query(server, name):
    qid = random.randrange(65536)
    q = struct.pack(">HHHHHH", qid, 0x0100, 1, 0, 0, 0)
    for part in name.split("."):
        q += bytes([len(part)]) + part.encode()
    q += b"\0" + struct.pack(">HH", 1, 1)
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(3)
    try:
        s.sendto(q, (server, 53))
        resp, _ = s.recvfrom(1500)
    finally:
        s.close()
    rcode = resp[3] & 0x0F
    an = struct.unpack(">H", resp[6:8])[0]
    if rcode or not an:
        return rcode, []
    i = len(q)
    out = []
    for _ in range(an):
        if resp[i] & 0xC0 == 0xC0:
            i += 2
        else:
            while resp[i]:
                i += resp[i] + 1
            i += 1
        rtype, _, _, rdlen = struct.unpack(">HHIH", resp[i:i + 10])
        i += 10
        if rtype == 1 and rdlen == 4:
            out.append(socket.inet_ntoa(resp[i:i + 4]))
        i += rdlen
    return 0, out


def http_get(ip, port=80):
    s = socket.create_connection((ip, port), timeout=10)
    try:
        s.sendall(b"GET / HTTP/1.0\r\nHost: web.home.internal\r\n\r\n")
        data = b""
        while True:
            chunk = s.recv(65536)
            if not chunk:
                break
            data += chunk
    finally:
        s.close()
    return data.decode("utf-8", errors="replace")


def nrpt_count():
    p = subprocess.run(["powershell", "-NoProfile", "-Command",
                        "@(Get-DnsClientNrptRule | Where-Object { $_.Comment -eq 'nexus' }).Count"],
                       capture_output=True, text=True)
    return int(p.stdout.strip() or "0")


def tun(a, b):
    step("虚拟网卡（完整节点）")
    port = free_port()
    page = os.path.join(b.home, "page")
    os.makedirs(page)
    with open(os.path.join(page, "index.html"), "w") as f:
        f.write("hello from home")
    srv = subprocess.Popen([sys.executable, "-m", "http.server", str(port), "--bind", "127.0.0.1", "--directory", page],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        b.run("publish", "web", "--port", str(port), "--proto", "http")
        a.stop()
        a.start("--tun", "nexus0")
        wait("laptop 看到 home 在线", lambda: [x for x in a.run("ls")["devices"] if not x["self"] and x["online"]])
        st = a.run("status")
        t = st.get("tun")
        assert t, st
        if MACOS:
            assert t["name"].startswith("utun"), t
        dns = t["dns"]
        print(f"   网卡 {t['name']}，网段 {t['net']}，名字服务 {dns}")
        _, ips = wait("名字服务回答 web.home.internal", lambda: (lambda r: r if r[1] else None)(dns_query(dns, "web.home.internal")))
        ip = ips[0]
        assert dns_query(dns, "web.nowhere.internal")[0] == 3, "没有这台设备应当是 NXDOMAIN"
        wait("经虚拟网卡访问服务", lambda: "hello from home" in http_get(ip))
        print(f"   web.home.internal -> {ip}，不设代理访问 80 端口：拿到服务的回应")
        sys_ip = wait("系统的名字解析", lambda: socket.getaddrinfo("web.home.internal", 80, socket.AF_INET)[0][4][0], within=30, every=1)
        assert sys_ip == ip, (sys_ip, ip)
        print(f"   系统的名字解析也得到 {sys_ip}")
        if WINDOWS:
            assert nrpt_count() == 1, nrpt_count()
            # 被结束（规则留下了）再启动：还是只有一条
            a.stop()
            assert nrpt_count() == 1
            a.start("--tun", "nexus0")
            assert nrpt_count() == 1, nrpt_count()
            print("   NRPT 规则：重新启动后还是一条")
    finally:
        srv.terminate()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--tun", action="store_true", help="也检查虚拟网卡（要管理员权限）")
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()
    v = subprocess.run([args.bin, "--version"], capture_output=True, text=True)
    print(f"{v.stdout.strip()} on {sys.platform}")
    tmp = os.path.realpath(tempfile.mkdtemp(prefix="nexus-platform-"))
    a = Dev(tmp, "laptop", args.bin)
    b = Dev(tmp, "home", args.bin)
    b.own_binary()
    ok = False
    try:
        a.start()
        b.start()
        network(a, b)
        daily(a, b)
        sessions(a, b)
        upgrade(b, tmp)
        if args.tun:
            tun(a, b)
        ok = True
    finally:
        if not ok:
            for d in (a, b):
                print(f"\n=== {d.name} 的日志 ===\n{d.log()}")
        for d in (a, b):
            try:
                d.stop()
            except Exception as e:  # noqa: BLE001
                print(f"停不了 {d.name}：{e}")
        kill_under(tmp)
        if WINDOWS and args.tun:
            subprocess.run(["powershell", "-NoProfile", "-Command",
                            "Get-DnsClientNrptRule | Where-Object { $_.Comment -eq 'nexus' } | Remove-DnsClientNrptRule -Force"],
                           capture_output=True)
        if not args.keep:
            shutil.rmtree(tmp, ignore_errors=True)
    print("\n平台检查通过")


if __name__ == "__main__":
    main()
