#!/usr/bin/env python3
"""No-network, no-compiler tests for CI selection and its required gate."""
import unittest

from ci_gate import check
from ci_plan import Plan, classify


class PolicyTests(unittest.TestCase):
    def test_docs_do_not_build(self):
        self.assertEqual(classify(["README.md", "docs/testing.md", "tasks/ci/notes.txt"], "pull_request"), Plan())

    def test_web_only(self):
        self.assertEqual(classify(["web/index.html", "crates/nexus-web/src/lib.rs"], "pull_request"), Plan(smoke=True, web=True))

    def test_web_core(self):
        self.assertEqual(classify(["crates/nexus-web-core/src/lib.rs"], "pull_request"), Plan(smoke=True, web=True))

    def test_rust_wire(self):
        self.assertEqual(classify(["crates/nexus-wire/src/lib.rs"], "pull_request"), Plan(True, True, True, True))

    def test_runtime(self):
        self.assertEqual(classify(["crates/nexus-runtime/src/lib.rs"], "pull_request"), Plan(True, True, True, True))

    def test_drive_core(self):
        self.assertEqual(classify(["crates/nexus-drive-core/src/lib.rs"], "pull_request"), Plan(True, True, False, False))

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
