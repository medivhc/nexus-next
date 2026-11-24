#!/usr/bin/env python3
"""Mandatory all-lane gate; skipped jobs pass only when explicitly planned off."""
from __future__ import annotations

import json
import os
import sys


def check(needs: dict) -> list[str]:
    errors: list[str] = []
    if needs.get("plan", {}).get("result") != "success":
        return ["CI plan failed; cannot trust selective skips"]
    for job, key in (
        ("sanity", None), ("smoke", "smoke"), ("ci", "full"),
        ("compat", "compat"), ("web", "web")
    ):
        actual = needs.get(job, {}).get("result")
        value = needs["plan"].get("outputs", {}).get(key) if key else "true"
        if value not in ("true", "false"):
            errors.append(f"{job}: invalid plan output {key}={value!r}")
            continue
        expected = "success" if value == "true" else "skipped"
        if actual != expected:
            errors.append(f"{job}: expected {expected}, got {actual}")
    return errors


def main() -> None:
    needs = json.loads(os.environ["CI_NEEDS_JSON"])
    errors = check(needs)
    if errors:
        print("CI gate FAILED: " + "; ".join(errors), file=sys.stderr)
        sys.exit(1)
    print("CI gate passed: all planned jobs succeeded; only intentionally excluded jobs were skipped")


if __name__ == "__main__":
    main()
