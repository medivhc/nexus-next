#!/usr/bin/env python3
"""Search for CI plans violating the browser-build impact contract."""
import argparse
import itertools
import json
from pathlib import Path
import runpy
import subprocess


ROOTS = {"nexus-web", "nexus-cli"}
SKIP_CONTROLS = {"nexus-android", "nexus-sys-sim", "nexus-testkit"}
DOC = "docs/ci-selective-policy.md"


def cargo_graph(repo, cargo):
    result = subprocess.check_output(
        [cargo, "metadata", "--format-version", "1", "--no-deps", "--locked", "--offline"],
        cwd=repo,
        text=True,
    )
    metadata = json.loads(result)
    members = set(metadata["workspace_members"])
    packages = {p["name"]: p for p in metadata["packages"] if p["id"] in members}
    by_dir = {Path(p["manifest_path"]).parent: name for name, p in packages.items()}
    graph = {
        name: {
            by_dir[Path(dep["path"])]
            for dep in pkg["dependencies"]
            if dep["kind"] in {None, "build"} and dep.get("path")
            and Path(dep["path"]) in by_dir
        }
        for name, pkg in packages.items()
    }
    return packages, graph


def build_inputs(graph):
    # Each witness is a path from a build root to the affected package.
    witnesses = {root: (root,) for root in sorted(ROOTS)}
    while True:
        inferred = {
            dependency: chain + (dependency,)
            for package, chain in witnesses.items()
            for dependency in sorted(graph[package])
            if dependency not in witnesses
        }
        if not inferred:
            return witnesses
        witnesses.update(inferred)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--cargo", default="cargo")
    args = parser.parse_args()
    repo = args.repo.resolve()
    packages, graph = cargo_graph(repo, args.cargo)
    witnesses = build_inputs(graph)
    print("Build inputs and dependency witnesses:")
    for name in sorted(witnesses):
        print("  " + " -> ".join(witnesses[name]))

    tracked = sorted(subprocess.check_output(["git", "ls-files", "-z"], cwd=repo, text=True).split("\0"))
    owners = {DOC: None}
    for name, pkg in packages.items():
        manifest = Path(pkg["manifest_path"]).relative_to(repo).as_posix()
        src_prefix = manifest.removesuffix("Cargo.toml") + "src/"
        owners[manifest] = name
        source = next(path for path in tracked if path.startswith(src_prefix) and path.endswith(".rs"))
        owners[source] = name

    classify = runpy.run_path(str(repo / "tools/ci_plan.py"))["classify"]
    counterexamples = []
    checked = 0
    for size in range(3):
        for paths in itertools.combinations(sorted(owners), size):
            changed = {owners[path] for path in paths} - {None}
            if changed & witnesses.keys():
                expected = True
            elif changed <= SKIP_CONTROLS:
                expected = False
            else:
                continue
            for order in (paths, paths[::-1]) if size == 2 else (paths,):
                checked += 1
                actual = classify(list(order), "pull_request").web
                if actual != expected:
                    counterexamples.append((order, expected, actual))

    for event in ("push", "schedule", "workflow_dispatch"):
        checked += 1
        actual = classify([], event).web
        if not actual:
            counterexamples.append(((event,), True, actual))

    for paths, expected, actual in counterexamples:
        if len(paths) <= 1:
            print(f"Minimal case: {json.dumps(paths)} expected web={expected}, got web={actual}")
    print(f"Checked {checked} cases, found {len(counterexamples)} counterexamples.")
    raise SystemExit(bool(counterexamples))


if __name__ == "__main__":
    main()
