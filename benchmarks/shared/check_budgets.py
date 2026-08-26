#!/usr/bin/env python3
"""Enforce latency budgets against the benchmark results.

Two suites, two shapes:

  startup  hyperfine exports in benchmarks/results/startup/, gated on each
           command's mean wall clock
  kernel   the summary written by kernel/report.py, gated on the percentile
           metrics the README publishes as the kernel's targets

Budgets are enforced on Linux only, because that is what CI runs and what
the published numbers were measured on; elsewhere the run is informational.
Set ORCA_BENCH_ENFORCE=1 to gate anyway.

The ceilings are deliberately several times the numbers a quiet machine
produces. They catch an order-of-magnitude regression — a blocking call
added to the hot path, a directory walk added to startup — not 20% drift,
which shared CI runners cannot measure honestly.
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


def kernel_measurements(results_dir: str) -> list[Measurement]:
    """kernel/report.py's summary. Only the second-valued entries are
    latencies; throughput and speedup are reported by the report itself."""
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
    parser.add_argument("--suite", choices=("startup", "kernel"), default="startup")
    parser.add_argument("--dir", default=None, help="override the results directory")
    args = parser.parse_args()

    results_dir = args.dir or os.path.join(BENCH_DIR, "results", args.suite)
    if args.suite == "startup":
        measurements = startup_measurements(results_dir)
        budgets, default_budget = STARTUP_BUDGETS, DEFAULT_STARTUP_BUDGET
    else:
        measurements = kernel_measurements(results_dir)
        # A kernel metric with no published target is reported, not gated:
        # p50s and the n=1 fan-out alias exist for context.
        budgets, default_budget = KERNEL_BUDGETS, None

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
