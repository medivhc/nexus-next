#!/usr/bin/env python3
"""Fail-closed GitHub Actions test selection for nexus-next."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import PurePosixPath
import re
import subprocess
from dataclasses import asdict, dataclass


@dataclass(frozen=True)
class Plan:
    smoke: bool = False
    full: bool = False
    compat: bool = False
    web: bool = False


DOC_DIRS = ("docs/", "tasks/")
DOC_SUFFIXES = (".md", ".rst", ".txt", ".png", ".jpg", ".jpeg", ".svg", ".webp")
# 只有浏览器才用的部分：不影响原生的 L2/L3。nexus-web-core 不在这里：testkit 的浏览器场景
# （web、web_drive、web_service，在 L2/L3 里）直接跑它。
WEB_DIRS = ("web/", "tools/web-e2e/", "crates/nexus-web/")
# 被测试当作输入执行的文档：tools/manual_check.py 照着用户手册做一遍（L3 用户手册）。
EXECUTED_DOCS = {"docs/manual.md"}
WIRE_CRATES = {
    "nexus-core-proto", "nexus-wire", "nexus-term-proto", "nexus-net-core",
    "nexus-cli", "nexus-runtime", "nexus-fabric", "nexus-fabric-iroh",
    "nexus-fabric-sim",
}


def is_document(path: str) -> bool:
    if path in EXECUTED_DOCS:
        return False
    if path.startswith(DOC_DIRS) and path.lower().endswith(DOC_SUFFIXES):
        return True
    if path.endswith(".md"):
        return True
    return path in {"LICENSE", "LICENSE-APACHE", "LICENSE-MIT", ".gitignore"}


def classify(paths: list[str], event: str) -> Plan:
    if event != "pull_request":
        # Main, nightly, and manual runs are the full-coverage backstop.
        return Plan(smoke=True, full=True, compat=True, web=True)

    smoke = full = compat = web = False
    for path in paths:
        if not path or path.startswith("/") or ".." in PurePosixPath(path).parts:
            raise ValueError(f"invalid changed path: {path!r}")
        if is_document(path):
            continue
        smoke = True
        if path in EXECUTED_DOCS:
            full = True
            continue
        if path.startswith(WEB_DIRS) or path == "tools/build-web.sh":
            web = True
            continue
        # A workflow/lockfile change must verify every CI lane.
        if path.startswith(".github/") or path in {"Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "rust-toolchain"}:
            full = compat = web = True
            continue
        if path.startswith("vendor/"):
            full = compat = web = True
            continue
        if path.startswith("tools/"):
            full = True
            if path.startswith("tools/ci") or path.startswith("tools/test_ci"):
                compat = web = True
            elif path == "tools/compat_check.py":
                compat = True
            continue
        if path.startswith("crates/"):
            parts = PurePosixPath(path).parts
            if len(parts) < 3:
                full = compat = web = True
                continue
            crate = parts[1]
            # Only browser-specific crates can bypass native L2/L3.
            full = True
            # The browser depends on both Web and Drive core (see Cargo manifests).
            if crate in {"nexus-web-core", "nexus-drive-core"}:
                web = True
            if crate in WIRE_CRATES:
                compat = True
            if crate in WIRE_CRATES or crate in {"nexus-sys-real", "nexus-sys"}:
                web = True
            continue
        # Unknown/new directory: fail closed by running everything.
        full = compat = web = True
    return Plan(smoke=smoke, full=full, compat=compat, web=web)


def changed_files(base: str, head: str) -> list[str]:
    for sha in (base, head):
        if not re.fullmatch(r"[0-9a-fA-F]{40}", sha):
            raise ValueError(f"invalid git SHA: {sha!r}")
    result = subprocess.run(
        ["git", "diff", "--name-only", "-z", f"{base}...{head}", "--"],
        check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    return [os.fsdecode(path) for path in result.stdout.split(b"\0") if path]


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--event", required=True)
    ap.add_argument("--base", default="")
    ap.add_argument("--head", default="")
    ap.add_argument("--paths-json", help="local deterministic test mode, without git")
    args = ap.parse_args()
    paths = (
        json.loads(args.paths_json)
        if args.paths_json is not None
        else changed_files(args.base, args.head) if args.event == "pull_request" else []
    )
    plan = classify(paths, args.event)
    print(json.dumps({"event": args.event, "paths": paths, "plan": asdict(plan)}, ensure_ascii=False))
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as f:
            for name, enabled in asdict(plan).items():
                f.write(f"{name}={str(enabled).lower()}\n")


if __name__ == "__main__":
    main()
