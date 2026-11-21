#!/usr/bin/env python3
"""新旧版本混跑（M7.1，FP-7.1-06）：一台设备用新版本、另一台用旧版本，日常操作照常。

两轮：先新版本当管理设备、旧版本当成员，再反过来。每轮在本机起两个常驻程序（--local-only），
检查组网、ls、status、exec、发布和使用服务、工作区同步。

用法：tools/compat_check.py --old-bin <旧版本的 nexus> [--new-bin target/debug/nexus] [--keep]
只给新版本（--old-bin 和 --new-bin 相同）时检查脚本本身。
"""
# covers: R65
import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


class Dev:
    def __init__(self, tmp, name, binary):
        self.name, self.bin = name, os.path.abspath(binary)
        self.home = os.path.join(tmp, name)
        self.data = os.path.join(self.home, "data")
        os.makedirs(self.data)
        self.proc = None

    def env(self):
        e = dict(os.environ)
        e.update(HOME=self.home, NEXUS_DATA_DIR=self.data)
        e.pop("NEXUS_AGENT", None)
        return e

    def start(self):
        log = open(os.path.join(self.home, "daemon.log"), "ab")
        self.proc = subprocess.Popen(
            [self.bin, "daemon", "--name", self.name, "--local-only", "--rendezvous", "disabled"],
            env=self.env(), cwd=self.home, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True,
        )
        sock = os.path.join(self.data, "ctl.sock")
        deadline = time.time() + 15
        while not os.path.exists(sock):
            if self.proc.poll() is not None or time.time() > deadline:
                raise SystemExit(f"{self.name}：常驻程序没能启动\n{self.log()}")
            time.sleep(0.05)

    def log(self):
        with open(os.path.join(self.home, "daemon.log"), errors="replace") as f:
            return f.read()[-3000:]

    def run(self, *args, json_out=True, stdin=None, check=True):
        argv = [self.bin, *args] + (["--json"] if json_out else [])
        p = subprocess.run(argv, env=self.env(), cwd=self.home, input=stdin, capture_output=True, text=True, timeout=60)
        if check and p.returncode != 0:
            raise SystemExit(f"{self.name}$ nexus {' '.join(args)} 退出码 {p.returncode}\n{p.stdout}{p.stderr}")
        if json_out and p.returncode == 0:
            return json.loads(p.stdout)
        return p

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()


def version(binary):
    return subprocess.run([binary, "--version"], capture_output=True, text=True).stdout.strip()


def round_(tmp, admin_bin, member_bin, label):
    print(f"── {label}")
    a = Dev(tmp, "laptop", admin_bin)
    b = Dev(tmp, "home", member_bin)
    for d in (a, b):
        d.start()
    try:
        a.run("init")
        code = b.run("join")["code"]
        a.run("device", "approve", code, "--name", "home")
        for d in (a, b):
            ls = d.run("ls")
            names = sorted(x["name"] for x in ls["devices"])
            assert names == ["home", "laptop"], ls
            other = [x for x in ls["devices"] if not x["self"]][0]
            assert other["online"], f"{d.name} 看 {other['name']} 不在线：{ls}"
            print(f"   {d.name}: ls 看到 {other['name']} 在线，版本 {other.get('version')}")
        for d, peer in ((a, "home"), (b, "laptop")):
            st = d.run("status", peer)
            assert st["online"], st
            out = d.run("exec", peer, "--", "echo", "hi", json_out=False)
            assert out.stdout.strip() == "hi", out.stdout
        print("   status、exec：两个方向都通")
        # 服务：成员上发布，管理设备上用
        srv = subprocess.Popen([sys.executable, "-m", "http.server", "18731", "--bind", "127.0.0.1"],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            time.sleep(0.5)
            b.run("publish", "files", "--port", "18731", "--proto", "http")
            got = a.run("run", "--use", "home/files", "--", "sh", "-c",
                        'python3 -c "import os,urllib.request;print(urllib.request.urlopen(\'http://\'+os.environ[\'NEXUS_SVC_HOME_FILES\']).status)"',
                        json_out=False)
            assert got.stdout.strip() == "200", got.stdout + got.stderr
        finally:
            srv.terminate()
        print("   发布和使用服务：通")
        # 工作区：成员当存储设备，管理设备提交，成员读
        b.run("drive", "serve", "on")
        a.run("drive", "create", "notes")
        a.run("drive", "put", "notes:/todo.txt", json_out=False, stdin="买牛奶\n")
        a.run("drive", "commit", "notes", "-m", "第一条")
        b.run("drive", "sync", "notes")
        got = b.run("drive", "cat", "notes:/todo.txt", json_out=False)
        assert got.stdout == "买牛奶\n", got.stdout
        print("   工作区同步：通")
    except BaseException:
        for d in (a, b):
            print(f"\n=== {d.name} 的日志 ===\n{d.log()}")
        raise
    finally:
        for d in (a, b):
            d.stop()
        shutil.rmtree(a.home, ignore_errors=True)
        shutil.rmtree(b.home, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--old-bin", required=True)
    ap.add_argument("--new-bin", default=os.path.join(ROOT, "target/debug/nexus"))
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()
    old, new = version(args.old_bin), version(args.new_bin)
    print(f"旧：{old}，新：{new}")
    tmp = tempfile.mkdtemp(prefix="nexus-compat-")
    try:
        round_(tmp, args.new_bin, args.old_bin, "新版本是管理设备、旧版本是成员")
        round_(tmp, args.old_bin, args.new_bin, "旧版本是管理设备、新版本是成员")
    finally:
        if not args.keep:
            shutil.rmtree(tmp, ignore_errors=True)
    print("\n新旧混跑通过")


if __name__ == "__main__":
    main()
