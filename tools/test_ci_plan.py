#!/usr/bin/env python3
"""No-network, no-compiler tests for CI selection and its required gate."""
from pathlib import Path
from tempfile import TemporaryDirectory
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
        # nexus-web-core uses Drive in the WASM build.
        self.assertEqual(
            classify(["crates/nexus-drive-core/src/lib.rs"], "pull_request"),
            Plan(smoke=True, full=True, web=True),
        )

    def test_crates_outside_web_build(self):
        for crate in ("nexus-android", "nexus-sys-sim", "nexus-testkit"):
            with self.subTest(crate=crate):
                self.assertEqual(
                    classify([f"crates/{crate}/src/lib.rs"], "pull_request"),
                    Plan(smoke=True, full=True),
                )

    def test_web_build_tracks_new_path_dependencies(self):
        manifests = {
            ".": '[workspace.dependencies]\nshared = { path = "crates/shared" }\n',
            "crates/nexus-web": '[dependencies]\nshared.workspace = true\n',
            "crates/nexus-cli": '[dev-dependencies]\ndev = { path = "../dev" }\n',
            "crates/shared": (
                "[target.wasm32-unknown-unknown.build-dependencies]\n"
                'helper = { path = "../../tools/build-helper", package = "build-helper" }\n'
            ),
            "tools/build-helper": '[dependencies]\nleaf = { path = "../../crates/leaf" }\n',
            "crates/leaf": "",
            "crates/dev": "",
        }
        with TemporaryDirectory() as temp:
            root = Path(temp)
            for directory, manifest in manifests.items():
                path = root / directory / "Cargo.toml"
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(manifest, encoding="utf-8")

            for path in ("crates/shared/src/lib.rs", "tools/build-helper/src/lib.rs", "crates/leaf/src/lib.rs"):
                with self.subTest(path=path):
                    self.assertEqual(
                        classify([path], "pull_request", root=root),
                        Plan(smoke=True, full=True, web=True),
                    )
            self.assertEqual(
                classify(["crates/dev/src/lib.rs"], "pull_request", root=root),
                Plan(smoke=True, full=True),
            )

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
