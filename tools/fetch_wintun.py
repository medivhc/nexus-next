#!/usr/bin/env python3
"""下载 Wintun（Windows 虚拟网卡的驱动库，M11.3，R83），校验后把 wintun.dll 和许可证放到指定目录。

Wintun 的预编译版允许随软件一起分发（见它的 LICENSE.txt），所以 Windows 的发布包里带着它，
放在 nexus.exe 旁边。CI 里检查虚拟网卡时也用它。

用法：tools/fetch_wintun.py <目录> [--arch amd64|arm64|x86|arm]
"""
import argparse
import hashlib
import io
import os
import sys
import urllib.request
import zipfile

VERSION = "0.14.1"
URL = f"https://www.wintun.net/builds/wintun-{VERSION}.zip"
SHA256 = "07c256185d6ee3652e09fa55c0b673e2624b565e02c4b9091c79ca7d2f24ef51"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dir")
    ap.add_argument("--arch", default="amd64")
    args = ap.parse_args()
    data = urllib.request.urlopen(URL, timeout=60).read()
    got = hashlib.sha256(data).hexdigest()
    if got != SHA256:
        sys.exit(f"{URL} 的 SHA256 是 {got}，应当是 {SHA256}")
    os.makedirs(args.dir, exist_ok=True)
    with zipfile.ZipFile(io.BytesIO(data)) as z:
        files = {
            f"wintun/bin/{args.arch}/wintun.dll": "wintun.dll",
            "wintun/LICENSE.txt": "wintun-LICENSE.txt",
        }
        for src, dst in files.items():
            with open(os.path.join(args.dir, dst), "wb") as f:
                f.write(z.read(src))
    print(f"Wintun {VERSION}（{args.arch}）→ {args.dir}")


if __name__ == "__main__":
    main()
