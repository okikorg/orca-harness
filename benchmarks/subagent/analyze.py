#!/usr/bin/env python3
"""Analyze fake-subagent concurrency CSVs and write benchmark summary JSON."""

import argparse
import csv
import json
import math
import os
import random
import statistics

FIELDS = {
    "fanout": int,
    "run": int,
    "wall_us": int,
    "throughput_per_s": float,
    "p50_us": int,
    "p95_us": int,
    "p99_us": int,
    "failures": int,
    "peak_active": int,
}


def read_csv(path: str) -> list[dict]:
    with open(path, newline="") as handle:
        reader = csv.DictReader(handle)
        if reader.fieldnames != list(FIELDS):
            raise ValueError(f"unexpected CSV header in {path}: {reader.fieldnames}")
        rows = []
        for line, row in enumerate(reader, start=2):
            if None in row or any(row[name] is None for name in FIELDS):
                raise ValueError(f"unexpected column count in {path}:{line}")
            parsed = {name: convert(row[name]) for name, convert in FIELDS.items()}
            if not math.isfinite(parsed["throughput_per_s"]) or parsed["throughput_per_s"] <= 0:
                raise ValueError(f"non-positive or non-finite throughput in {path}:{line}")
            if parsed["fanout"] <= 0 or parsed["run"] < 0:
                raise ValueError(f"invalid fanout/run in {path}:{line}")
            for name in ("wall_us", "p50_us", "p95_us", "p99_us", "failures", "peak_active"):
                if parsed[name] < 0:
                    raise ValueError(f"negative {name} in {path}:{line}")
            if parsed["failures"] > parsed["fanout"] or parsed["peak_active"] > parsed["fanout"]:
                raise ValueError(f"failures/peak exceed fanout in {path}:{line}")
            if not parsed["p50_us"] <= parsed["p95_us"] <= parsed["p99_us"] <= parsed["wall_us"]:
                raise ValueError(f"invalid latency ordering in {path}:{line}")
            rows.append(parsed)
    if not rows:
        raise ValueError(f"no benchmark rows in {path}")
    return rows


def wilson(k: int, n: int, z: float = 1.95996398454) -> tuple[float, float]:
    if n <= 0 or not 0 <= k <= n:
        raise ValueError("Wilson interval requires 0 <= failures <= calls")
    p = k / n
    denominator = 1 + z * z / n
    center = (p + z * z / (2 * n)) / denominator
    half = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / denominator
    return max(0.0, center - half), min(1.0, center + half)


def bootstrap_median(values: list[float], samples: int = 20_000, seed: int = 42) -> tuple[float, float]:
    if not values or samples <= 0:
        raise ValueError("bootstrap requires values and positive samples")
    rng = random.Random(seed)
    simulated = sorted(statistics.median(rng.choices(values, k=len(values))) for _ in range(samples))
    return simulated[int(0.025 * samples)], simulated[min(samples - 1, int(0.975 * samples))]


def entry(name: str, unit: str, value: float, extra: str) -> dict:
    return {"name": name, "unit": unit, "value": value, "range": "± 0", "extra": extra}


def analyze(suites: list[tuple[str, list[dict]]], bootstrap_samples: int = 20_000) -> tuple[dict, list[dict]]:
    all_rows = [row for _, rows in suites for row in rows]
    calls = sum(row["fanout"] for row in all_rows)
    failures = sum(row["failures"] for row in all_rows)
    _, failure_upper = wilson(failures, calls)
    peak = max(row["peak_active"] for row in all_rows)
    fully_observed = [row["fanout"] for row in all_rows if row["peak_active"] == row["fanout"]]
    statistics_fanout = max(fully_observed) if fully_observed else None
    statistics_rows = (
        [row for row in all_rows if row["fanout"] == statistics_fanout]
        if statistics_fanout is not None
        else []
    )
    throughputs = [row["throughput_per_s"] for row in statistics_rows]
    bootstrap = bootstrap_median(throughputs, bootstrap_samples) if throughputs else (0.0, 0.0)
    report = {
        "calls": calls,
        "failures": failures,
        "observedFailureRate": failures / calls,
        "wilsonTwoSided95Upper": failure_upper,
        "maximumObservedActive": peak,
        "maximumObservedRuns": sum(row["peak_active"] == peak for row in all_rows),
        "statisticsFanout": statistics_fanout,
        "statisticsRuns": len(statistics_rows),
        "maximumObservedThroughputsPerSecond": throughputs,
        "maximumObservedMedianThroughputPerSecond": statistics.median(throughputs) if throughputs else None,
        "maximumObservedBootstrapMedian95": list(bootstrap) if throughputs else None,
        "maximumObservedMedianBatchP50Ms": statistics.median(row["p50_us"] for row in statistics_rows) / 1000 if statistics_rows else None,
        "maximumObservedMedianBatchP95Ms": statistics.median(row["p95_us"] for row in statistics_rows) / 1000 if statistics_rows else None,
        "maximumObservedMedianBatchP99Ms": statistics.median(row["p99_us"] for row in statistics_rows) / 1000 if statistics_rows else None,
        "notes": [
            "Maximum observed active is not a hard capacity limit.",
            "Failure interval assumes iid Bernoulli calls; calls and batches share machine state.",
            "Per-call latency starts at each spawned task's first poll, excluding admission wait.",
            "The bootstrap interval is exploratory, especially when based on few boundary runs.",
        ],
    }
    summary = [
        entry(
            "subagent/max observed active",
            "workers",
            peak,
            f"largest fully observed fanout={statistics_fanout if statistics_fanout is not None else 'none'}; higher is informational",
        ),
        entry("subagent/completed calls", "calls", calls, f"failures={failures}"),
        entry("subagent/failure rate", "ratio", failures / calls, f"two-sided 95% Wilson upper={failure_upper:.9g}"),
    ]
    if throughputs:
        summary.append(
            entry(
                f"subagent/throughput fanout={statistics_fanout} median",
                "calls/sec",
                statistics.median(throughputs),
                f"exploratory bootstrap95={bootstrap[0]:.3f}..{bootstrap[1]:.3f}; runs={len(throughputs)}",
            )
        )
    return report, summary


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("csv", nargs="+", help="labeled CSV as label=path")
    parser.add_argument("--out", required=True, help="output directory")
    parser.add_argument("--bootstrap-samples", type=int, default=20_000)
    args = parser.parse_args()
    suites = []
    for item in args.csv:
        if "=" not in item:
            parser.error(f"expected label=path, got {item}")
        label, path = item.split("=", 1)
        suites.append((label, read_csv(path)))
    report, summary = analyze(suites, args.bootstrap_samples)
    os.makedirs(args.out, exist_ok=True)
    with open(os.path.join(args.out, "analysis.json"), "w") as handle:
        json.dump(report, handle, indent=2)
    with open(os.path.join(args.out, "summary.json"), "w") as handle:
        json.dump(summary, handle, indent=2)
    print(json.dumps(report, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
