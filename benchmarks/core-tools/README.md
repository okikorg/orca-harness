# Core-tools mutation benchmark

This suite measures the native `apply_patch` and `multi_edit` tools in
isolation through the real `Dispatcher` and real filesystem operations. Each
sample covers five workloads for both tools:

- 32 distinct 1 MiB files, serialized and fully admitted;
- the same distinct-file workload with the production `FileGuard` enabled;
- one call mutating 32 files;
- one 1 MiB file receiving 32 ordered edits or hunks;
- one call appending distinct content to 32 existing 1 MiB files.

Every resulting file is checked for the expected marker before a sample passes.

It records average per-call latency from the serial run, concurrent batch
p50/p99 wall time and throughput, speedup over serial dispatch, the exact raw
test output, and machine/build provenance.

Before performance sampling, the runner executes `core_tools.rs`, which
covers preflight failure safety, transactional multi-file append behavior,
multi-file add/update/delete accuracy, CRLF, ambiguous and oversized hunks,
workspace escape rejection, multi-key classification, and same-path call
ordering. This separates correctness from the timing workload without
substituting no-op tools.

Run:

```bash
./benchmarks/core-tools/run.sh
./benchmarks/core-tools/run.sh --quick
```

Results are written to `benchmarks/results/core-tools/`:

- `accuracy.txt` — focused correctness test output;
- `raw.txt` — every release performance sample;
- `measurements.json` — aggregate metadata and p50/p99 statistics;
- `summary.json` — benchmark-action compatible metric entries.
- `baseline.json` — the pre-optimization result preserved on the first run;
- `comparison.json` — like-for-like distinct-file p50 and throughput changes
  from that baseline. It excludes p99 because the preserved baseline predates
  the nearest-rank percentile calculation and its raw samples are unavailable.

These numbers are informational because filesystem behavior is machine-specific.
They do not include model generation or Auto-mode permission-review latency. A
single multi-file call still avoids repeated model/tool admission round trips,
but that saving belongs to an end-to-end model benchmark rather than this
isolated core-tool measurement.
