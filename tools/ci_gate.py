#!/usr/bin/env python3
"""Mandatory all-lane gate; skipped jobs pass only when explicitly planned off."""
from __future__ import annotations

import json
import os
import sys

# The exact set of jobs the gate job declares in needs:. Anything else means
# the workflow changed without updating this gate, which must fail closed.
EXPECTED_JOBS = {"plan", "sanity", "smoke", "ci", "compat", "web", "platforms"}


def check(needs: dict) -> list[str]:
    errors: list[str] = []
    if set(needs.keys()) != EXPECTED_JOBS:
        errors.append(
            f"unexpected needs keys: {sorted(needs.keys())!r}, "
            f"expected {sorted(EXPECTED_JOBS)!r}"
        )
    if needs.get("plan", {}).get("result") != "success":
        return errors + ["CI plan failed; cannot trust selective skips"]
    for job, key in (
        ("sanity", None), ("smoke", "smoke"), ("ci", "full"),
        ("compat", "compat"), ("web", "web"), ("platforms", "platforms")
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
