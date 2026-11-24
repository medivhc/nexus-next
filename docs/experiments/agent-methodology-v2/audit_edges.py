#!/usr/bin/env python3
"""Probe the historical candidates and round-2 baseline outside the round-1 model domain."""
import argparse
import json
from pathlib import Path
import sys
import tempfile

from audit_round1 import NEW, git, rename_fixture, run


def classify(tree, sources, *arguments):
    program = str(tree.resolve() / "tools/ci_plan.py")
    return json.loads(run(sources, sys.executable, program, "--event", "pull_request", *arguments).stdout)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for role in ("a", "b", "main"):
        parser.add_argument(f"--{role}", type=Path, required=True)
    args = parser.parse_args()
    affected_asset = "crates/nexus-web-core/src/embedded-note.md"
    report = {}
    with tempfile.TemporaryDirectory(prefix="nexus-audit-edges-") as root:
        repository = Path(root)
        before, after = rename_fixture(repository)
        for name in ("a", "b", "main"):
            tree = getattr(args, name)
            report[name] = {
                "sha": git(tree, "rev-parse", "HEAD"),
                "possible_embedded_markdown": classify(tree, tree.resolve(), "--paths-json",
                                                       json.dumps([affected_asset])),
                "rename": classify(tree, repository, "--base", before, "--head", after),
            }
    assert report["main"]["rename"]["paths"] == [NEW]
    assert not report["main"]["rename"]["plan"]["web"]
    assert report["main"]["possible_embedded_markdown"]["plan"]["web"]
    assert all(not report[key]["possible_embedded_markdown"]["plan"]["web"] for key in ("a", "b"))
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
