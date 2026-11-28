#!/usr/bin/env python3
"""Android 模拟器定向回归。用两个应用 UID 验证代理、绕过、失败关闭和切网。"""

import argparse
import os
from pathlib import Path
import re
import secrets
import shlex
import subprocess
import tempfile
import time
from urllib.parse import urlsplit
import xml.etree.ElementTree as ET
import zipfile

from android_proxy_fixtures import fixtures


ROOT = Path(__file__).resolve().parent.parent
NEXUS = "link.bewith.nexus"
APP = f"/data/data/{NEXUS}"
PREFS = f"{APP}/shared_prefs/vpn.xml"
PROXY = f"{NEXUS}.e2e.proxy"
BYPASS = f"{NEXUS}.e2e.bypass"
PROBE_ACTIVITY = f"{NEXUS}.probe.ProbeActivity"
GUEST_HOST = "10.0.2.2"
LOG_TAG = "NexusProxyE2E"


def command(*args, timeout=60, check=True):
    result = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
    if check and result.returncode:
        raise RuntimeError(f"{shlex.join(str(a) for a in args)} 失败：{result.stderr or result.stdout}")
    return result.stdout.strip()


def wait_for(description, ready, timeout=90):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = ready()
        if result:
            print(f"通过  {description}", flush=True)
            return result
        time.sleep(2)
    raise RuntimeError(f"等待超时：{description}")


def build_probes(sdk, work):
    platform = sdk / "platforms/android-34/android.jar"
    tools = sdk / "build-tools/34.0.0"
    classes = work / "classes"
    classes.mkdir()
    command("javac", "-source", "8", "-target", "8", "-bootclasspath", platform, "-d", classes,
            ROOT / "tools/android-proxy-probe/ProbeActivity.java")
    dex = work / "dex"
    dex.mkdir()
    command(tools / "d8", "--min-api", "29", "--lib", platform, "--output", dex, *sorted(classes.rglob("*.class")))
    key = work / "probe.keystore"
    command("keytool", "-genkeypair", "-keystore", key, "-storepass", "android", "-keypass", "android",
            "-alias", "probe", "-keyalg", "RSA", "-keysize", "2048", "-validity", "14", "-dname", "CN=Nexus E2E")
    for package in (PROXY, BYPASS):
        manifest = work / "AndroidManifest.xml"
        manifest.write_text(f'''<manifest xmlns:android="http://schemas.android.com/apk/res/android" package="{package}">
    <uses-permission android:name="android.permission.INTERNET" />
    <application android:allowBackup="false" android:usesCleartextTraffic="true">
        <activity android:name="{PROBE_ACTIVITY}" android:exported="true">
            <intent-filter>
                <action android:name="android.intent.action.MAIN" />
                <category android:name="android.intent.category.LAUNCHER" />
            </intent-filter>
        </activity>
    </application>
</manifest>
''', encoding="utf-8")
        unsigned = work / "unsigned.apk"
        command(tools / "aapt2", "link", "--manifest", manifest, "-I", platform, "--min-sdk-version", "29",
                "--target-sdk-version", "34", "-o", unsigned)
        with zipfile.ZipFile(unsigned, "a") as apk:
            apk.write(dex / "classes.dex", "classes.dex")
        aligned = work / "aligned.apk"
        command(tools / "zipalign", "-f", "4", unsigned, aligned)
        command(tools / "apksigner", "sign", "--ks", key, "--ks-pass", "pass:android", "--out",
                work / (package + ".apk"), aligned)


class Emulator:
    def __init__(self, adb, serial, work, wifi):
        self.adb_bin = adb
        self.serial = serial
        self.work = work
        self.wifi = wifi
        self.tag = secrets.token_hex(5)
        self.remote = f"/data/local/tmp/nexus-proxy-e2e-{self.tag}"

    def adb(self, *args, **options):
        return command(self.adb_bin, "-s", self.serial, *args, **options)

    def shell(self, script, **options):
        return self.adb("shell", script, **options)

    def root(self):
        if (self.shell("getprop ro.boot.qemu") != "1"
                or self.shell("getprop ro.build.type") not in ("userdebug", "eng")):
            raise RuntimeError("需要专用的 AOSP userdebug Android Emulator")
        if self.shell("id -u") != "0":
            self.adb("root")
            self.adb("wait-for-device")
        if self.shell("id -u") != "0":
            raise RuntimeError("模拟器必须支持 adb root")

    def stop(self):
        self.shell(f"am force-stop {NEXUS}")
        wait_for("Nexus VPN 已释放", lambda: not self.vpn_id())

    def connectivity(self):
        return self.shell("dumpsys connectivity | grep -E 'Active default network|NetworkAgentInfo\\{network'", check=False)

    def vpn_agent(self):
        return next((line for line in self.connectivity().splitlines()
                     if "ni{VPN CONNECTED" in line and "sessionId=Nexus" in line), "")

    def vpn_id(self):
        match = re.search(r"network\{(\d+)\}", self.vpn_agent())
        return match[1] if match else None

    def default_is(self, kind):
        info = self.connectivity()
        active = re.search(r"Active default network: (\d+)", info)
        if active:
            return any(f"network{{{active[1]}}}" in line and re.search(r"ni\{" + kind + r"(?:\[[^]]+\])? CONNECTED", line)
                       for line in info.splitlines())
        return False

    def wifi_only(self):
        self.shell(f"svc wifi enable; cmd wifi connect-network {shlex.quote(self.wifi)} open")
        wait_for("已连接虚拟 Wi-Fi", lambda: "Wifi is connected" in (status := self.shell("cmd wifi status"))
                 and self.wifi in status)
        self.shell("svc data disable")
        wait_for("虚拟 Wi-Fi 成为默认网络", lambda: self.default_is("WIFI"))

    def cellular_only(self):
        wait_for("虚拟蜂窝已注册", lambda: "mDataRegState=0(IN_SERVICE)" in
                 self.shell("dumpsys telephony.registry | grep mServiceState", check=False), timeout=150)
        self.shell("svc data enable; svc wifi disable")
        wait_for("虚拟蜂窝成为默认网络", lambda: self.default_is("MOBILE"))

    def start_vpn(self, port):
        self.stop()
        settings = ET.Element("map")
        ET.SubElement(settings, "boolean", name="vpn_enabled", value="true")
        ET.SubElement(settings, "string", name=f"app_route:{PROXY}").text = f"socks5:{GUEST_HOST}:{port}"
        ET.SubElement(ET.SubElement(settings, "set", name="bypass_apps"), "string").text = BYPASS
        local = self.work / "vpn.xml"
        ET.ElementTree(settings).write(local, encoding="utf-8", xml_declaration=True)
        self.adb("push", local, f"{self.remote}/vpn.xml")
        self.shell(f'set -e; owner=$(stat -c %u:%g {APP}); mkdir -p {APP}/shared_prefs; '
                   f'chown "$owner" {APP}/shared_prefs; chmod 771 {APP}/shared_prefs; '
                   f'cat {self.remote}/vpn.xml > {PREFS}; chown "$owner" {PREFS}; chmod 660 {PREFS}; '
                   f'restorecon -R {APP}/shared_prefs; appops set {NEXUS} ACTIVATE_VPN allow; '
                   f'am start -n {NEXUS}/.MainActivity --es name proxy-e2e --es relay disabled '
                   '--ez discovery false --ez multipath false')
        vpn_id = wait_for("Nexus VPN 已启动", self.vpn_id)
        wait_for("VPN 接管 IPv4 和 IPv6", lambda: "0.0.0.0/0 ->" in (agent := self.vpn_agent())
                 and "::/0 ->" in agent)
        wait_for("Nexus 核心已就绪", lambda: self.shell(f"[ -S {APP}/files/ctl.sock ] && echo ready",
                 check=False) == "ready")
        return vpn_id

    def probe(self, package, name, port, tcp, udp, host=GUEST_HOST, dns=None):
        token = f"{self.tag}-{name}"
        self.shell(f"am force-stop {package}; am start -n {package}/{PROBE_ACTIVITY} "
                   f"--es host {shlex.quote(host)} --es token {token} --ei port {port}")

        def read():
            log = self.adb("logcat", "-d", "-s", f"{LOG_TAG}:I", "*:S")
            return next((line.partition(token + " ")[2] for line in log.splitlines() if token + " dns=" in line), None)

        result = wait_for(f"探针完成 {name}", read)
        print(f"{token} {result}", flush=True)
        fields = dict(field.split("=", 1) for field in result.split())
        expected_tcp = f"{tcp}:{token}"
        expected_udp = udp if udp == "SocketTimeoutException" else f"{udp}:{token}"
        if (fields.get("tcp"), fields.get("udp")) != (expected_tcp, expected_udp) or (
                dns and not fields.get("dns", "").startswith(dns)):
            raise RuntimeError(f"{name} 预期 tcp={expected_tcp} udp={expected_udp}，实际 {result}")
        return token.encode("ascii")


def check_scenarios(device, records, socks, refusal, echo):
    device.wifi_only()
    try:
        device.probe(PROXY, "wifi-preflight", echo, "direct-tcp", "direct-udp")
    except RuntimeError as error:
        raise RuntimeError("VPN 启动前的 Wi-Fi 连通性失败，请检查 10.0.2.2 和模拟器宿主机 HTTP 代理配置") from error

    vpn = device.start_vpn(socks)
    for name, host, dns in (("wifi-v4", GUEST_HOST, None), ("wifi-v6", "2001:db8::42", None),
                            ("wifi-domain", "proxy-e2e.test", "198.18.")):
        token = device.probe(PROXY, name, echo, "socks-tcp", "socks-udp", host, dns)
        for proto in ("socks-tcp", "socks-udp"):
            if (proto, (host, echo), token) not in records():
                raise RuntimeError(f"SOCKS5 上游没有收到 {name} {proto} 的目标和探针数据")
    device.probe(BYPASS, "wifi-bypass", echo, "direct-tcp", "direct-udp")

    for name, switch in (("cellular", device.cellular_only), ("wifi-return", device.wifi_only)):
        switch()
        if device.vpn_id() != vpn:
            raise RuntimeError(f"切到 {name} 后 VPN 会话发生变化")
        device.probe(PROXY, name, echo, "socks-tcp", "socks-udp")
    print(f"Wi-Fi → 蜂窝 → Wi-Fi 使用了同一个 VPN 会话 {vpn}", flush=True)
    check_refusal_and_stop(device, records, refusal, echo)


def check_refusal_and_stop(device, records, refusal, echo):
    device.start_vpn(refusal)
    log = f"{APP}/files/daemon.log"
    offset = int(device.shell(f"wc -c < {log}"))
    token = device.probe(PROXY, "refused-udp", echo, "socks-tcp", "SocketTimeoutException")
    wait_for("核心记录了 SOCKS5 UDP 拒绝原因", lambda: "上游拒绝 UDP ASSOCIATE" in
             device.shell(f"tail -c +{offset + 1} {log}"))
    observed = records()
    if not any(proto == "socks-udp-refused" for proto, _, _ in observed) or any(
            proto == "direct-udp" and data == token for proto, _, data in observed):
        raise RuntimeError("测试上游未拒绝 UDP，或目标端收到回退直连的数据报")
    print("UDP 被上游拒绝后，目标端没有收到回退流量", flush=True)
    # NEW_TASK | CLEAR_TASK ensures MainActivity.onCreate receives the stop intent.
    device.shell(f"am start -f 0x10008000 -n {NEXUS}/.MainActivity --es action stop")
    wait_for("停用 Nexus VPN 后系统释放了会话", lambda: not device.vpn_id())
    device.probe(PROXY, "after-stop", echo, "direct-tcp", "direct-udp")


def run(args, work, upstream):
    emulator = Emulator(args.sdk / "platform-tools/adb", args.serial, work, args.wifi)
    emulator.root()
    build_probes(args.sdk, work)
    mobile = emulator.shell("settings get global mobile_data")
    wifi = emulator.shell("cmd wifi status").startswith("Wifi is enabled")
    emulator.stop()
    emulator.adb("install", "-r", args.apk, timeout=180)
    emulator.shell(f"mkdir {emulator.remote}; if [ -f {PREFS} ]; then cp -p {PREFS} {emulator.remote}/original; fi")
    try:
        for package in (PROXY, BYPASS):
            emulator.adb("uninstall", package, check=False)
            emulator.adb("install", work / (package + ".apk"), timeout=180)
        socks, refusal, echo = range(args.port_base, args.port_base + 3)
        with fixtures(socks, refusal, echo, args.local_http_port, upstream) as records:
            check_scenarios(emulator, records, socks, refusal, echo)
    finally:
        emulator.shell(f"am force-stop {NEXUS}; if [ -f {emulator.remote}/original ]; then "
                       f"cp -p {emulator.remote}/original {PREFS}; restorecon {PREFS}; else rm -f {PREFS}; fi; "
                       f"rm -f {emulator.remote}/original {emulator.remote}/vpn.xml; rmdir {emulator.remote}", check=False)
        emulator.shell(f"svc data {'enable' if mobile == '1' else 'disable'}; svc wifi {'enable' if wifi else 'disable'}",
                       check=False)
        for package in (PROXY, BYPASS):
            emulator.adb("uninstall", package, check=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--serial", default="emulator-5554", help="专用 AOSP Android Emulator 的 adb 序列号")
    parser.add_argument("--sdk", type=Path, default=os.environ.get("ANDROID_HOME", Path.home() / "android-sdk"))
    parser.add_argument("--apk", type=Path, default=ROOT / "target/android/nexus.apk")
    parser.add_argument("--wifi", default="AndroidWifi", help="模拟器内用于测试的开放 Wi-Fi SSID")
    parser.add_argument("--port-base", type=int, default=19180, help="三个连续的宿主机测试端口，默认 19180–19182")
    parser.add_argument("--local-http-port", type=int, help="为 netsimd 提供只放行本地测试端口的 HTTP 转发器")
    args = parser.parse_args()
    if not 1024 <= args.port_base <= 65533:
        parser.error("--port-base 必须在 1024–65533 之间")
    upstream = None
    if args.local_http_port is not None:
        if not 1024 <= args.local_http_port <= 65535 or args.local_http_port in range(args.port_base, args.port_base + 3):
            parser.error("--local-http-port 必须是另一个可用的非特权端口")
        proxy = urlsplit(os.environ.get("http_proxy") or os.environ.get("HTTP_PROXY", ""))
        if proxy.scheme != "http" or not proxy.hostname or not proxy.port or proxy.username:
            parser.error("--local-http-port 需要设置不带认证的 http_proxy 或 HTTP_PROXY")
        upstream = (proxy.hostname, proxy.port)
    if not args.apk.is_file():
        parser.error(f"找不到 Nexus APK：{args.apk}，请先运行 tools/build-android.sh x86_64")
    with tempfile.TemporaryDirectory(prefix="nexus-android-proxy-") as work:
        run(args, Path(work), upstream)
    print("Android 模拟器代理定向回归通过", flush=True)


if __name__ == "__main__":
    main()
