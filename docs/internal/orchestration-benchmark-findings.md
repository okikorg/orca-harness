# Orchestration benchmarks: tokens, cost, latency, and correctness

## Executive summary

These exploratory benchmarks compare direct task execution with delegation in Orca. The economically relevant comparison is **Astra alone versus Astra coordinating exactly one cheaper worker**, not merely enabling orchestration mode or using the same model for every agent.

Findings:

- **Lower cost does not imply fewer tokens.** Cheaper workers and cached inputs can reduce dollars while increasing total session tokens.
- **Delegation is not automatically faster.** GLM performed many sequential model calls; Luna was substantially faster in the sampled runs.
- **Parent-only usage is misleading.** Count parent and worker input, output, cache reads, and cache writes. Reasoning is an output-token subset, not an additional billable category.
- **A real configuration gap inflated worker work:** routed workers discarded explicit effort/output limits. Same-provider workers now inherit them, with an exception for Codex's unsupported output cap.
- **Correctness must be evaluated separately from process completion and cost.** Some workers completed successfully but left defects. Some evaluator assertions were stricter than the written requirements.
- **These are single trials on small synthetic tasks**, not statistically reliable model rankings or production cost forecasts.

No production deployment or commit was performed as part of these experiments. Code changes described below were implemented and locally tested; the release CLI was rebuilt before subsequent trials.

## Method and measurement contract

### Main comparison

| Arm | Parent | Worker | Strategy |
|---|---|---|---|
| Direct | `openai/gpt-6-astra` through OpenRouter | None | Parent investigates, implements, and tests |
| Mixed | Same Astra parent/provider | Exactly one cheaper foreground worker | Worker implements/tests; parent reviews read-only |

Workers tested through OpenRouter:

- `z-ai/glm-5.3-flash`
- `openai/gpt-5.6-luna`

Early exploratory runs also used `anthropic/claude-haiku-4.5` throughout, Astra throughout, and Astra through the Codex subscription provider. Those are distinct experiments, not interchangeable baselines.

### Controls for the later coding trials

- Fresh identical copies of the original buggy fixture for each arm; manually repaired copies were never used as benchmark inputs.
- Same base task, requirements, model settings, and external checks per matched pair.
- Maximum 40 parent/worker steps; requested low effort and 4,096 output tokens per response.
- Worker timeout 300 seconds; process timeout 600 seconds; depth one.
- One worker/model/tool attempt, retry extension disabled, no benchmark retries.
- Worker routing verified from returned identity when the worker completed.
- One run per task/arm. Usually direct first; the final two-task experiment alternated ordering across tasks.
- Private configuration copies were removed afterward; real configuration and production source were not changed by benchmark execution.

**Historical qualification:** before the worker-budget fix, the requested effort/output settings reached the parent but were discarded for routed workers. Do not interpret those earlier runs as having the same effective worker settings.

### Accounting

- Parent headless `summary.usage` is parent-only.
- Add each unique executed worker's usage exactly once.
- Total reported token categories = uncached input + output + cache reads + cache writes.
- Do not add reasoning tokens again: they are included in output.
- Estimated USD = the sum of each model's token classes multiplied by its own published rates.
- Catalog snapshots and retrieval timestamps are stored with each run. Estimates are **not provider invoices**.
- A zero cache field is different from an unavailable field. Early worker results omitted cache usage, making those mixed totals incomplete.
- External monotonic wall time covers the CLI process and foreground worker waiting. Do not add parent duration and worker duration: they overlap.
- Setup/controller agents, builds, catalog retrieval, and external grading are excluded from benchmark session usage/time.

## Experiment history

### Small log-correlation task

Task: correlate request `req-1042` across gateway and ledger logs, identify the failing dependency/root error/attempt count/final status, and return an exact final line.

| Experiment | Direct tokens | Orchestration tokens | Direct / orchestration time | Outcome |
|---|---:|---:|---|---|
| Haiku, mode flag only | 11,047 | 11,598 | 6.505 / 6.251 s | **Invalid delegation comparison:** neither arm spawned a worker |
| Haiku, two required Haiku workers | 9,094 | 17,820* | 5.695 / 10.435 s | Both correct; two workers completed |
| Haiku, one required Haiku worker | 11,544 | 12,282* | 6.726 / 9.060 s | Both correct; estimated cost $0.013540 / $0.016210* |
| Astra through Codex, one Astra worker | 5,418 | 7,755* | 10.290 / 27.573 s | Both correct; exact-model subscription-channel USD unavailable |
| Astra through OpenRouter, one Astra worker | 5,424 | 4,908* | 7.125 / 20.812 s | Both correct; estimated cost $0.033533 / $0.065749* |
| Astra through OpenRouter, one GLM worker | 5,654 | 12,998* | 9.301 / 18.880 s | Both correct; estimated cost $0.034599 / $0.052401* |

\* Worker cache categories were unavailable. These are exposed totals/partial estimates, **not evidence of full token or cost savings**. The apparent token reduction in the OpenRouter Astra-to-Astra row cannot be interpreted as a saving.

The first trial also had a strict-format failure in the direct answer, despite a correct diagnosis. Subsequent trials required actual substantive delegation and separate correctness checks.

### Multi-step invoice task

A dependency-free Python package split across money, discounts, shipping, and invoice modules. The agents had to inspect README rules, repair interacting defects, add regression tests, execute tests, and summarize changes. Rules covered half-up rounding, pre-coupon discount eligibility, per-order shipping, coupon caps, and rejecting negative coupons.

| Stage | Direct / mixed tokens | Direct / mixed estimated USD | Direct / mixed wall time | Correctness and interpretation |
|---|---|---|---|---|
| Initial GLM coding trial | 48,191 / 96,410* | $0.264895 / $0.148023* | 45.82 / 330.01 s | Direct passed; worker timed out at 300 s and left negative-sub-cent defect |
| Cache telemetry enabled | 41,665 / 114,563 | $0.245689 / $0.135294 | 37.607 / 85.603 s | GLM completed, but still missed negative-sub-cent check |
| Verification instructions corrected | 36,110 / 209,894 | $0.260382 / $0.170621 | 52.922 / 284.303 s | Both passed predeclared checks; parent raised additional SKU concern |
| Worker budgets propagated | 31,232 / 157,194 | $0.269695 / $0.136778 | 50.870 / 147.265 s | Both passed public, hidden, and negative-sub-cent checks |
| Luna replaces GLM | 31,086 / 50,481 | $0.259538 / $0.115425 | 47.304 / 42.355 s | Both passed predeclared checks; review found Luna helper-level defect |

\* Incomplete worker cache accounting. Other estimates cover all exposed nonzero usage classes at recorded catalog rates.

These stages changed instrumentation, instructions, or effective worker budgets. They are useful diagnostic observations, **not repeated samples of a single unchanged treatment**.

#### Correctness lessons

1. **Validate before lossy normalization.** A coupon of `-0.001` rounded to signed zero before the `< 0` check, so the negative amount was accepted. Tests for `-1.00` did not cover this boundary.
2. **Test helpers as well as entry points where their contracts require it.** Luna repaired invoice-level validation but left `coupon_discount(Decimal("1.00"), "-0.001")` returning signed zero. Direct Astra rejected it. The invoice-level external probe alone missed this inconsistency.
3. **Do not introduce asymmetric post-hoc grading.** In the corrected-instructions trial, Astra review flagged `sku=None`. Independent inspection found both arms accepted it, and the README did not explicitly define allowed SKU types. It was not fair to fail only the mixed arm on that basis.
4. **An agent can finish while its task remains incomplete.** Harness `success` means the protocol completed, not that business rules were satisfied.
5. **Do not overwrite historical outcomes with manual repairs.** Coupon fixes and red/green regression tests were performed in separate post-fix copies; their success does not retroactively turn a benchmark failure into a pass.

## Latency RCA and implemented fixes

### 1. Cache usage was collected internally but dropped from worker results

The existing meter accumulated full usage, but worker outcome serialization retained only input/output.

Implemented:

- Preserve `cacheReadTokens`, `cacheCreateTokens`, and optional `reasoningTokens` in worker outcomes.
- Use the existing usage serialization/accumulation path.
- Include cache counters in existing failure/timeout error text.
- Cover foreground, typed host, and background completion results.

Parent-only summary semantics remain unchanged. Nested descendants are not automatically folded into an ancestor's usage; these benchmarks used depth one.

### 2. Verification instructions contradicted orchestrate-mode policy

The old benchmark simultaneously required exactly one worker, prohibited another call, and instructed the parent to personally run shell tests. Orchestrate mode intentionally blocks parent command execution.

The trace showed Astra **recognized** the coupon defect and reported it unresolved. It did not falsely claim correctness. A previous description that shell execution was "denied" was imprecise: no shell call was emitted in that run; the parent recognized the restriction and refrained.

Implemented:

- Clarified orchestration briefing: delegate command-based verification and discovered-defect repair; do not claim personal execution of blocked commands.
- Corrected the benchmark contract: sole worker owns implementation/testing, parent reviews read-only.
- Preserved tool-policy boundaries.

The briefing is guidance, not an automatic correctness/recovery guarantee. Under the exactly-one-worker constraint, a defect found after completion remains unresolved unless a future benchmark explicitly permits a follow-up call.

### 3. Routed worker effort/output budgets were reset

`provider_endpoint` in `crates/cli/src/subagent_models.rs` reset `reasoning_effort` and `max_output_tokens` to `None`. Consequently, parent low effort and the 4,096 per-response cap did not reach the GLM worker.

The captured OpenRouter catalog listed GLM's default reasoning effort as `max`. Request overrides were absent; provider defaults applied. We cannot reconstruct the exact historical reasoning-token split because the parser then discarded that detail.

Implemented:

- Same-provider routed workers inherit explicit parent effort/output settings.
- Codex still omits its unsupported output cap.
- Cross-provider propagation remains disabled as a conservative compatibility fallback.
- A mock HTTP test verifies the actual OpenRouter worker request includes low effort and `max_tokens: 4096`.

This is provider-level propagation, not a universal model-capability resolver. Same-provider models can still have different supported parameters; verify compatibility when selecting a new worker.

### 4. Added timing and reasoning telemetry

Implemented:

- Parse `completion_tokens_details.reasoning_tokens`, preserving absent versus zero.
- Preserve output totals; do not double-count reasoning.
- Record bounded successful model-call durations and tool-name/duration samples in worker `timing`.
- Preserve cumulative durations after sample limits.
- Do not log tool arguments, outputs, or prompts in timing samples.

Limitations:

- Model-call intervals include network, queueing, routing, and generation; they are not pure inference time.
- Failed model calls do not reach the successful after-model hook and lack completed timing samples.
- Unknown/policy-denied tools lack execution-hook timing.
- Parallel tool durations overlap; cumulative tool milliseconds are not additive wall-clock attribution.

### Measured latency after the fixes

| Metric | GLM before budget fix | GLM after budget fix | Luna subsequent trial |
|---|---:|---:|---:|
| Worker runtime | 269.180 s | 133.065 s | 27.870 s |
| Worker model steps | 19 | 19 | 9 |
| Worker tool calls | 27 | 18 | 14 |
| Worker output tokens | 29,619 | 6,248 | 2,743 |
| Model-call cumulative time | Unavailable | 132.819 s | 27.769 s |
| Tool cumulative time | Unavailable | 0.226 s | 0.094 s |

The budget-fixed GLM run had 48.2% lower full mixed-session wall time than the preceding run. It still spent nearly all worker time in model-call intervals, not tests or filesystem operations. Luna used fewer turns and completed much faster in its sampled run. Neither observation is a statistically isolated causal estimate.

Local validation at integration: 6 routing tests, 53 subagent tests, and 28 provider tests passed; one provider test was ignored. Release build succeeded. Earlier mode-policy tests also passed (19 tests).

## Two additional tasks: Astra versus Astra + Luna

Both tasks used fresh original fixtures, seven public tests, ten hidden tests, CLI checks, and reference solutions verified before paid execution. Order was event-log direct then mixed, followed by dependency-planner mixed then direct. Each mixed run completed exactly one verified Luna worker.

### Results

| Task | Arm | Tokens including cache | Estimated USD | Wall time | Frozen grading |
|---|---|---:|---:|---:|---|
| Event-log aggregation | Direct Astra | 35,364 | $0.298362 | 53.12 s | Pass: 7 public, 10 hidden, CLI |
| Event-log aggregation | Astra + Luna | 85,031 | $0.126928 | 58.64 s | Pass: 7 public, 10 hidden, CLI |
| Dependency planner | Direct Astra | 77,419 | $0.537075 | 95.96 s | 7 public, 9/10 hidden, CLI pass; evaluator issue below |
| Dependency planner | Astra + Luna | 51,261 | $0.119042 | 42.40 s | Same evaluator issue |

Tasks:

- **Event logs:** duplicate-ID handling, UTC-day grouping, counts, validation, stable JSON output, and CLI behavior.
- **Dependency planner:** prerequisite closure, lexicographically minimal topological ordering, global cycle/missing-node validation, and CLI behavior.

The event-log mixed run used **2.40× tokens**, cost **57.5% less**, and took **10.4% longer**.

The dependency-planner mixed run used **33.8% fewer tokens**, cost **77.8% less**, and took **55.8% less time**, but its frozen quality result needs the evaluator qualification below.

### Evaluator defect: unspecified error wording

Both dependency implementations correctly raised `ValueError` for missing prerequisites. The hidden test additionally required the literal phrase `missing node`, which the README did not require. Responses such as `missing prerequisite: b` and `unknown prerequisite for a: b` failed the regex despite satisfying the stated exception contract.

The original frozen FAIL records are preserved. This is **not evidence of an established implementation defect in either arm**. A future evaluator revision should remove the unspecified wording requirement (or explicitly specify it beforehand), apply the revision symmetrically, and keep its results separately versioned. No post-hoc passing score is substituted here.

Parent acceptance is also separate: mixed parents expressed coverage concerns, while event-log external grading passed. A reviewer's conservative disposition is not interchangeable with a fixed external test score.

### Aggregate measurements

| Metric | Direct Astra | Astra + Luna |
|---|---:|---:|
| Tokens including cache | 112,783 | 136,292 |
| Estimated cost | $0.835437 | $0.245969 |
| Summed session wall time | 149.08 s | 101.04 s |

Mixed execution consumed **20.8% more tokens**, with **70.6% lower estimated cost** and **32.2% lower summed wall time**. These are descriptive resource totals across two small tasks, not a claim of established equal-quality production savings.

## Interpretation and next steps

### Supported conclusions

- Cost and token count must remain separate metrics.
- Reducing frontier-model work can save money even when total tokens rise.
- Worker selection and effective effort budgets materially affect observed latency.
- In measured post-fix workers, model-call intervals dominate tool execution time.
- Cache-aware accounting and independent grading are necessary for credible comparisons.

### Not established

- A universal percentage saving or a reliable model ranking.
- Actual provider invoice amounts or subscription economics.
- Pure inference throughput from model-call wall time.
- Benefits from parallelism: these runs intentionally used one foreground worker.
- Automatic repair after parent review under a strict one-call limit.
- General correctness beyond the written contract and checks exercised.

### Recommended next benchmark improvements

1. Repair and version the dependency evaluator's unspecified message assertion without modifying historical records.
2. Freeze explicit helper/entry-point contracts and cover both where appropriate.
3. Run multiple alternating repetitions with the same prompts, fixtures, budgets, and provider routes; report distributions rather than single-point speed claims.
4. Report correctness, parent review disposition, complete cost coverage, token classes, and time together.
5. If testing review-and-repair, declare a bounded follow-up budget as a separate strategy; do not silently relax the one-worker experiment.
6. Preserve durable sanitized artifacts outside temporary directories before publication or cleanup.

## Artifact index

Raw outputs may contain task content and local paths. Temporary credential copies were removed; do not commit live user configuration or credentials. The paths below are local temporary artifacts and are **not durable repository fixtures**.

| Experiment | Artifact root |
|---|---|
| Initial no-delegation Haiku trial | `/tmp/orca-normal-v-orchestrate-5mde6f7c` |
| Two Haiku workers | `/var/folders/n7/shsj_t096d50t8dx465nkh3h0000gn/T/orca-normal-v-orchestrate-revised-z8fw6bsl` |
| One Haiku worker | `/var/folders/n7/shsj_t096d50t8dx465nkh3h0000gn/T/orca-normal-v-orchestrate-one-worker-HnFnSIjj` |
| Codex Astra baseline | `/tmp/orca-astra-benchmark-ymU5hM` |
| OpenRouter Astra-to-Astra | `/tmp/orca-openrouter-astra-benchmark-OdWUI8` |
| OpenRouter Astra-to-GLM log task | `/tmp/orca-openrouter-astra-glm53-benchmark-vvmkLt` |
| Initial multi-step coding | `/tmp/orca-openrouter-astra-glm53-coding-9mK9En` |
| Coding with cache telemetry | `/tmp/orca-openrouter-astra-glm53-coding-rerun-qDGhNU` |
| Corrected verification instructions | `/tmp/orca-openrouter-astra-glm53-coding-fresh-YL25KJ` |
| Budget/timing fixes | `/var/folders/n7/shsj_t096d50t8dx465nkh3h0000gn/T/orca-budget-fixed-tz8b1rz7` |
| Luna invoice trial | `/var/folders/n7/shsj_t096d50t8dx465nkh3h0000gn/T/orca-astra-luna-fresh-jc9k_dom` |
| Two additional tasks | `/var/folders/n7/shsj_t096d50t8dx465nkh3h0000gn/T/orca-two-task-astra-luna-v67xir` |

Typical artifacts include exact prompts/commands, pricing snapshots, raw JSONL/stderr, monotonic timings, code diffs, original fixtures, external grading, and machine-readable summaries. Their filenames vary between iterations.
