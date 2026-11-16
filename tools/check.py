#!/usr/bin/env python3
"""Nexus 的静态检查（测试方案中的 L0）。

检查四件事，任何一件不满足都以非零退出码失败：
  1. 结构约束：纯逻辑层不碰系统；各层只依赖允许的 crate。
  2. 需求追溯：FEATURES.md 里已实现的需求都有测试覆盖；测试引用的编号都存在。
  3. 依赖许可证：都在允许清单内。
  4. （--self-test）以上检查本身有效：故意制造违规，确认能被拦住。

用法：tools/check.py [--self-test] [--no-license]
"""
# covers: R25
import json
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent

# 纯逻辑层：crate 名 -> 允许的依赖
PURE = {
    "nexus-core-proto": {"serde", "postcard", "ed25519-dalek", "p256", "sha2", "serde_json"},
    "nexus-wire": {"nexus-core-proto", "nexus-term-proto", "serde", "postcard"},
    "nexus-term-proto": {"serde", "vt100"},
}
PURE_DEV_EXTRA = {"proptest"}

# 纯逻辑层源码里不允许出现的东西
BANNED_IN_PURE = [
    (r"\bSystemTime\b", "系统时间"),
    (r"\bInstant\b", "系统时钟"),
    (r"\bstd::(net|thread|fs|process|env|os)\b", "操作系统接口"),
    (r"\b(tokio|futures|futures_util|async_std)\b", "异步运行时"),
    (r"\b(rand|getrandom)::", "随机数源"),
    (r"\bunsafe\b", "unsafe"),
]

# 其他层禁止直接依赖的 crate
FORBIDDEN_DEPS = {
    "nexus-runtime": {
        "iroh", "portable-pty", "turmoil", "russh",
        "nexus-sys-real", "nexus-sys-sim", "nexus-fabric-sim", "nexus-fabric-iroh",
    },
    "nexus-sys": {"tokio", "turmoil", "iroh"},
    "nexus-fabric": {"turmoil", "iroh"},
    "nexus-cli": {"turmoil", "nexus-sys-sim", "nexus-fabric-sim"},
    # 网页节点核心要编译进浏览器：只能依赖接缝和纯逻辑。
    # 浏览器绑定：和网页节点核心一样不能碰仿真、系统驱动和运行时。
    "nexus-web": {
        "turmoil", "portable-pty", "russh", "nexus-runtime",
        "nexus-sys-real", "nexus-sys-sim", "nexus-fabric-sim",
    },
    "nexus-web-core": {
        "iroh", "turmoil", "portable-pty", "russh", "nexus-runtime",
        "nexus-sys-real", "nexus-sys-sim", "nexus-fabric-sim", "nexus-fabric-iroh",
    },
}

ALLOWED_LICENSES = {
    "MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause", "BSD-3-Clause",
    "ISC", "Unicode-3.0", "Unicode-DFS-2016", "Zlib", "Unlicense", "CC0-1.0", "BSL-1.0", "0BSD",
    # 根证书数据使用的宽松数据许可证
    "CDLA-Permissive-2.0",
}

# 逐个批准的例外：crate 名 -> (许可证, 理由)。新增例外需要写明理由。
LICENSE_EXCEPTIONS = {
    "attohttpc": ("MPL-2.0", "iroh 端口映射功能的依赖；未修改使用，MPL 的约束限于该 crate 自身的文件"),
}


def strip_comments(text):
    return "\n".join(line.split("//", 1)[0] for line in text.splitlines())


def check_arch(root):
    errors = []
    crates = root / "crates"
    for name, allowed in PURE.items():
        manifest = tomllib.loads((crates / name / "Cargo.toml").read_text(encoding="utf-8"))
        for dep in manifest.get("dependencies", {}):
            if dep not in allowed:
                errors.append(f"{name}: 纯逻辑层不允许依赖 {dep}（如确有必要，修改 tools/check.py 的白名单）")
        for dep in manifest.get("dev-dependencies", {}):
            if dep not in allowed | PURE_DEV_EXTRA:
                errors.append(f"{name}: 纯逻辑层的测试不允许依赖 {dep}")
        for path in sorted((crates / name / "src").rglob("*.rs")):
            code = strip_comments(path.read_text(encoding="utf-8"))
            for lineno, line in enumerate(code.splitlines(), 1):
                if "forbid(unsafe_code)" in line:
                    continue
                for pattern, what in BANNED_IN_PURE:
                    if re.search(pattern, line):
                        errors.append(f"{path.relative_to(root)}:{lineno}: 纯逻辑层不允许使用{what}：{line.strip()}")
    for name, forbidden in FORBIDDEN_DEPS.items():
        manifest_path = crates / name / "Cargo.toml"
        if not manifest_path.exists():
            continue
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
        for dep in manifest.get("dependencies", {}):
            if dep in forbidden:
                errors.append(f"{name}: 不允许直接依赖 {dep}，请通过接缝 trait 使用")
    return errors


def parse_features(root):
    """返回 {编号: 是否已实现}。"""
    reqs = {}
    current = None
    for line in (root / "FEATURES.md").read_text(encoding="utf-8").splitlines():
        m = re.match(r"###\s+(R\d\d)\b", line)
        if m:
            current = m.group(1)
            reqs[current] = None
            continue
        m = re.match(r"-\s*状态：(已实现|待实现)", line)
        if m and current and reqs[current] is None:
            reqs[current] = m.group(1) == "已实现"
    return reqs


def collect_covers(root):
    """返回 {编号: [引用它的文件]}。识别 `covers: "R01, R02"` 和 `// covers: R01`。"""
    found = {}
    # 浏览器端到端测试（tools/web-e2e/*.mjs）也算：有些需求只能在真实浏览器里验证。
    files = (
        list((root / "crates").rglob("*.rs"))
        + list((root / "tools").glob("*.py"))
        + list((root / "tools").glob("*/*.mjs"))
    )
    for path in files:
        if "target" in path.parts or "node_modules" in path.parts:
            continue
        for m in re.finditer(r"covers:\s*\"?((?:R\d\d)(?:\s*,\s*R\d\d)*)", path.read_text(encoding="utf-8")):
            for rid in re.findall(r"R\d\d", m.group(1)):
                found.setdefault(rid, []).append(str(path.relative_to(root)))
    return found


def check_trace(root):
    errors = []
    reqs = parse_features(root)
    covers = collect_covers(root)
    for rid, implemented in sorted(reqs.items()):
        if implemented is None:
            errors.append(f"FEATURES.md: {rid} 没有写状态")
        elif implemented and rid not in covers:
            errors.append(f"FEATURES.md: {rid} 标为已实现，但没有任何测试用 covers 标注它")
    for rid, files in sorted(covers.items()):
        if rid not in reqs:
            errors.append(f"{files[0]}: 引用了 FEATURES.md 中不存在的需求 {rid}")
    return errors


def license_ok(expr):
    expr = expr.replace("(", " ").replace(")", " ")
    for part in re.split(r"\s+AND\s+", expr.strip()):
        options = [o.strip() for o in re.split(r"\s+OR\s+|/", part)]
        if not any(o in ALLOWED_LICENSES for o in options):
            return False
    return True


def check_licenses(root):
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=root, capture_output=True, text=True,
    )
    if out.returncode != 0:
        return [f"cargo metadata 失败：{out.stderr.strip()[:200]}"]
    errors = []
    for pkg in json.loads(out.stdout)["packages"]:
        lic = pkg.get("license")
        exception = LICENSE_EXCEPTIONS.get(pkg["name"])
        if exception and lic == exception[0]:
            continue
        if not lic or not license_ok(lic):
            errors.append(f"依赖 {pkg['name']} {pkg['version']} 的许可证不在允许清单内：{lic}")
    return errors


def self_test():
    """故意制造违规，确认每一类都能被拦住。"""
    failures = []

    def scratch():
        tmp = pathlib.Path(tempfile.mkdtemp(prefix="nexus-check-"))
        shutil.copytree(ROOT / "crates", tmp / "crates", ignore=shutil.ignore_patterns("target"))
        shutil.copytree(ROOT / "tools", tmp / "tools")
        shutil.copy(ROOT / "FEATURES.md", tmp / "FEATURES.md")
        return tmp

    def expect(label, mutate, check, needle):
        tmp = scratch()
        try:
            if check(tmp):
                failures.append(f"{label}: 未改动的副本就已经报错")
                return
            mutate(tmp)
            errors = check(tmp)
            if not any(needle in e for e in errors):
                failures.append(f"{label}: 违规没有被发现（得到 {errors}）")
        finally:
            shutil.rmtree(tmp, ignore_errors=True)

    def append(rel, text):
        def go(tmp):
            with open(tmp / rel, "a", encoding="utf-8") as f:
                f.write(text)
        return go

    expect("纯逻辑层读系统时间",
           append("crates/nexus-core-proto/src/lib.rs",
                  "\npub fn bad() -> u64 { std::time::SystemTime::now().elapsed().unwrap().as_secs() }\n"),
           check_arch, "系统时间")
    expect("纯逻辑层使用异步运行时",
           append("crates/nexus-wire/src/lib.rs", "\npub async fn bad() { tokio::task::yield_now().await }\n"),
           check_arch, "异步运行时")
    expect("纯逻辑层新增依赖",
           append("crates/nexus-core-proto/Cargo.toml", 'tokio = "1"\n'),
           check_arch, "不允许依赖 tokio")
    expect("运行时直接依赖具体传输",
           append("crates/nexus-runtime/Cargo.toml", 'turmoil = "0.7"\n'),
           check_arch, "不允许直接依赖 turmoil")

    def add_untested(tmp):
        p = tmp / "FEATURES.md"
        p.write_text(p.read_text(encoding="utf-8") + "\n### R98 自检用的需求\n- 状态：已实现（自检）\n- 没有任何测试覆盖它。\n", encoding="utf-8")
    expect("已实现的需求没有测试", add_untested, check_trace, "R98 标为已实现")
    expect("测试引用不存在的需求",
           append("crates/nexus-cli/tests/scenarios.rs", "\n// covers" + ": R" + "99\n"),
           check_trace, "不存在的需求 R" + "99")

    if license_ok("GPL-3.0") or not license_ok("MIT OR Apache-2.0") or license_ok("MIT AND GPL-3.0"):
        failures.append("许可证表达式的判断不正确")
    return failures


def main():
    args = set(sys.argv[1:])
    if "--self-test" in args:
        failures = self_test()
        for f in failures:
            print(f"自检失败  {f}")
        print("自检通过：7 类故意制造的违规都被拦住" if not failures else f"自检失败：{len(failures)} 项")
        return 1 if failures else 0

    errors = check_arch(ROOT) + check_trace(ROOT)
    if "--no-license" not in args:
        errors += check_licenses(ROOT)
    for e in errors:
        print(e)
    reqs = parse_features(ROOT)
    done = sum(1 for v in reqs.values() if v)
    if errors:
        print(f"\n检查失败：{len(errors)} 个问题")
        return 1
    print(f"检查通过：结构约束、需求追溯（{done}/{len(reqs)} 条需求已实现并有测试）、依赖许可证")
    return 0


if __name__ == "__main__":
    sys.exit(main())
