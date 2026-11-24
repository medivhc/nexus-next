# Evidence-driven agent development — V1 (experimental)
Status: provisional, 2026-11-24. This is a method to evaluate, not a claim of universal superiority.

## Method
1. Define the observable contract, allowed states/transitions, invariants and scope from product requirements and independent sources. Separate assumptions from facts. Select high-risk ambiguities, not every line of code.
2. Build the smallest executable intermediate model or oracle that can discriminate behavior: dependency graph, state machine, reference implementation, property or finite exhaustive enumeration. Derive expected results from the contract, not the implementation under test. Do not duplicate the production algorithm as the oracle.
3. Execute on the unchanged baseline; record counterexample (input, actual, expected, seed and replay command). A compile error, timeout, unavailable dependency or unrelated assertion is BLOCKED, not evidence of a defect. If no counterexample is found, record the explored scope; do not infer correctness.
4. Give the counterexample to the implementation agent. Make the smallest production change; preserve the independent judge. Prefer separate model/judge and implementation roles for high-risk changes; a separate process or author alone does not guarantee semantic independence.
5. Replay the counterexample, run affected regressions and verify the contract. Retain a regression in the appropriate test layer. Escalate only on ambiguous contracts, broader impact or repeated failure.
6. Integrate with risk-based CI and record evidence (baseline/candidate SHA, contract source, model version, replay, result, coverage scope, tool/runtime versions, open uncertainty). Evidence status: PASS / FAIL / BLOCKED / UNKNOWN. Full integration requirements and protected gates remain intact.

### When to use
Default for authorization, data preservation, state transitions, dependency selection, protocol invariants and other high-consequence behavior with an independent oracle. Ordinary lint, mechanical changes and low-risk localized fixes should use ordinary fast checks unless a model has demonstrable value. Timing and distributed systems may need traces, simulation or statistical testing rather than a small deterministic model. Do not introduce a model where its construction costs more than the expected value or the contract cannot be established.

### Scheduling
Run independent modeling and fixes concurrently in separate worktrees, respecting dependency and file-overlap conflicts. Run only impacted tests when safe; fail closed on unknown impact. Share builds/evidence only with matching source/dependency closure, configuration, toolchain, target, environment and judge identity; do not use cached evidence to bypass required integration checks. Parallelize only within resource limits; for time comparisons use matched resources or counterbalanced serial order.

## Evidence so far
- Authorization: PR #20, full CI success: https://github.com/medivhc/nexus-next/actions/runs/45397816553
- CI dependency and Drive merge: two real-code calibration jobs independently demonstrated old suite green, defect-specific oracle red, patched suite green: https://github.com/medivhc/nexus-next/actions/runs/45403680386. CI policy bug had previously been raised; Drive collision was a new counterexample in the recorded investigation. This supports incremental diagnostic value, not comparative agent productivity or a general proof.
- Drive PR #22 full CI success: https://github.com/medivhc/nexus-next/actions/runs/45399307890. PR #21 full CI failed scheduling performance; separate retest passed 5/5 (added P95 7–8 ms against 150 ms bound), with identical Rust sources and lockfile: https://github.com/medivhc/nexus-next/actions/runs/45403065060. An intermittent issue is not ruled out and the original gate is not retroactively green.

## Economic experiment: preregistered pilot
Reuse the existing agent_benchmark.py V2 runner and its calibrated judge; no new harness needed.
Baseline: a54db991393645db5e9f13888fcc79b4a2d5cf3f. Tasks: ci (judge dd2fb2cf2fa12c06e42b722c0e4626f44ffa5a00; corrected d3ee611b140f45c2c4357fa09a45f29535316601), drive (judge ef0b80e7cdf59b603c5d8ef435e8a28a1adfec28; corrected 49bc7e134b98b9a9f1510d67adb48e1b2b7f668d). These are retrospective cases, not held-out evidence.
- A: normal coding workflow; allow naturally chosen modeling. B: explicitly require executable model and pre-edit counterexample. Same task description, model/version/effort, tools, baseline and time budget. Fresh isolated sessions, no fix refs, judge tests withheld until solver exits, no upstream access to solutions. Record any leakage.
- First pair on ci only, then drive if pilot instrumentation works. Repeat three pairs per task if informative; balance order, control resource contention and cache conditions. Use targeted judges only. Precalibrate judge on buggy and corrected revisions. Report all attempts, including failures/timeouts; do not cherry-pick.
- Primary: judge pass rate. Secondary: time to valid solution (including modeling, execution and repairs), actual input/output/cached token counts, tool calls, repair iterations, model compliance evidence, infrastructure compute/wait time separately. Record missing data as null. Token budgets requested by CLI are not enforced caps; do not claim equal enforced token budgets. Do not equate tokens with currency without pricing.
- Decision: continue to held-out tasks if correctness is comparable or improved and cost is acceptable; restrict to high-risk classes if overhead dominates simple tasks; revise/stop if independent model produces unreliable feedback or worse outcomes. No superiority claim from two retrospective tasks. Select a practical cost threshold before held-out evaluation; report uncertainty rather than treating a small pilot as statistically conclusive.
- For portability, after same-model A/B, use a 2x2 model-capability x method design on identical held-out tasks.
- Status: real-code judge calibration passed. Independent Agent runs not yet started; this environment lacks an authenticated Agent CLI/API executor. No token or agent time measurements exist.

### Run on an authorized developer machine
Use the existing V2 bundle, fetch both PR heads into a local checkout, run its self-test and per-task calibrate command. Then:
```sh
python3 agent_benchmark.py run --repo "$REPO" --out "$OUT" --task ci --arm A --model "$MODEL"
python3 agent_benchmark.py run --repo "$REPO" --out "$OUT" --task ci --arm B --model "$MODEL"
python3 agent_benchmark.py report --out "$OUT"
```
Use separate fresh sessions and matched resources; swap order on repeats. The agent cannot see this experiment branch, evaluator refs or earlier solutions. Record actual run metadata. Repeat for drive only after pilot review.

## V1 exit criteria
The written workflow is a provisional skill candidate. Promote it only after reproducible pilot evidence, review of independent model quality and an explicit decision on applicable task classes. A future skill should wrap existing tools, return replayable evidence, and avoid building a second CI system.
