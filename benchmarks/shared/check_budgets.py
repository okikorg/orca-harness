#!/usr/bin/env python3
"""Enforce latency budgets against the benchmark results.

Three suites, two shapes:

  startup     hyperfine exports in benchmarks/results/startup/, gated on
              each command's mean wall clock
  kernel      the summary written by kernel/report.py, gated on the
              percentile metrics the README publishes as the kernel's
              targets, plus the criterion estimates it folds in under
              --criterion
  core-tools  the summary written by core-tools/report.py, gated on the
              median wall clock of each real-file mutation workload

Budgets are enforced on Linux only, because that is what CI runs and what
the published numbers were measured on; elsewhere the run is informational.
Set ORCA_BENCH_ENFORCE=1 to gate anyway.

The ceilings are deliberately several times the numbers a quiet machine
produces. They catch an order-of-magnitude regression — a blocking call
added to the hot path, a directory walk added to startup, a fsync added to
every edit — not 20% drift, which shared CI runners cannot measure honestly.
"""

import argparse
import glob
import json
import os
import platform
import sys
from typing import NamedTuple, Optional

BENCH_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# Seconds, roughly 3-4x the means an Apple Silicon Mac produces (2.8ms
# startup, 4.7ms resume). Re-baseline with `./benchmarks/startup/run.sh` and
# the table in benchmarks/README.md if the shape of startup changes.
STARTUP_BUDGETS = {
    "orcacode --help": 0.010,
    "orcacode (startup)": 0.012,
    "orcacode (startup, new session)": 0.012,
    "orcacode (startup, skills)": 0.016,
    "orcacode (resume)": 0.020,
}
DEFAULT_STARTUP_BUDGET = 0.012

# Seconds. p99 over the iteration count kernel/run.sh runs (5000 in --ci), so
# these are percentiles rather than worst samples. Roughly 5-20x what a
# quiet Apple Silicon Mac measures — enough headroom for a shared runner's
# scheduler, not enough to hide a regression. The 1ms fan-out ceiling is
# the kernel's published target, not a widened one.
KERNEL_BUDGETS = {
    "dispatch (T1-T0) n=1 p99": 0.000020,
    "dispatch (T1-T0) n=10 p99": 0.000050,
    "dispatch (T1-T0) n=100 p99": 0.000200,
    "fan-out (T2-T0) n=10 p99": 0.000150,
    "fan-out (T2-T0) n=100 p99": 0.001000,
    "total round-trip n=1 p99": 0.000025,
    "total round-trip n=10 p99": 0.000200,
    "total round-trip n=100 p99": 0.001500,
}

# Seconds. Criterion's mean estimate for each `cargo bench` case, folded
# into the kernel summary by kernel/report.py --criterion. Roughly 10x an
# Apple Silicon Mac: a mean over criterion's sample window is far steadier
# than a single p99, so the headroom is for a slower runner, not for noise.
# The raw_sql memory cases are the sqlite floor the harness is compared
# against, not harness code, so they stay informational.
CRITERION_BUDGETS = {
    "criterion/dispatch/noop_calls/1": 0.000010,
    "criterion/dispatch/noop_calls/10": 0.000250,
    "criterion/dispatch/noop_calls/100": 0.002000,
    "criterion/extensions/ten_calls_with_exts/0": 0.000200,
    "criterion/extensions/ten_calls_with_exts/1": 0.000200,
    "criterion/extensions/ten_calls_with_exts/10": 0.000400,
    "criterion/keyed/100_calls_4_keys": 0.001200,
    "criterion/harness_loop/round_trip_n_calls/1": 0.000025,
    "criterion/harness_loop/round_trip_n_calls/4": 0.000200,
    "criterion/harness_loop/round_trip_n_calls/16": 0.000400,
    "criterion/harness_loop/round_trip_n_calls/64": 0.002000,
    "criterion/harness_loop_fanout/suspending_calls/1": 0.000025,
    "criterion/harness_loop_fanout/suspending_calls/8": 0.000250,
    "criterion/harness_loop_fanout/suspending_calls/64": 0.002000,
    # Bounded below by the 200µs sleep each call performs plus timer
    # granularity; what is gated is that 64 calls still overlap.
    "criterion/harness_loop_fanout/parallel_200us_calls/1": 0.020000,
    "criterion/harness_loop_fanout/parallel_200us_calls/8": 0.020000,
    "criterion/harness_loop_fanout/parallel_200us_calls/64": 0.020000,
    "criterion/memory_fts_50k/orcacode_store/exact": 0.001200,
    "criterion/memory_fts_50k/orcacode_store/broad": 0.350000,
    "criterion/memory_fts_50k/orcacode_context/exact": 0.001200,
    "criterion/memory_fts_50k/orcacode_context/broad": 0.350000,
    "criterion/memory_fts_50k/memory_manage/forget_no_approval": 0.000800,
    "criterion/skills/discover/empty": 0.000150,
    "criterion/skills/discover/1": 0.000500,
    "criterion/skills/discover/10": 0.002500,
    "criterion/skills/discover/25": 0.005000,
    "criterion/skills/discover/100": 0.020000,
    "criterion/skills/schema/1": 0.000020,
    "criterion/skills/schema/25": 0.000150,
    "criterion/skills/schema/100": 0.000500,
    "criterion/skills/load/instructions": 0.000500,
    "criterion/skills/load/resource": 0.000600,
    "criterion/skills/unload/toggle": 0.005000,
    "criterion/skills/unload/delete": 0.050000,
}

# Seconds. Median wall clock across the samples core-tools/run.sh takes,
# per workload, roughly 10x an Apple Silicon Mac. Each workload moves 32
# MiB through the real filesystem, so the ceiling is for a slower disk;
# what it catches is a per-edit fsync, a lost fan-out, or a rewrite of
# every file per hunk. p99 and the serial single-call averages are
# reported for context and not gated: 20 samples make a p99 the maximum.
CORE_TOOLS_BUDGETS = {
    "core-tools/apply_patch/distinct wall p50": 0.075,
    "core-tools/apply_patch/guarded_distinct wall p50": 0.075,
    "core-tools/apply_patch/batch_files wall p50": 0.400,
    "core-tools/apply_patch/repeated_file wall p50": 0.015,
    "core-tools/apply_patch/append_files wall p50": 0.400,
    "core-tools/multi_edit/distinct wall p50": 0.050,
    "core-tools/multi_edit/guarded_distinct wall p50": 0.060,
    "core-tools/multi_edit/batch_files wall p50": 0.150,
    "core-tools/multi_edit/repeated_file wall p50": 0.035,
    "core-tools/multi_edit/append_files wall p50": 0.125,
}

INFORMATIONAL = {"process baseline"}


class Measurement(NamedTuple):
    name: str
    value: float
    """Columns printed after the value: spread for a timed command, empty
    for a percentile that has none."""
    detail: str


def enforcing(system_name: str) -> bool:
    if os.environ.get("ORCA_BENCH_ENFORCE") == "1":
        return True
    return system_name == "Linux"


def duration(seconds: float) -> str:
    """Percentiles here run from sub-microsecond to milliseconds; one fixed
    unit would print half the suite as 0.00."""
    if seconds < 1e-3:
        return f"{seconds * 1e6:.1f}µs"
    return f"{seconds * 1e3:.2f}ms"


def startup_measurements(results_dir: str) -> list[Measurement]:
    out = []
    for path in sorted(glob.glob(os.path.join(results_dir, "*.json"))):
        if os.path.basename(path) == "summary.json":
            continue
        with open(path) as handle:
            result = json.load(handle)["results"][0]
        out.append(
            Measurement(
                result["command"],
                result["mean"],
                f"median={duration(result['median']):>9}  min={duration(result['min']):>9}",
            )
        )
    return out


def summary_measurements(results_dir: str) -> list[Measurement]:
    """A suite's benchmark-action summary.json (kernel/report.py and
    core-tools/report.py write the same shape). Only the second-valued
    entries are latencies; throughput and speedup are reported by the
    report itself."""
    path = os.path.join(results_dir, "summary.json")
    if not os.path.isfile(path):
        return []
    with open(path) as handle:
        entries = json.load(handle)
    return [
        Measurement(entry["name"], entry["value"], entry.get("extra", ""))
        for entry in entries
        if entry.get("unit") == "s"
    ]


def check(
    measurements: list[Measurement],
    budgets: dict[str, float],
    default_budget: Optional[float],
    enforce: bool,
) -> bool:
    passed = True
    for item in measurements:
        head = f"  {{tag}}  {item.name:<48} {duration(item.value):>9}"
        tail = f"  {item.detail}" if item.detail else ""
        if item.name in INFORMATIONAL:
            print(head.format(tag="BASE") + tail)
            continue
        budget = budgets.get(item.name, default_budget)
        if budget is None:
            print(head.format(tag="INFO") + tail)
            continue
        if not enforce:
            print(head.format(tag="INFO") + tail + f"  (budget: {duration(budget)})")
            continue
        ok = item.value <= budget
        print(head.format(tag="PASS" if ok else "FAIL") + tail + f"  (limit: {duration(budget)})")
        passed = passed and ok
    return passed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--suite", choices=("startup", "kernel", "core-tools"), default="startup"
    )
    parser.add_argument("--dir", default=None, help="override the results directory")
    args = parser.parse_args()

    results_dir = args.dir or os.path.join(BENCH_DIR, "results", args.suite)
    if args.suite == "startup":
        measurements = startup_measurements(results_dir)
        budgets, default_budget = STARTUP_BUDGETS, DEFAULT_STARTUP_BUDGET
    elif args.suite == "kernel":
        measurements = summary_measurements(results_dir)
        # A kernel metric with no published target is reported, not gated:
        # p50s, the n=1 fan-out alias and the real-tool probe exist for
        # context. Criterion cases share the summary and the lookup.
        budgets, default_budget = {**KERNEL_BUDGETS, **CRITERION_BUDGETS}, None
    else:
        measurements = summary_measurements(results_dir)
        budgets, default_budget = CORE_TOOLS_BUDGETS, None

    if not measurements:
        print(f"No {args.suite} results found in {results_dir}")
        return 1

    system_name = platform.system()
    enforce = enforcing(system_name)
    if check(measurements, budgets, default_budget, enforce):
        if not enforce:
            print(f"\nBudgets not enforced on {system_name}; numbers are informational")
        else:
            print("\nAll measurements within budget")
        return 0
    print("\nLatency budget exceeded")
    return 1


if __name__ == "__main__":
    sys.exit(main())
