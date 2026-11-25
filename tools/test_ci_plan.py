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
        # nexus-cli depends on nexus-drive-core, and the web_drive scenarios
        # run nexus-web-core (which depends on it) directly: compat and web
        # must run when it changes.
        self.assertEqual(classify(["crates/nexus-drive-core/src/lib.rs"], "pull_request"), Plan(True, True, True, True))

    def test_sys_real_and_sys(self):
        # nexus-sys-real is linked into the nexus binary the compat job
        # builds, and it depends on nexus-sys: both get compat (web already).
        self.assertEqual(classify(["crates/nexus-sys-real/src/lib.rs"], "pull_request"), Plan(True, True, True, True))
        self.assertEqual(classify(["crates/nexus-sys/src/lib.rs"], "pull_request"), Plan(True, True, True, True))

    def test_full_only_crates(self):
        # Test-only / platform-only code: full lane, no compat/web.
        for crate in ("nexus-sys-sim", "nexus-testkit", "nexus-android"):
            with self.subTest(crate=crate):
                self.assertEqual(classify([f"crates/{crate}/src/lib.rs"], "pull_request"), Plan(True, True, False, False))

    def test_unknown_crate_fails_closed(self):
        # A crate added without updating the planner lists runs every lane
        # instead of silently skipping compat/web.
        self.assertEqual(classify(["crates/nexus-vault/src/lib.rs"], "pull_request"), Plan(True, True, True, True))

    def test_unknown_tools_script_fails_closed(self):
        self.assertEqual(classify(["tools/new_tool.py"], "pull_request"), Plan(True, True, True, True))

    def test_crate_md_is_not_document(self):
        # A .md file inside a crate may be embedded or otherwise
        # load-bearing; only root docs and docs/ skip the build.
        self.assertEqual(classify(["crates/nexus-wire/README.md"], "pull_request"), Plan(True, True, True, True))
        self.assertEqual(classify(["docs/guide.md"], "pull_request"), Plan())

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

    def test_unexpected_needs_keys_fail(self):
        needs = self.make_needs()
        needs["extra-job"] = {"result": "success"}
        self.assertTrue(check(needs))
        needs = self.make_needs()
        del needs["web"]
        self.assertTrue(check(needs))


if __name__ == "__main__":
    unittest.main()
