#!/usr/bin/env python3
"""Select the next validation step from observed engineering evidence."""
import argparse
import json


def select(*, contract, runtime_claim, runtime_available, interaction, witness,
           witness_executed, residual_risk, evidence_matches, required_gate):
    """Choose investigation steps. Only actual check results support acceptance."""
    if contract == "unclear":
        return {"next": "resolve_contract", "claim": "blocked", "required_gate": required_gate}
    if witness in {"setup_failed", "timeout"}:
        return {"next": "diagnose_execution", "claim": "blocked", "required_gate": required_gate}

    if interaction and (witness != "discriminates" or residual_risk):
        step = "independent_property"
    elif witness == "discriminates" and (witness_executed or evidence_matches):
        step = "replay_witness_and_impacted_checks"
    else:
        step = "targeted_check"
    runtime = "not_requested"
    if runtime_claim:
        runtime = "controlled_runtime_measurement" if runtime_available else "blocked"
    return {"next": step, "runtime_evidence": runtime, "required_gate": required_gate}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--contract", choices=("clear", "unclear"), required=True)
    parser.add_argument("--runtime-claim", action="store_true")
    parser.add_argument("--runtime-available", action="store_true")
    parser.add_argument("--interaction", action="store_true")
    parser.add_argument("--witness",
                        choices=("missing", "discriminates", "inconclusive", "setup_failed", "timeout"),
                        default="missing")
    parser.add_argument("--witness-executed", action="store_true", help="observed on this baseline")
    parser.add_argument("--residual-risk", action="store_true", help="a material interaction remains")
    parser.add_argument("--evidence-matches", action="store_true",
                        help="source boundary, dependency inputs, judge, target and environment match")
    parser.add_argument("--required-gate", action="store_true")
    print(json.dumps(select(**vars(parser.parse_args())), sort_keys=True))


if __name__ == "__main__":
    main()
