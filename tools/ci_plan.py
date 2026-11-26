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
    # M11.3：在 macOS 和 Windows 的 runner 上编译并跑 tools/platform_check.py（代替真机）。
    # 和 compat 一样只看会进 nexus 二进制的改动；macOS 的 runner 按 10 倍计时，文档和网页改动不跑。
    platforms: bool = False


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
# Crates whose change reaches the real nexus binary (compat job) and the
# browser end-to-end test (web job) even though they are not wire crates:
# nexus-cli depends on nexus-drive-core and nexus-sys-real, nexus-sys-real
# depends on nexus-sys, and nexus-web-core (run directly by the web_drive
# scenarios) depends on nexus-drive-core.
NATIVE_CRATES = {"nexus-drive-core", "nexus-sys-real", "nexus-sys"}
# Crates that only need the full lane: test-only or platform-only code that
# neither the compat binaries nor the browser page exercise.
FULL_ONLY_CRATES = {"nexus-sys-sim", "nexus-testkit", "nexus-android"}


def is_document(path: str) -> bool:
    if path in EXECUTED_DOCS:
        return False
    if path.startswith(DOC_DIRS) and path.lower().endswith(DOC_SUFFIXES):
        return True
    if path.endswith(".md"):
        # Only root-level docs and docs/ count as documentation. A .md file
        # inside a crate may be embedded (include_str!) or otherwise
        # load-bearing, so it must not skip the build.
        return "/" not in path or path.startswith("docs/")
    return path in {"LICENSE", "LICENSE-APACHE", "LICENSE-MIT", ".gitignore"}


def classify(paths: list[str], event: str) -> Plan:
    if event != "pull_request":
        # Main, nightly, and manual runs are the full-coverage backstop.
        return Plan(smoke=True, full=True, compat=True, web=True, platforms=True)

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
            else:
                # Any other tools/ script is unknown to the planner: fail
                # closed instead of silently skipping compat/web.
                compat = web = True
            continue
        if path.startswith("crates/"):
            parts = PurePosixPath(path).parts
            if len(parts) < 3:
                full = compat = web = True
                continue
            crate = parts[1]
            # Only browser-specific crates can bypass native L2/L3.
            full = True
            if crate == "nexus-web-core":
                web = True
            if crate in WIRE_CRATES:
                compat = True
            if crate in WIRE_CRATES or crate in {"nexus-sys-real", "nexus-sys"}:
                web = True
            if crate in NATIVE_CRATES:
                compat = web = True
            elif crate not in WIRE_CRATES and crate != "nexus-web-core" and crate not in FULL_ONLY_CRATES:
                # New/unknown crate: fail closed. A crate added without
                # updating the lists above runs every lane instead of
                # silently skipping compat/web.
                compat = web = True
            continue
        # Unknown/new directory: fail closed by running everything.
        full = compat = web = True
    return Plan(smoke=smoke, full=full, compat=compat, web=web, platforms=compat)


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
