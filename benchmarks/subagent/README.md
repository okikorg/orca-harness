# Fake-subagent concurrency benchmark

This informational suite measures how many fake subagents the harness can keep
in flight through the real `SubagentTool::call` and inner `Agent` path. The fake
model returns one final answer after a configurable Tokio timer delay, so the
suite isolates harness allocation, scheduling, model-boundary, and completion
behavior without provider latency, API cost, rate limits, or network failures.

The Rust probe lives beside the code it exercises:

```text
crates/tools/examples/bench_subagent_concurrency.rs
```

This directory owns orchestration, analysis, tests, and methodology. Generated
artifacts go to the repository-standard ignored location:

```text
benchmarks/results/subagent/
```

## Run

```bash
./benchmarks/subagent/run.sh --quick      # safe smoke test
./benchmarks/subagent/run.sh --standard   # sweep through 65,536 submissions
./benchmarks/subagent/run.sh --boundary   # 65,536 and 131,072; may exceed 5 GiB
```

Run reporting tests independently:

```bash
python3 -m unittest discover -s benchmarks/subagent -p '*_test.py' -v
```

The runner builds the release probe, removes stale suite outputs, records
machine/toolchain/git metadata, captures process resource statistics, and writes
CSV plus `analysis.json` and benchmark-action-compatible `summary.json`. Each
invocation replaces the prior generated result set, so copy results elsewhere
before switching profiles if you need to retain both.

## Measurements

Each CSV row is one batch:

```text
fanout,run,wall_us,throughput_per_s,p50_us,p95_us,p99_us,failures,peak_active
```

- `fanout`: submitted fake-subagent calls.
- `wall_us`: batch admission through the final joined task.
- `throughput_per_s`: submitted calls divided by batch wall time.
- `p50/p95/p99_us`: empirical order statistics using
  `round((n - 1) * p)`. Latency starts when each spawned task receives its
  first poll, so it excludes pre-poll admission delay.
- `failures`: `SubagentTool::call` errors plus Tokio join failures. Process
  termination such as OOM cannot become a CSV failure row.
- `peak_active`: maximum fake-model futures simultaneously inside the delayed
  model body. This is asynchronous in-flight concurrency, not simultaneous CPU
  execution.

A successful call must return `Ok` with an `"answer": "ok"` field; tool errors,
semantic mismatches, and Tokio join failures count as failures.

## Statistical interpretation

`analyze.py` reports raw calls and failures, and computes the upper endpoint of
a two-sided 95% Wilson score interval. The interval assumes independent,
identically distributed Bernoulli calls. Real benchmark calls are grouped in
batches and share scheduler, allocator, thermal, and machine state, so the raw
failure count is the primary result and the interval is conditional context.

At the largest fanout for which at least one batch reached complete overlap, the
analyzer reports all repetitions at that fanout, every batch throughput, and an
exploratory seeded bootstrap interval for the median. With only three boundary
repetitions that bootstrap is discrete and weak; quote the raw observations
with the median, not the interval alone.

## Scope and gating

This suite is intentionally **informational and not a CI budget**:

- High levels are machine- and memory-dependent.
- The boundary profile uses several GiB of RAM.
- A deterministic timer model does not represent provider, network, auth,
  token-generation, tool-I/O, or rate-limit failure behavior.
- “Maximum observed active” means the largest level completed and directly
  observed in this run. It is not a hard capacity limit.

For regression-gated dispatch overhead, use `./benchmarks/kernel/run.sh`; for host
startup, use `./benchmarks/startup/run.sh`.

## Reference run

The initial local reference was measured on one macOS arm64 machine with 16
logical CPUs and 64 GiB RAM. Three completed 500 ms boundary batches directly
observed 131,072 active fake model futures, with zero failures. Across all three
initial sweeps, 0 failures were counted in 1,444,844 completed calls. Those raw
machine-specific files are generated under `benchmarks/results/subagent/` and
remain git-ignored by repository policy.
