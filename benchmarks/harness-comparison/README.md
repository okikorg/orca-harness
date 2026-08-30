# Orcacode versus Pi versus Oh My Pi versus Claude Code live benchmark

This suite compares the complete Orcacode, Pi, Oh My Pi (`omp`), and Claude Code
headless harnesses on the same isolated workload. It measures correctness,
latency, tool behavior, token use, cache behavior, and normalized token cost. It
does not run a server and never points any harness at the repository checkout.

The benchmark is scripted before execution:

- [`run.py`](run.py) copies the fixture for every attempt, alternates harness
  order, captures timestamped raw streams, enforces timeouts, and validates
  changed files.
- [`analyze.py`](analyze.py) derives correctness, TTFT, wall time, tool timing,
  token economics, per-task medians, and paired Orcacode deltas.
- [`render_html.py`](render_html.py) exports a dependency-free static report from
  the analyzed JSON artifacts.
- [`tasks.json`](tasks.json) is the reviewable workload and scoring contract.
- [`fixture/`](fixture/) is the synthetic project used by every attempt.

The previous Sonnet report remains available as
[`../results/harness-comparison-2026-08-30.html`](../results/harness-comparison-2026-08-30.html).
It is historical evidence and is not mixed with new four-harness runs.

## Default comparison profile

The runner defaults to:

| Setting | Value |
| :-- | :-- |
| Harnesses | Orcacode, Pi, Oh My Pi, and Claude Code |
| Gateway | OpenRouter; underlying provider endpoint is not pinned |
| Model | `anthropic/claude-haiku-4.5` |
| Reasoning effort | `low` |
| Tasks | 16 |
| Repetitions | 3 |
| Total attempts | 192 |
| Order | Alternates by task and repetition |
| Attempt timeout | 45 seconds |
| Model-step ceiling | 24 for Orcacode and Claude Code; external timeout bounds Pi and Oh My Pi |
| Output-token ceiling | 1,536 for Orcacode and Claude Code; provider defaults for Pi and Oh My Pi |
| Prompt caching | On; opt out with `--no-prompt-cache` where supported |
| User extensions and context | Disabled |

Haiku 4.5 is the low-cost default because its current OpenRouter catalog entry
advertises tool calling and prices input below the Sonnet model used in the
historical run. The model remains overrideable so a recorded run never depends
on an implicit catalog choice.

Pi and Oh My Pi do not expose the same print-mode step and output-token flags as
Orcacode and Claude Code. The common parent-process timeout remains enforced for
every attempt, and each raw record retains the exact command. Do not interpret
the manifest's requested ceilings as provider-side controls where the command
does not contain them.

The default normalized price schedule is the OpenRouter catalog snapshot checked
on 2026-08-30:

| Token class | USD per million tokens |
| :-- | --: |
| Uncached input | $1.00 |
| Output | $5.00 |
| Cache read | $0.10 |
| Five-minute cache write | $1.25 |

Source: [OpenRouter Claude Haiku 4.5 model page](https://openrouter.ai/anthropic/claude-haiku-4.5).
Provider routing and prices can change, so verify and override the four price
flags before a publishable run. Calculated cost is a normalized comparison, not
an OpenRouter invoice.

## Review before running

Print the complete run order without resolving binaries, reading credentials,
creating output directories, or invoking a harness:

```bash
python3 benchmarks/harness-comparison/run.py --dry-run
```

Useful smaller reviews:

```bash
python3 benchmarks/harness-comparison/run.py --dry-run --repetitions 1 --profile read
python3 benchmarks/harness-comparison/run.py --dry-run --task incident-correlation
python3 benchmarks/harness-comparison/run.py --dry-run --profile edit
python3 benchmarks/harness-comparison/run.py --dry-run --harness both
python3 benchmarks/harness-comparison/run.py --dry-run --harness omp
python3 benchmarks/harness-comparison/run.py --dry-run --harness claude
```

## Run and analyze

Build Orcacode separately, make `pi`, `omp`, and `claude` available, export the
OpenRouter key through the environment, then start the benchmark from the
repository root:

```bash
export OPENROUTER_API_KEY='...'
python3 benchmarks/harness-comparison/run.py
```

The key is never placed in a prompt, command argument, manifest, or raw record.
The runner resolves `target/release/orcacode`, `pi`, `omp`, and `claude`, records
their versions, and creates a timestamped directory under
`benchmarks/results/harness-comparison/`.

Claude Code is deliberately routed through the same OpenRouter endpoint and key
as the other harnesses. Its normal OAuth/keychain state is neither read nor used.
OpenRouter can still route individual requests to different underlying provider
endpoints. The installed harness CLIs do not expose one common per-request routing
control or route identifier, so the manifest records that boundary explicitly.
Latency is therefore a complete gateway-routed harness result, not an isolated
comparison of harness overhead on one verified inference backend.

Analyze that directory after all attempts finish:

```bash
python3 benchmarks/harness-comparison/analyze.py \
  benchmarks/results/harness-comparison/<run-id>
```

The runner does not automatically analyze partial data. This keeps raw capture
and interpretation separate and allows the parser to be rerun after review.

## Workload

Each attempt receives a fresh temporary copy of the fixture. The default suite
covers more than simple lookup:

| Task | Category | Mode |
| :-- | :-- | :-- |
| `component-weights` | Targeted retrieval | Read |
| `timeout-delta` | Cross-file comparison | Read |
| `retry-budgets` | Repository search | Read |
| `invoice-endpoint` | Cross-format join | Read |
| `traffic-aggregate` | Structured aggregation | Read |
| `role-permissions` | Structured-data reasoning | Read |
| `cache-contract` | Source/document consistency | Read |
| `latest-breaking-change` | Temporal reasoning | Read |
| `dependency-order` | Graph reasoning | Read |
| `invoice-slo` | Threshold evaluation | Read |
| `incident-correlation` | Multi-log diagnosis | Read |
| `api-contract-diff` | Schema diff | Read |
| `dependency-budget` | Cost aggregation | Read |
| `feature-precedence` | Configuration precedence | Read |
| `fix-retry-boundary` | Single-file code edit | Edit |
| `fix-worker-heartbeat` | Cross-file diagnosis and config edit | Edit |

Read tasks expose only read, search, glob, and listing tools. Edit tasks add
file edit/write tools but still exclude shell and process execution. Each edit
task declares its only permitted changed path and content checks. A run fails
workspace validation if it changes anything else, misses the required change,
or leaves the known defect in place.

The exact prompts, expected terminal answers, permitted changes, and validators
live in [`tasks.json`](tasks.json). Treat that file as the benchmark contract.

## Isolation and parity

The runner applies these controls to all four harnesses:

- same exact model slug, gateway, reasoning effort, task prompt, fixture, and
  run timeout;
- fresh ephemeral workspace and session for every attempt;
- sequential attempts to avoid local resource contention;
- rotating Orcacode/Pi/Oh My Pi/Claude Code order across tasks and repetitions;
- no shell, process, server, web, MCP, skill, plugin, or subagent tools;
- no retries that could replace a failed primary attempt;
- raw output retained even when an attempt times out or fails validation.

Orcacode uses `--bare` with an explicit tool allowlist. Pi disables extensions,
skills, prompt templates, themes, context files, project approvals, and session
persistence. Oh My Pi uses JSON print mode with isolated state and `HOME`,
explicit tools, and extensions, skills, rules, context discovery, LSP, PTY,
title generation, and session persistence disabled. Its per-attempt config
overlay disables discovery providers without disabling the OpenRouter model
provider. Claude Code uses bare restricted mode, explicit tools, an empty MCP
configuration, disabled skills and browser integration, and fresh `HOME` and
configuration directories. Tool names differ because the harness APIs differ;
the manifest records the exact command surface for every attempt.

Oh My Pi's JSON stream uses the Pi agent event family: `message_update`,
`message_end`, `tool_execution_start`, and `tool_execution_end`. The analyzer
therefore shares the Pi event parser while retaining `omp` as its own harness.
Prompt-cache retention is explicitly `short` by default and `none` under
`--no-prompt-cache`. See the current [Oh My Pi RPC event reference](https://github.com/can1357/oh-my-pi/blob/main/docs/rpc.md)
and [environment-variable reference](https://github.com/can1357/oh-my-pi/blob/main/docs/environment-variables.md).
The runner prepares OMP's compiled native addon once in a temporary runtime
home before timing begins. Per-attempt agent/config/session state remains fresh
under a separate `PI_CODING_AGENT_DIR`, so installation work is not charged to
every OMP attempt and no real user profile is read or modified.

## Latency metrics

Every stdout and stderr line is wrapped with a parent-process monotonic
timestamp. This enables consistent harness-level timings without trusting
provider clocks:

| Metric | Definition |
| :-- | :-- |
| Wall time | Process spawn to process exit or timeout |
| End-to-end TTFT | Process spawn to first model-generated delta |
| Model TTFT | Observed agent/turn start to first model-generated delta |
| Time to answer | Process spawn to first visible final-answer text delta |
| Time to first tool | Process spawn to first tool-execution start event |
| Time to first tool result | Process spawn to first completed tool event |
| Tool duration sum | Sum of matched tool start/end durations |
| Tool duration max | Slowest matched tool call |

The first model delta may be hidden reasoning, visible text, or streamed tool
arguments. End-to-end TTFT therefore measures perceived harness startup plus
provider response latency. Model TTFT removes the observable pre-agent startup
portion, but it is still not a provider-side network trace.

Tool-duration sums can exceed wall time when independent tools run in parallel.
Use the maximum tool duration and raw timeline when diagnosing concurrency.
Claude Code's stream exposes model-side `tool_use` and a later `tool_result`, but
not execution start/end events equivalent to Orcacode, Pi, and Oh My Pi. Its tool
count is reported; first-tool and tool-duration metrics are left unavailable.

## Token economics

Provider-reported usage is mapped consistently:

| Metric | Orcacode | Pi | Oh My Pi | Claude Code |
| :-- | :-- | :-- | :-- | :-- |
| Uncached input | Final summary `inputTokens` | Sum assistant usage `input` | Sum assistant usage `input` | Final result `input_tokens` |
| Output | Final summary `outputTokens` | Sum assistant usage `output` | Sum assistant usage `output` | Final result `output_tokens` |
| Cache read | Final summary `cacheReadTokens` | Sum assistant usage `cacheRead` | Sum assistant usage `cacheRead` | Final result `cache_read_input_tokens` |
| Cache write | Final summary `cacheCreateTokens` | Sum assistant usage `cacheWrite` | Sum assistant usage `cacheWrite` | Final result `cache_creation_input_tokens` |
| Tools | Final summary `toolCalls` | Count `tool_execution_start` | Count `tool_execution_start` | Count unique streamed `tool_use` ids |
| Turns | Final summary `modelSteps` | Count assistant messages with usage | Count assistant messages with usage | Final result `num_turns` |

Derived metrics include:

- total prompt tokens = input + cache reads + cache writes;
- total tokens = total prompt + output;
- cache-read share = cache reads / total prompt;
- output tokens per model turn;
- normalized cost by token class;
- normalized cost per successful attempt;
- total spend on failed attempts;
- total tokens per successful attempt;
- per-task median cost and paired cost delta.

Do not compare uncached input alone. A harness can look artificially efficient
there while creating or reading a much larger cached prompt. Compare correctness,
wall time, total prompt, token-class cost, and failure spend together.

## Outputs

The runner writes:

- `manifest.json`: configuration, versions, pricing, tasks, and balanced order;
- `rep-<n>-<task>-<harness>.jsonl`: timestamped raw stdout/stderr plus process
  and workspace-validation records.

The analyzer writes:

- `runs.json`: every parsed attempt and derived metric;
- `runs.csv`: flat data for external analysis;
- `summary.json`: harness totals and per-task paired medians;
- `summary.md`: compact review report.

Export a self-contained HTML report without starting a server:

```bash
python3 benchmarks/harness-comparison/render_html.py \
  benchmarks/results/harness-comparison/<run-id> \
  --output benchmarks/results/harness-comparison-report.html
```

Generated JSON, CSV, and text artifacts under `benchmarks/results/` are ignored.
Export a reviewed HTML report separately when a result is ready to publish.

## Interpretation boundaries

- Three repetitions are a minimum, not proof of long-run superiority. Increase
  repetitions for release gating and report median plus spread.
- Workload P95 is a percentile across task samples, not a confidence interval.
- This measures complete installed binaries, provider routing, model behavior,
  tool schemas, and network conditions—not only the underlying agent loop.
- Exact-answer scoring is deterministic but narrower than human review of
  open-ended coding quality.
- Edit validation proves the declared fixture change, not compilation or test
  success, because shell and code execution are intentionally excluded.
- Orcacode prompt caching is on by default. Haiku 4.5 requires a 4,096-token
  cacheable prefix, so short attempts can still report zero reads and writes.
  Use a controlled long-prefix profile before drawing cache-economics conclusions.

## Parameters worth expanding later

The current scripts deliberately capture metrics available from all raw event
streams. Useful next extensions, kept separate from the base score, are:

- peak resident memory, CPU time, and process startup cost;
- provider route/provider identity when OpenRouter exposes it consistently;
- explicit request-start timestamps in all harness event schemas for cleaner
  network-only TTFT;
- output token throughput for uninterrupted text-generation phases;
- warm-cache and cold-cache profiles with controlled cache identity;
- deterministic shell/test tasks in stronger disposable sandboxes;
- larger real-world repositories with licensed, publishable fixtures;
- human- or judge-scored patch quality alongside deterministic validators.

Add a metric only when all harnesses expose an equivalent boundary. Otherwise
record it as harness-specific diagnostic data rather than ranking evidence.
