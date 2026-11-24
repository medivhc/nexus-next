#!/usr/bin/env python3
"""No-network, no-compiler tests for CI selection and its required gate."""
import unittest
from pathlib import Path
import tomllib

from ci_gate import check
from ci_plan import Plan, classify


# Executable model of the Web lane's build-time dependencies. Derive the
# graph from Cargo manifests, not from the planner's manually maintained sets.
# Include every target's normal/build dependencies conservatively; dev-only
# dependencies are not part of the two cargo builds in the Web lane.
def web_build_closure():
    root = Path(__file__).resolve().parents[1]
    workspace = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    inherited = workspace.get("workspace", {}).get("dependencies", {})
    manifests = {}
    for manifest in (root / "crates").glob("*/Cargo.toml"):
        data = tomllib.loads(manifest.read_text(encoding="utf-8"))
        manifests[data["package"]["name"]] = (manifest.parent.name, data)

    def dependencies(data):
        for scope in [data, *data.get("target", {}).values()]:
            for section in ("dependencies", "build-dependencies"):
                for alias, spec in scope.get(section, {}).items():
                    if isinstance(spec, dict) and spec.get("workspace"):
                        spec = inherited[alias]
                    name = spec.get("package", alias) if isinstance(spec, dict) else alias
                    if name in manifests:
                        yield name

    # The Web job builds both the WASM frontend and the native nexus CLI.
    pending = ["nexus-web", "nexus-cli"]
    visited = set()
    while pending:
        name = pending.pop()
        if name in visited:
            continue
        visited.add(name)
        _, data = manifests[name]  # missing root is a hard failure, not a skip
        pending.extend(dependencies(data))
    return {manifests[name][0] for name in visited}


class PolicyTests(unittest.TestCase):
    def test_docs_do_not_build(self):
        self.assertEqual(classify(["README.md", "docs/testing.md", "tasks/ci/notes.txt"], "pull_request"), Plan())

    def test_web_only(self):
        self.assertEqual(classify(["web/index.html", "crates/nexus-web/src/lib.rs"], "pull_request"), Plan(smoke=True, web=True))

    def test_web_core(self):
        # testkit 的浏览器场景在 L2/L3 里直接跑网页节点核心。
        self.assertEqual(classify(["crates/nexus-web-core/src/lib.rs"], "pull_request"), Plan(smoke=True, full=True, web=True))

    def test_manual_is_executed(self):
        # L3 用户手册照着 docs/manual.md 做一遍：改手册要跑 Full。
        self.assertEqual(classify(["docs/manual.md"], "pull_request"), Plan(smoke=True, full=True))

    def test_rust_wire(self):
        self.assertEqual(classify(["crates/nexus-wire/src/lib.rs"], "pull_request"), Plan(True, True, True, True))

    def test_runtime(self):
        self.assertEqual(classify(["crates/nexus-runtime/src/lib.rs"], "pull_request"), Plan(True, True, True, True))

    def test_drive_core(self):
        self.assertEqual(classify(["crates/nexus-drive-core/src/lib.rs"], "pull_request"), Plan(True, True, False, True))

    def test_web_build_dependencies_are_never_skipped(self):
        # The contract is independent of the classifier: modifying any crate
        # used to build the browser or its native peer requires the Web lane.
        closure = web_build_closure()
        self.assertIn("nexus-drive-core", closure)
        for crate in sorted(closure):
            path = f"crates/{crate}/Cargo.toml"
            with self.subTest(crate=crate):
                self.assertTrue(classify([path], "pull_request").web,
                                f"counterexample: {path} affects a Web build")

    def test_ci_change_triggers_all(self):
        self.assertEqual(classify([".github/workflows/ci.yml"], "pull_request"), Plan(True, True, True, True))
        self.assertEqual(classify(["tools/ci_plan.py"], "pull_request"), Plan(True, True, True, True))

    def test_unknown_fails_closed(self):
        self.assertEqual(classify(["some-new-dir/file.bin"], "pull_request"), Plan(True, True, True, True))

    def test_lockfile_triggers_all(self):
        self.assertEqual(classify(["Cargo.lock"], "pull_request"), Plan(True, True, True, True))

    def test_scheduled_and_main_full(self):
        for event in ("push", "schedule", "workflow_dispatch"):
            with self.subTest(event=event):
                self.assertEqual(classify([], event), Plan(True, True, True, True))

    def test_executable_under_tasks_is_not_skipped(self):
        self.assertEqual(classify(["tasks/ci/run.py"], "pull_request"), Plan(True, True, True, True))

    def test_invalid_path(self):
        with self.assertRaises(ValueError):
            classify(["../etc/passwd"], "pull_request")


class GateTests(unittest.TestCase):
    def make_needs(self, *, smoke=False, full=False, compat=False, web=False):
        flags = {"smoke": smoke, "full": full, "compat": compat, "web": web}
        return {
            "plan": {"result": "success", "outputs": {k: str(v).lower() for k, v in flags.items()}},
            "sanity": {"result": "success"},
            **{job: {"result": "success" if active else "skipped"} for job, active in (("smoke", smoke), ("ci", full), ("compat", compat), ("web", web))},
        }

    def test_all_skipped_intentionally(self):
        self.assertEqual(check(self.make_needs()), [])

    def test_full_runs_successfully(self):
        self.assertEqual(check(self.make_needs(smoke=True, full=True, compat=True, web=True)), [])

    def test_unexpected_skip_fails(self):
        needs = self.make_needs(full=True)
        needs["ci"]["result"] = "skipped"
        self.assertTrue(check(needs))

    def test_failure_or_cancel_fails(self):
        for result in ("failure", "cancelled"):
            needs = self.make_needs(smoke=True)
            needs["smoke"]["result"] = result
            self.assertTrue(check(needs))

    def test_failed_plan_fails(self):
        needs = self.make_needs()
        needs["plan"]["result"] = "failure"
        self.assertTrue(check(needs))

    def test_unknown_plan_value_does_not_pass(self):
        for selected in (True, False):
            needs = self.make_needs(smoke=selected)
            needs["plan"]["outputs"]["smoke"] = "typo"
            self.assertTrue(check(needs))


if __name__ == "__main__":
    unittest.main()
