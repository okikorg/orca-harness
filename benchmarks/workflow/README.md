# Workflow graph benchmark

This suite submits real dependency graphs through the real `WorkflowTool` path
and measures three things: how large a graph the harness will carry, what the
engine costs between one stage finishing and the next one starting, and whether
the run kept its promises. The model answers after a fixed timer delay, so no
provider latency, cost, rate limit or network failure is in the numbers.

The Rust probe lives beside the code it exercises:

```text
crates/tools/examples/bench_workflow_graph.rs
```

This directory owns orchestration, analysis, tests, and methodology. Generated
artifacts go to the repository-standard ignored location:

```text
benchmarks/results/workflow/
```

## Run

```bash
./benchmarks/workflow/run.sh --quick      # five shapes at 8 and 32 stages
./benchmarks/workflow/run.sh --standard   # shapes, a scale sweep, and one limited case
./benchmarks/workflow/run.sh --boundary   # 4,096 concurrent stages and 512-deep chains
```

Run reporting tests independently:

```bash
python3 -m unittest discover -s benchmarks/workflow -p '*_test.py' -v
```

## Shapes

Each shape isolates a different part of the engine. Every stage costs one model
delay, so a shape's cost is decided by its structure, not its prompts.

| Shape | Graph | What it isolates |
| :---- | :---- | :--------------- |
| `chain` | `s0 → s1 → … → sn` | per-stage dispatch with no concurrency at all |
| `diamond` | one root, `n-2` parallel, one join | the shape a workflow is usually drawn as |
| `wide-join` | `n-1` roots → one join | admission cost, and one settle with every dependency already satisfied |
| `fanout` | source → `map` → join | expansion of a map into children, and the joined barrier |
| `mesh` | `√n` waves of `√n`, two edges back | a schedule that is genuinely constrained rather than wide or deep |

## Measurements

Each CSV row is one submitted run:

```text
shape,stages,run,wall_us,critical_path_us,nominal_path_us,service_p50_us,service_p95_us,
delay_us,limit,dispatch_p50_us,dispatch_p95_us,dispatch_p99_us,peak_observed,peak_admitted,
peak_running,ordering_violations,duplicate_stages,missing_stages,template_mismatches,failures
```

- `wall_us`: submission through the run's terminal notification.
- `critical_path_us`: the longest chain of **measured** stage service times.
  Measured rather than assumed, because a timer sleeps for at least its delay
  and never exactly it; charging that rounding to the engine would overstate it.
- `nominal_path_us`: graph depth times the configured delay — what the critical
  path would be if the machine were idle.
- `service_p50/p95_us`: how long one model call took from entry to return.
  Divided by `delay_us` this is the **service inflation**: at 1 the runtime is
  idle, and well above 1 it is saturated and the wall time has stopped being a
  measurement of the engine.
- `dispatch_p50/p95/p99_us`: **the latency number.** Per stage, the time from
  its last dependency's model returning to its own model being entered — the
  engine advancing the graph, admitting, building an agent and reaching the
  model. Root stages measure from submission. With a concurrency `limit` this
  necessarily includes queue wait, which is a scheduling decision rather than
  overhead.
- `peak_observed` / `peak_admitted` / `peak_running`: concurrent model calls the
  model itself saw; stages the run had in flight (`peakAdmitted`); stages
  holding a concurrency slot (`peakRunning`). The last two diverge exactly when
  a limit binds.
- The five defect columns, below.

## Accuracy

Accuracy here is not a score, because with a deterministic model nothing is
uncertain. Each stage must return `<id>#ok`, so every promise the engine makes
is exactly decidable, and each of these counts a violation of one:

- `ordering_violations`: a stage entered the model before a dependency returned.
- `duplicate_stages`: a stage executed more than once, or a stage executed that
  the graph never declared.
- `missing_stages`: a declared stage never executed.
- `template_mismatches`: a prompt reached the model without an upstream answer
  its `{{ stages.X.output }}` named, or a map's joined barrier was not its
  children's answers **in input order**.
- `failures`: the run did not end `done`, or a terminal output was not the
  answer the stage owed.

A nonzero total is a defect, not noise: `analyze.py` exits non-zero on one. This
is the part of the suite worth gating. The latency and scale numbers are
machine-dependent and are informational, exactly as `benchmarks/subagent/` is.

## Reference run

One macOS arm64 machine, 16 logical CPUs, 64 GiB RAM, 2 ms model delay,
unbounded concurrency. **123 runs, 60,330 stages executed, zero defects.**

| Case | wall | critical path | dispatch p50 | peak running |
| :--- | ---: | ---: | ---: | ---: |
| `wide-join` 128 | 14.7 ms | 11.6 ms | 1.19 ms | 127 |
| `wide-join` 1024 | 318 ms | 296 ms | 15.5 ms | 1007 |
| `wide-join` 4096 | 6.35 s | 6.17 s | 195 ms | 4079 |
| `fanout` 1023 | 195 ms | 177 ms | 18.2 ms | 1006 |
| `fanout` 4095 | 1.85 s | 1.80 s | 65.0 ms | 4078 |
| `mesh` 1024 | 2.16 s | 844 ms | 25.5 ms | 32 |
| `chain` 128 | 509 ms | 432 ms | 0.60 ms | 1 |
| `chain` 512 | 2.80 s | 1.74 s | 2.13 ms | 1 |
| `wide-join` 256, limit 8 | 122 ms | 7.5 ms | 56.9 ms | 8 |

Read the limited row as the queue working: 256 stages through 8 slots is 32
waves, so nearly all of that dispatch time is a stage waiting for a slot.

## An observation worth acting on

`chain` dispatch scales with the length of the chain rather than staying flat:

| chain length | 8 | 32 | 128 | 256 | 512 |
| :--- | ---: | ---: | ---: | ---: | ---: |
| dispatch p50 | 71 µs | 207 µs | 598 µs | 1.29 ms | 2.13 ms |

Per-stage cost growing linearly makes the run quadratic, and at 512 deep the
engine costs about as much per stage as the 2 ms of work it is scheduling. Two
places in `crates/tools/src/workflow/runtime.rs` are O(n) per stage and are the
first things to look at:

- `Runtime::key` calls `Dag::stage_key`, which serializes the whole transitive
  ancestor closure of a stage — for the last stage of a chain, every stage
  before it — and it runs once per spawn.
- `Runtime::persist` scans every stage in the graph on every completion.

Neither shows up in the wide shapes, where closures are shallow. This suite is
not gated on latency, so this is a finding rather than a failure.

## Scope

Informational for latency and scale; the defect counts are deterministic and
gate. A fixed-delay model does not represent provider, network, token
generation, tool I/O or rate-limit behavior, and "4,096 concurrent stages" means
the largest level this machine completed and directly observed, not a limit.

For dispatch overhead between a model and its tools, use
`./benchmarks/kernel/run.sh`; for detached subagent concurrency without a graph,
use `./benchmarks/subagent/run.sh`.
