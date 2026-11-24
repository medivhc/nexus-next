#!/usr/bin/env python3
"""Apply one archived trial patch to its frozen baseline, then run a selected judge."""
import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile

EXPERIMENT = Path(__file__).resolve().parent
REPO = EXPERIMENT.parents[2]
BASE = "2b75c737122b32e3b81a6d493d5a87f146372560"
TARGET = "tools/sched_bench.py"
JUDGES = {"input": "test_input.py", "window": "test_window_loopback.py",
          "payload": "test_delayed_payload.py", "strategy": "test_strategy.py"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("trial", help="archived trial name, or baseline")
    parser.add_argument("--judge", choices=JUDGES, required=True)
    parser.add_argument("--check-only", action="store_true", help="verify patch applicability")
    args = parser.parse_args()
    patch = EXPERIMENT / "data" / "trials" / f"{args.trial}.patch"
    if args.trial != "baseline" and (patch.parent != EXPERIMENT / "data" / "trials" or not patch.is_file()):
        parser.error(f"unknown trial: {args.trial}")
    baseline = subprocess.check_output(["git", "show", f"{BASE}:{TARGET}"], cwd=REPO)
    with tempfile.TemporaryDirectory(prefix="nexus-trial-replay-") as directory:
        workspace = Path(directory)
        target = workspace / TARGET
        target.parent.mkdir()
        target.write_bytes(baseline)
        if args.trial != "baseline":
            subprocess.run(["git", "apply", str(patch)], cwd=workspace, check=True)
        if args.check_only:
            return 0
        result = subprocess.run(
            [sys.executable, "-B", "-m", "unittest", "discover", "-s",
             str(EXPERIMENT / "judges"), "-p", JUDGES[args.judge], "-v"],
            cwd=workspace, env={**os.environ, "TARGET": str(target)}, timeout=90,
        )
        return result.returncode


if __name__ == "__main__":
    sys.exit(main())
