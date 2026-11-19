#!/usr/bin/env python3
"""按用户手册（docs/manual.md）实际执行一遍：手册里的例子必须能照着做。

手册里信息串写成 ```sh <设备> 的代码块（例如 ```sh laptop）是「可执行」的：从上到下，
每块在那台设备的环境里用 bash 执行（`set -e`），任何一行失败整个检查失败。
没有设备名的代码块（```sh）只是说明，不执行，例如要 root、要域名或一直在前台运行的命令。

每台设备是本机上的一个常驻程序，有自己的数据目录和 HOME（工作目录也是 HOME）：

- 块里的 `nexus daemon ...` 一行不交给 bash：由本脚本在后台启动那台设备的常驻程序
  （追加 --local-only：只用本机回环地址，不用公共中继和地址发现），等它就绪再继续。
- `<加入码>` 换成最近一次 `nexus join` 输出的加入码。手册的可执行块里不能有其他 `<…>` 占位符。
- 块里用 `&` 放到后台的程序在检查结束时全部结束。

用法：tools/manual_check.py [--bin target/debug/nexus] [--manual docs/manual.md] [--keep]
"""
# covers: 手册
import argparse
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FENCE = re.compile(r"^```sh[ \t]+([a-z][a-z0-9-]*)[ \t]*$")
PLACEHOLDER = re.compile(r"<[^<>\s]+>")


def blocks(path):
    """手册里的可执行块：[(行号, 设备, 文本)]。"""
    out, cur = [], None
    with open(path, encoding="utf-8") as f:
        for no, line in enumerate(f, 1):
            line = line.rstrip("\n")
            if cur is None:
                m = FENCE.match(line)
                if m:
                    cur = (no, m.group(1), [])
            elif line.startswith("```"):
                out.append((cur[0], cur[1], "\n".join(cur[2])))
                cur = None
            else:
                cur[2].append(line)
    return out


class Check:
    def __init__(self, args):
        self.bin = os.path.abspath(args.bin)
        self.tmp = tempfile.mkdtemp(prefix="nexus-manual-")
        self.bindir = os.path.join(self.tmp, "bin")
        os.makedirs(self.bindir)
        os.symlink(self.bin, os.path.join(self.bindir, "nexus"))
        self.daemons = {}
        self.join_code = None
        self.keep = args.keep

    def home(self, dev):
        d = os.path.join(self.tmp, dev)
        os.makedirs(os.path.join(d, "data"), exist_ok=True)
        return d

    def env(self, dev):
        e = dict(os.environ)
        e["HOME"] = self.home(dev)
        e["NEXUS_DATA_DIR"] = os.path.join(self.home(dev), "data")
        e["NEXUS_AGENT_SOCKET"] = os.path.join(self.home(dev), "agent.sock")
        e["PATH"] = self.bindir + os.pathsep + e.get("PATH", "")
        e.pop("NEXUS_AGENT", None)
        e["SHELL"] = "/bin/sh"
        return e

    def start_daemon(self, dev, line):
        import shlex
        argv = shlex.split(line.split("#", 1)[0])
        if argv[-1] == "&":
            argv = argv[:-1]
        if dev in self.daemons:
            raise SystemExit(f"{dev}: 常驻程序已经在运行")
        data = os.path.join(self.home(dev), "data")
        log = open(os.path.join(self.home(dev), "daemon.log"), "ab")
        p = subprocess.Popen([self.bin] + argv[1:] + ["--local-only",
                              "--agent-socket", os.path.join(self.home(dev), "agent.sock")],
                             env=self.env(dev), cwd=self.home(dev), stdin=subprocess.DEVNULL,
                             stdout=log, stderr=log, start_new_session=True)
        self.daemons[dev] = p
        sock = os.path.join(data, "ctl.sock")
        deadline = time.time() + 15
        while not os.path.exists(sock):
            if p.poll() is not None or time.time() > deadline:
                raise SystemExit(f"{dev}: 常驻程序没能启动，日志：\n" + self.log(dev))
            time.sleep(0.05)

    def log(self, dev):
        try:
            with open(os.path.join(self.home(dev), "daemon.log"), errors="replace") as f:
                return f.read()[-4000:]
        except OSError:
            return ""

    def run_block(self, no, dev, text):
        script = []
        for line in text.splitlines():
            s = line.strip()
            if re.match(r"^(sudo\s+)?nexus(\s+--data-dir\s+\S+)?\s+daemon\b", s) and "--help" not in s:
                self.flush(no, dev, script)
                script = []
                self.start_daemon(dev, s)
                continue
            if "<加入码>" in line:
                if not self.join_code:
                    raise SystemExit(f"第 {no} 行的块：用到 <加入码>，但之前没有 nexus join")
                line = line.replace("<加入码>", self.join_code)
            left = PLACEHOLDER.findall(line.split("#", 1)[0])
            if left:
                raise SystemExit(f"第 {no} 行的块（{dev}）：可执行块里不能有占位符 {left}：{line}")
            script.append(line)
        self.flush(no, dev, script)

    def flush(self, no, dev, script):
        if not any(l.strip() and not l.strip().startswith("#") for l in script):
            return
        body = "\n".join(script)
        print(f"── 第 {no} 行 · {dev}")
        for l in script:
            if l.strip():
                print(f"   {dev}$ {l}")
        # 后台程序留在一个会话里，结束时一起结束；bash 自己不等它们。
        # 输出写进文件而不是管道：块里放到后台的程序会一直占着继承来的输出。
        outp = os.path.join(self.home(dev), f"block-{no}.out")
        with open(outp, "w") as f:
            p = subprocess.run(["bash", "-e", "-o", "pipefail", "-c", body], env=self.env(dev),
                               cwd=self.home(dev), stdin=subprocess.DEVNULL, stdout=f,
                               stderr=subprocess.STDOUT, timeout=120)
        with open(outp, errors="replace") as f:
            out = f.read()
        for l in out.splitlines()[:40]:
            print(f"   | {l}")
        if p.returncode != 0:
            print(f"\n失败：第 {no} 行的块在 {dev} 上退出码 {p.returncode}")
            for d in self.daemons:
                print(f"\n=== {d} 的常驻程序日志（末尾） ===\n{self.log(d)}")
            raise SystemExit(1)
        m = re.search(r"加入码[^\n]*\n\s*\n(\S+)", out)
        if m:
            self.join_code = m.group(1)

    def teardown(self):
        # 后台程序（`&`）的进程组都在本次运行的会话里：按 HOME 找出来结束。
        me = os.getpid()
        for pid in os.listdir("/proc"):
            if not pid.isdigit() or int(pid) == me:
                continue
            try:
                with open(f"/proc/{pid}/environ", "rb") as f:
                    env = f.read()
            except OSError:
                continue
            if f"HOME={self.tmp}/".encode() in env:
                try:
                    os.kill(int(pid), signal.SIGTERM)
                except OSError:
                    pass
        for p in self.daemons.values():
            try:
                p.wait(timeout=5)
            except subprocess.TimeoutExpired:
                p.kill()
        if not self.keep:
            shutil.rmtree(self.tmp, ignore_errors=True)
        else:
            print(f"保留目录：{self.tmp}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.path.join(ROOT, "target/debug/nexus"))
    ap.add_argument("--manual", default=os.path.join(ROOT, "docs/manual.md"))
    ap.add_argument("--keep", action="store_true", help="保留各设备的目录和日志")
    args = ap.parse_args()
    bs = blocks(args.manual)
    if not bs:
        raise SystemExit("手册里没有可执行块")
    c = Check(args)
    try:
        for no, dev, text in bs:
            c.run_block(no, dev, text)
    finally:
        c.teardown()
    print(f"\n手册检查通过：{len(bs)} 个可执行块，{len(c.daemons)} 台设备")


if __name__ == "__main__":
    main()
