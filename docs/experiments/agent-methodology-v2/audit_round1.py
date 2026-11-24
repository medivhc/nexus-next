#!/usr/bin/env python3
"""Replay PR #25's model across both heads, and check Git rename and gate behavior."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile

BASE = "a54db991393645db5e9f13888fcc79b4a2d5cf3f"
A = "bacc642a821571ad981f350037b57a0c125bb120"
B = "3c96e640389021564b4b7921397be33ca4d82c38"
MODEL_PATH = "docs/experiments/ab-result-b/model.py"
CONTRACT_PATH = "docs/experiments/ab-result-b/contract.md"
OLD = "crates/nexus-web-core/src/drive.rs"
NEW = "crates/nexus-testkit/src/renamed_drive.rs"


def run(repo, *args, check=True, **kwargs):
    return subprocess.run(args, cwd=repo, text=True, capture_output=True, check=check, **kwargs)


def git(repo, *args):
    return run(repo, "git", *args).stdout.strip()


def model_result(tree, model, cargo):
    result = run(tree, sys.executable, str(model), "--repo", str(tree), "--cargo", cargo, check=False)
    match = re.search(r"Checked (\d+) cases, found (\d+) counterexamples", result.stdout)
    if not match:
        raise RuntimeError(f"Model did not produce a result:\n{result.stdout}\n{result.stderr}")
    return {"cases": int(match[1]), "counterexamples": int(match[2]), "exit": result.returncode,
            "stdout": result.stdout}


def rename_fixture(tree):
    old = tree / OLD
    old.parent.mkdir(parents=True, exist_ok=True)
    old.write_text("pub fn fixture() {}\n", encoding="utf-8")
    git(tree, "init", "--quiet")
    git(tree, "add", ".")
    commit = ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
              "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", "commit", "-qm")
    git(tree, *commit, "baseline")
    base = git(tree, "rev-parse", "HEAD")
    (tree / NEW).parent.mkdir(parents=True, exist_ok=True)
    git(tree, "mv", OLD, NEW)
    git(tree, *commit, "move")
    return base, git(tree, "rev-parse", "HEAD")


def gate_result(tree, plan, web_result):
    import os
    needs = {"plan": {"result": "success", "outputs": {
        key: str(value).lower() for key, value in plan.items()
    }}, "sanity": {"result": "success"}}
    for job, key in (("smoke", "smoke"), ("ci", "full"), ("compat", "compat")):
        needs[job] = {"result": "success" if plan[key] else "skipped"}
    needs["web"] = {"result": web_result}
    result = run(tree, sys.executable, "tools/ci_gate.py", check=False,
                 env={**os.environ, "CI_NEEDS_JSON": json.dumps(needs)})
    return {"exit": result.returncode, "output": result.stdout + result.stderr}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("base", "a", "b"):
        parser.add_argument(f"--{name}", required=True, type=Path)
    parser.add_argument("--cargo", default="cargo")
    args = parser.parse_args()
    trees = {name: getattr(args, name).resolve() for name in ("base", "a", "b")}
    if {name: git(tree, "rev-parse", "HEAD") for name, tree in trees.items()} != {
        "base": BASE, "a": A, "b": B,
    } or any(git(tree, "status", "--porcelain", "--untracked-files=no") for tree in trees.values()):
        raise RuntimeError("Audit needs the three exact, clean tracked trees")
    model = trees["b"] / MODEL_PATH
    report = {"cargo": run(trees["b"], args.cargo, "--version").stdout.strip(),
              "python": sys.version.split()[0], "git": git(trees["b"], "--version"),
              "model_sha256": hashlib.sha256(model.read_bytes()).hexdigest(),
              "contract_sha256": hashlib.sha256((trees["b"] / CONTRACT_PATH).read_bytes()).hexdigest(),
              "revisions": {}}
    with tempfile.TemporaryDirectory(prefix="nexus-rename-audit-") as temporary:
        fixture = Path(temporary)
        before, after = rename_fixture(fixture)
        for name, tree in trees.items():
            model_output = model_result(tree, model, args.cargo)
            expected_failures = 32 if name == "base" else 0
            if (model_output["cases"], model_output["counterexamples"], model_output["exit"]) != (
                1197, expected_failures, int(bool(expected_failures))
            ):
                raise RuntimeError(f"Unexpected model result on {name}: {model_output}")
            tests = run(tree, sys.executable, "-B", "-m", "unittest", "discover", "-s", "tools",
                        "-p", "test_ci_plan.py", "-v")
            plan_output = json.loads(run(fixture, sys.executable, str(tree / "tools/ci_plan.py"),
                                         "--event", "pull_request", "--base", before, "--head", after).stdout)
            plan = plan_output["plan"]
            skip = gate_result(tree, plan, "skipped")
            success = gate_result(tree, plan, "success")
            candidate = name != "base"
            if plan["web"] != candidate or (skip["exit"], success["exit"]) != (
                (1, 0) if candidate else (0, 1)
            ) or set(plan_output["paths"]) != ({OLD, NEW} if candidate else {NEW}):
                raise RuntimeError(f"Unexpected rename/gate result on {name}: {plan_output}, {skip}, {success}")
            report["revisions"][name] = {"sha": git(tree, "rev-parse", "HEAD"), "model": model_output,
                "tests": tests.stdout + tests.stderr, "rename": plan_output,
                "web_skipped": skip, "web_success": success}
            if candidate:
                git(tree, "diff", "--check", BASE, "HEAD")
    print(json.dumps(report, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
