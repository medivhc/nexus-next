"""Run against a candidate by setting TARGET to its sched_bench.py."""
import os
from pathlib import Path
import subprocess
import sys
import unittest

TARGET = Path(os.environ["TARGET"]).resolve()
REJECT = (
    ("--seconds", "5"),
    ("--seconds", "nan"),
    ("--seconds", "inf"),
    ("--rate-mbit", "0"),
    ("--rate-mbit", "-1"),
    ("--rate-mbit", "inf"),
    ("--max-added-ms", "-0.1"),
    ("--max-added-ms", "nan"),
    ("--attempts", "0"),
    ("--attempts", "-1"),
)
ACCEPT = ((), ("--seconds", "5.25"), ("--rate-mbit", "0.5"),
          ("--max-added-ms", "0"), ("--attempts", "2"))

CHILD = """
import runpy
import sys
module = runpy.run_path(sys.argv[1])
def unavailable():
    print("ENVIRONMENT CHECK REACHED")
    return "controlled unavailable host"
module["main"].__globals__["missing_requirements"] = unavailable
sys.argv = sys.argv[1:]
sys.exit(module["main"]())
"""


class InputContract(unittest.TestCase):
    def run_cli(self, options):
        return subprocess.run([sys.executable, "-c", CHILD, str(TARGET), *options],
                              capture_output=True, text=True, check=False, timeout=5)

    def test_bad_parameters_rejected_before_environment_check(self):
        for option, value in REJECT:
            with self.subTest(option=option, value=value):
                result = self.run_cli((option, value))
                self.assertEqual(result.returncode, 2, result)
                self.assertNotIn("ENVIRONMENT CHECK REACHED", result.stdout)
                self.assertIn(option, result.stderr)
                self.assertNotIn("Traceback", result.stderr)

    def test_valid_parameters_keep_optional_skip(self):
        for options in ACCEPT:
            with self.subTest(options=options):
                result = self.run_cli(options)
                self.assertEqual(result.returncode, 0, result)
                self.assertIn("ENVIRONMENT CHECK REACHED", result.stdout)
