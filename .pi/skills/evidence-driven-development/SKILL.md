---
name: evidence-driven-development
description: Select and execute proportionate validation for engineering changes. Use for bug fixes, verification planning, data preservation, state machines, performance measurement, dependency selection and protocol invariants. Record an executable witness, the evidence limits, and the conditions for escalation.
---

# Evidence-driven development V2

1. Record the observable contract, its source, the baseline and affected boundary. Start with the cheapest check that could disprove the claim.
2. Choose validation for the specific risk.

   | Evidence needed | Start with |
   | --- | --- |
   | One clear rule or localized case | Targeted regression or replay |
   | Interacting states, ordering, graph edges, retries or data-loss risks | An independent property, table, trace or bounded model |
   | Real timing, concurrency, network or device behavior | A controlled runtime measurement. Use a deterministic trace to isolate mechanism if useful |
   | Conflicting contract or materially incomplete environment | Record the unresolved claim and continue independent checks |

3. For a suspected defect, execute the smallest witness on unchanged code before editing. Record input, expected and actual outcomes, replay command, and checked bounds. Promote a targeted check to an independent property when an interaction defeats it or high-impact edges remain untested. Stop adding cases once the check distinguishes the defect, a nearby negative control, and the material interactions. An independently derived, already-calibrated judge can serve as the property.
4. Freeze independent judge inputs before the repair. Keep them separate from the solver in comparative runs. Keep the fix scoped to the observed defect, replay the witness, and run impacted regressions. Treat setup errors, timeouts and ambiguous outcomes separately. Keep runtime claims pending if the runtime boundary was simulated.
5. On failed replay, diagnose the new evidence before retrying. Escalate resources or model capability when a materially different attempt needs them. Record the trigger and the cost already spent. Reuse evidence only for matching relevant source, dependency inputs, judge, toolchain and environment. Run required integration gates for production acceptance.
6. Report PASS, FAIL, BLOCKED or UNKNOWN per claim. Include baseline and candidate identity, replay, scope, exclusions, and measured time or usage. Leave unobserved usage unknown. For advice on an unverified contract, record the question separately from code correctness.

For a repeatable route decision, run [the validation selector](scripts/select_validation.py)
with explicit contract, interaction, runtime and witness facts. Use a runtime
replay across real subprocess, file or network boundaries when a test double
fixes the interface the repair might legitimately change. Calibrate the runtime
fixture with a known-good control and a defect witness. Report the fixture
boundary and keep required integration checks visible.
