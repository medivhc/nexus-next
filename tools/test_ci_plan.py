#!/usr/bin/env python3
"""No-network, no-compiler tests for CI selection and its required gate."""
import json
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

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
        for path in ("src/lib.rs", "Cargo.toml"):
            with self.subTest(path=path):
                self.assertEqual(
                    classify([f"crates/nexus-drive-core/{path}"], "pull_request"),
                    Plan(smoke=True, full=True, web=True),
                )

    def test_new_transitive_build_dependencies_select_web(self):
        manifests = {
            "Cargo.toml": '[workspace.dependencies]\nalias = { path = "crates/shared", package = "shared" }\n',
            "crates/nexus-web/Cargo.toml": "[dependencies]\nalias.workspace = true\n",
            "crates/nexus-cli/Cargo.toml": '[dev-dependencies]\nhelper = { path = "../helper" }\n',
            "crates/shared/Cargo.toml": (
                '[target.\'cfg(target_arch = "wasm32")\'.build-dependencies]\n'
                'generator = { path = "../generator" }\n'
            ),
            "crates/generator/Cargo.toml": "",
            "crates/helper/Cargo.toml": "",
        }
        with TemporaryDirectory() as directory:
            root = Path(directory)
            for name, contents in manifests.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(contents, encoding="utf-8")

            with patch("ci_plan.ROOT", root):
                self.assertTrue(classify(["crates/shared/src/lib.rs"], "pull_request").web)
                self.assertTrue(classify(["crates/generator/build.rs"], "pull_request").web)
                self.assertFalse(classify(["crates/helper/src/lib.rs"], "pull_request").web)
                self.assertFalse(classify(["crates/generator-extra/src/lib.rs"], "pull_request").web)

    def test_move_out_of_web_dependency_selects_web(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            old = "crates/nexus-web-core/src/drive.rs"
            new = "crates/nexus-testkit/src/renamed_drive.rs"

            def git(*args):
                return subprocess.check_output(
                    [
                        "git", "-c", "user.name=CI test", "-c", "user.email=ci@example.invalid",
                        "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", *args,
                    ],
                    cwd=root, text=True,
                ).strip()

            git("init", "--quiet")
            (root / old).parent.mkdir(parents=True)
            (root / old).write_text("mod drive;\n", encoding="utf-8")
            git("add", old)
            git("commit", "--quiet", "-m", "Add browser input")
            base = git("rev-parse", "HEAD")
            (root / new).parent.mkdir(parents=True)
            git("mv", old, new)
            git("commit", "--quiet", "-m", "Move browser input")
            head = git("rev-parse", "HEAD")

            script = Path(__file__).with_name("ci_plan.py").resolve()
            result = subprocess.check_output(
                [sys.executable, str(script), "--event", "pull_request", "--base", base, "--head", head],
                cwd=root, text=True,
            )
            replay = json.loads(result)
            self.assertCountEqual(replay["paths"], [old, new])
            self.assertTrue(replay["plan"]["web"])

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
