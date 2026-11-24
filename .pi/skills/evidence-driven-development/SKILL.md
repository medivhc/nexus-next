---
name: evidence-driven-development
description: "Experimental workflow for ambiguous or high-consequence behavior: define independent contracts, execute a minimal oracle, search counterexamples, repair and verify with replayable evidence. Use for authorization, data preservation, state machines, dependency selection and protocol invariants; skip unnecessary modeling for low-risk mechanical changes."
---

# Evidence-driven development (V1, experimental)

Read only the relevant product/module contract, interfaces, tests and implementation. Use existing repository tools. Do not introduce another framework. Do not claim this skill is cheaper or generally superior before paired evaluation.

## 1. Select a route before editing

| Situation | Route |
| --- | --- |
| Mechanical/local low-risk change, clear contract | Ordinary targeted checks; skip model. |
| High-risk invariant with independent expected behavior | Minimal executable model or property; continue below. |
| Timing, concurrency or distributed behavior | Use traces, bounded model, simulation, or repeated controlled measurement as appropriate; report limits. |
| Ambiguous or contradictory product requirements | BLOCKED for the affected claim; clarify the contract before declaring a defect. |

Existing Nexus verification: use `.pi/skills/formal-verify-nexus/SKILL.md` for relevant authorization model checking and `.pi/skills/verify-nexus/SKILL.md` for real user-path evidence. These are complementary, not replacements. Inspect actual tool availability and commands before running.

## 2. Establish a contract and a frozen baseline

Record baseline SHA, affected boundary, independent contract source, expected observable behavior, assumptions and unknowns. Define invariants from the contract rather than copying production branches. State input bounds and excluded behavior. If the contract itself is unresolved, stop the affected work rather than inventing an oracle.

## 3. Make feedback executable

Choose the cheapest meaningful oracle: dependency closure, truth table, state machine, reference algorithm, property, bounded exhaustive enumeration or real-world trace. Prefer the standard library and existing tests. Build a witness search that yields an input, expected result, actual result, seed (if applicable), and replay command. Run against unchanged production code before repair. A timeout, compilation failure, missing tool or unrelated assertion is BLOCKED, not a counterexample. An explored domain with no violation is not universal correctness.

For risky changes, separate contract/judge and solver roles where possible; isolate judge files and freeze their digest before editing production. If the same agent performs both roles, record this limitation. Do not weaken the oracle to pass a repair. Any contract correction must be separately justified and both baseline and candidate recalibrated.

## 4. Repair or stop

- Defect-specific counterexample: make the smallest production repair, preserving the judge; replay the witness and relevant regressions.
- No counterexample: report checked scope and uncertainty; do not invent a bug or make speculative production changes.
- A failing candidate: inspect the evidence, then repair; after repeated failures, escalate to a stronger model or human review rather than retrying blindly.
- Unavailable execution environment: deliver reproducible commands and BLOCKED, never fabricated PASS.

For a new feature without a buggy historical baseline, use a deliberately failing preimplementation check or calibrated negative control where meaningful; do not label missing functionality an independently discovered defect.

## 5. Schedule by dependencies, not agent count

Parallelize independent tasks/worktrees only when contracts and write sets do not conflict. Dependent repairs must wait for their prerequisites. Run affected tests first and use existing CI impact planning; unknown impact fails closed. Reuse evidence only when its relevant inputs, dependency closure, judge, toolchain, settings, target and environment match. Reuse build outputs only under valid cache keys. Required integration gates still run; do not remove them or turn an unrelated flake into a PASS. Control shared-resource contention when comparing timing.

## 6. Deliver evidence, not assertions

Record in the task output or existing evidence directory:
- route and why, contract source, baseline and candidate SHA (or diff digest);
- invariant, oracle path/digest, search domain and exclusions;
- baseline witness and candidate replay commands, exit codes and relevant logs;
- affected regressions and remaining integration gates;
- separate per-check PASS / FAIL / BLOCKED / UNKNOWN, and overall decision;
- wall time, model-reported input/output/cached tokens and tool calls if available; missing values are null, not zero.

Keep production diffs and regression tests, not disposable framework code or secrets. A PASS applies only to the named check and explored scope. Report task completion separately from code review/merge readiness.

## Worked route selection

- A typo: ordinary check, no model.
- CI dependency omission: graph closure from manifests; distinguish old tests passing from the defect-specific witness failing.
- File merge conflict: independently specify data preservation; enumerate collision cases, replay on original and repaired code.
- Real network latency: repeat controlled measurements; do not turn a deterministic functional model into a latency guarantee.
- Unclear ownership policy: clarify contract, no speculative repair.

## Evaluation status

This skill is provisional. See `docs/agent-methodology-v1.md` for evidence and the frozen independent A/B pilot. Do not install this skill in the A/B baseline or give it to the control arm. Do not cite these historical examples to solvers of the blinded/held-out tasks. The economic pilot evaluates whether the method is worth its overhead; it is not a requirement that every task use a model.
