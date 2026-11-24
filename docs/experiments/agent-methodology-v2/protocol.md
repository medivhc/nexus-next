# Round 2 protocol

Baseline `2b75c737122b32e3b81a6d493d5a87f146372560`. Use two previously
unfixed behaviors of the R44 scheduling measurement tool. These are unrelated to
the first-round browser-selection defect. The suite tests measurement logic on
controlled clocks and subprocess fixtures. It makes no claim about actual R44
performance on a shaped link.

| Task | Product boundary | Complexity assigned before runs |
| --- | --- | --- |
| input | Reject invalid measurement parameters before environmental checks, even on a host that cannot run the benchmark | Local rule and CLI integration |
| window | Ensure a sample labeled loaded is taken while the bulk transfer is active and within the configured measurement window | Timing, process lifecycle and aggregation |

For each task compare ordinary workflow A with the same task plus the V2 Skill B,
using GPT-6.2 Sol and GPT-6.2 Astra at the same low effort. The model comparison
concerns these two configurations only. Give both arms the same contract and
source files. Use fresh sessions, distinct archives without Git history or
judge files, the same environment, and up to 8 minutes per trial. Do not allow
external sources or access to other trials. Shared host access means isolation
is procedural. Dispatch in staggered AB/BA order, with two models in parallel
where possible. No hard token cap is claimed. One observation per cell is a pilot.

Freeze the judge before solving and require a defect-specific baseline signal,
with positive controls. Run the fixed judge once per submitted patch, counting
all attempts including infrastructure failures. Capture candidate patch,
model configuration, observed wall time, candidate and judge digest, exit status,
individual assertions, solver verification trace when available, and usage or
null. Primary measure is all assertions passed. Secondary measures are
time-to-passing candidate, verification route and observed cost. Compare within
task/model first. For comparable correctness, flag B overhead above both
60 seconds and 25% of A time. Treat this threshold as a decision aid, not a
statistical inference. Re-run real shaped-link acceptance separately if needed.
