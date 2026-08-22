#!/usr/bin/env python3
"""Summarize hyperfine results into benchmark-action format and print a table.

Reads every hyperfine export under benchmarks/results/<suite>/, prints one
row per command, and writes summary.json in the shape
github-action-benchmark's `customSmallerIsBetter` consumes, so a CI job can
chart the trend without re-parsing hyperfine.
"""

import argparse
import glob
import json
import os
import sys

BENCH_DIR = os.path.dirname(os.path.abspath(__file__))


def ms(value: float) -> str:
    return f"{value * 1000:.1f}ms"


def result_files(results_dir: str) -> list[str]:
    return [
        path
        for path in sorted(glob.glob(os.path.join(results_dir, "*.json")))
        if os.path.basename(path) != "summary.json"
    ]


def summarize(results_dir: str) -> int:
    if not os.path.isdir(results_dir):
        print(f"error: no results directory at {results_dir}", file=sys.stderr)
        return 1

    files = result_files(results_dir)
    if not files:
        print(f"error: no hyperfine results in {results_dir}", file=sys.stderr)
        return 1

    entries = []
    rows = []
    for path in files:
        with open(path) as handle:
            result = json.load(handle)["results"][0]
        rows.append(
            (
                result["command"],
                ms(result["mean"]),
                ms(result["stddev"]),
                ms(result["min"]),
                ms(result["max"]),
            )
        )
        entries.append(
            {
                "name": result["command"],
                "unit": "s",
                "value": round(result["mean"], 6),
                "range": f"± {result['stddev']:.6f}",
                "extra": (
                    f"min={result['min']:.6f}s max={result['max']:.6f}s "
                    f"median={result['median']:.6f}s runs={len(result.get('times', []))}"
                ),
            }
        )

    print(f"{'COMMAND':<32} {'MEAN':>10} {'STDDEV':>10} {'MIN':>10} {'MAX':>10}")
    print(f"{'-' * 32} {'-' * 10:>10} {'-' * 10:>10} {'-' * 10:>10} {'-' * 10:>10}")
    for name, mean, stddev, minimum, maximum in rows:
        print(f"{name:<32} {mean:>10} {stddev:>10} {minimum:>10} {maximum:>10}")
    print()

    out_path = os.path.join(results_dir, "summary.json")
    with open(out_path, "w") as handle:
        json.dump(entries, handle, indent=2)
    print(f"Results written to {out_path}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--suite", default="startup")
    parser.add_argument("--dir", default=None, help="override the results directory")
    args = parser.parse_args()
    results_dir = args.dir or os.path.join(BENCH_DIR, "results", args.suite)
    return summarize(results_dir)


if __name__ == "__main__":
    sys.exit(main())
