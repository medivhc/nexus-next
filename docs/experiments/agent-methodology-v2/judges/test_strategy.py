"""Replay route selection against observed Nexus scenarios and failure states."""
from pathlib import Path
import runpy
import unittest

REPO = Path(__file__).resolve().parents[4]
select = runpy.run_path(str(
    REPO / ".pi/skills/evidence-driven-development/scripts/select_validation.py"
))["select"]


class Strategy(unittest.TestCase):
    def decide(self, **changes):
        facts = dict(contract="clear", runtime_claim=False, runtime_available=False,
                     interaction=False, witness="missing", witness_executed=False,
                     residual_risk=False, evidence_matches=False, required_gate=False)
        return select(**(facts | changes))

    def test_local_cli_contract(self):
        self.assertEqual(self.decide()["next"], "targeted_check")
        self.assertEqual(self.decide(witness="discriminates", witness_executed=True)["next"],
                         "replay_witness_and_impacted_checks")

    def test_cargo_and_git_interactions_require_independent_evidence(self):
        result = self.decide(interaction=True, witness="discriminates",
                             evidence_matches=True, residual_risk=True, required_gate=True)
        self.assertEqual(result["next"], "independent_property")
        self.assertTrue(result["required_gate"])

    def test_do_not_reuse_mismatched_ci_evidence(self):
        result = self.decide(interaction=True, witness="discriminates", evidence_matches=False)
        self.assertEqual(result["next"], "targeted_check")

    def test_timing_trace_preserves_real_measurement_requirement(self):
        available = self.decide(interaction=True, runtime_claim=True, runtime_available=True)
        blocked = self.decide(interaction=True, runtime_claim=True)
        self.assertEqual(available["next"], "independent_property")
        self.assertEqual(available["runtime_evidence"], "controlled_runtime_measurement")
        self.assertEqual(blocked["runtime_evidence"], "blocked")

    def test_ambiguity_and_timeout(self):
        self.assertEqual(self.decide(contract="unclear")["next"], "resolve_contract")
        self.assertEqual(self.decide(witness="timeout")["next"], "diagnose_execution")
