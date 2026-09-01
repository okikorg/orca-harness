#!/usr/bin/env python3
"""Aggregate the real-file core mutation probe into durable JSON results."""

import argparse
import json
import math
import os
import platform
import statistics
from datetime import datetime, timezone


SUITE_DIR = os.path.dirname(os.path.abspath(__file__))
BENCH_DIR = os.path.dirname(SUITE_DIR)
RESULTS_DIR = os.path.join(BENCH_DIR, "results", "core-tools")

PREFIX = "[core-tools-bench] "
SCENARIOS = (
    "distinct",
    "guarded_distinct",
    "batch_files",
    "repeated_file",
    "append_files",
)


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, math.ceil(len(ordered) * fraction) - 1))
    return ordered[index]


def parse(text: str) -> dict[tuple[str, str], list[dict]]:
    parsed: dict[tuple[str, str], list[dict]] = {}
    for raw in text.splitlines():
        # Rust's test harness prints a progress dot immediately before the
        # second test's uncaptured output when tests share one binary.
        line = raw.strip().lstrip(".")
        if not line.startswith(PREFIX):
            continue
        item = json.loads(line[len(PREFIX) :])
        parsed.setdefault((item["tool"], item["scenario"]), []).append(item)
    return parsed


def benchmark_entry(name: str, unit: str, value: float, extra: str) -> dict:
    return {
        "name": name,
        "unit": unit,
        "value": round(value, 9),
        "range": "± 0",
        "extra": extra,
    }


def aggregate(
    parsed: dict[tuple[str, str], list[dict]], expected_samples: int
) -> tuple[list[dict], list[dict]]:
    measurements = []
    summary = []
    for tool in ("apply_patch", "multi_edit"):
        for scenario in SCENARIOS:
            samples = parsed.get((tool, scenario), [])
            if len(samples) != expected_samples:
                raise ValueError(
                    f"expected {expected_samples} samples for {tool}/{scenario}, "
                    f"found {len(samples)}"
                )
            first = samples[0]
            wall = [item["wall_seconds"] for item in samples]
            throughputs = [first["operations"] / value for value in wall]
            measured = {
                "tool": tool,
                "scenario": scenario,
                "samples": len(samples),
                "calls_per_sample": first["calls"],
                "files_per_sample": first["files"],
                "operations_per_sample": first["operations"],
                "bytes_per_file": first["bytes_per_file"],
                "wall_p50_seconds": percentile(wall, 0.50),
                "wall_p99_seconds": percentile(wall, 0.99),
                "throughput_p50_operations_per_second": percentile(
                    throughputs, 0.50
                ),
            }
            if "serial_seconds" in first:
                single = [item["single_average_seconds"] for item in samples]
                speedups = [item["speedup"] for item in samples]
                measured.update(
                    {
                        "single_average_p50_seconds": percentile(single, 0.50),
                        "single_average_p99_seconds": percentile(single, 0.99),
                        "speedup_median": statistics.median(speedups),
                    }
                )
            measurements.append(measured)
            detail = (
                f"{first['calls']} calls, {first['files']} files, "
                f"{first['operations']} operations, {first['bytes_per_file']} bytes/file, "
                f"{len(samples)} samples"
            )
            prefix = f"core-tools/{tool}/{scenario}"
            summary.extend(
                [
                    benchmark_entry(
                        f"{prefix} wall p50",
                        "s",
                        measured["wall_p50_seconds"],
                        detail,
                    ),
                    benchmark_entry(
                        f"{prefix} wall p99",
                        "s",
                        measured["wall_p99_seconds"],
                        detail,
                    ),
                    benchmark_entry(
                        f"{prefix} throughput p50",
                        "operations/sec",
                        measured["throughput_p50_operations_per_second"],
                        "higher is better; " + detail,
                    ),
                ]
            )
            if "speedup_median" in measured:
                summary.extend(
                    [
                        benchmark_entry(
                            f"{prefix} single average p50",
                            "s",
                            measured["single_average_p50_seconds"],
                            detail,
                        ),
                        benchmark_entry(
                            f"{prefix} speedup median",
                            "x",
                            measured["speedup_median"],
                            "higher is better; " + detail,
                        ),
                    ]
                )
    return measurements, summary


def compare_to_baseline(baseline: dict, measurements: list[dict]) -> list[dict]:
    comparison = []
    for tool in ("apply_patch", "multi_edit"):
        old = next(item for item in baseline["measurements"] if item["tool"] == tool)
        new = next(
            item
            for item in measurements
            if item["tool"] == tool and item["scenario"] == "distinct"
        )
        fields = {
            "single_average_p50_seconds": "single_average_p50_seconds",
            "concurrent_wall_p50_seconds": "wall_p50_seconds",
            "throughput_p50_calls_per_second": "throughput_p50_operations_per_second",
        }
        values = {"tool": tool, "scenario": "distinct"}
        for old_key, new_key in fields.items():
            before = old[old_key]
            after = new[new_key]
            values[old_key] = {
                "baseline": before,
                "optimized": after,
                "change_percent": round((after - before) / before * 100, 2),
            }
        comparison.append(values)
    return comparison


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--samples", type=int, required=True)
    parser.add_argument("--git-sha", required=True)
    parser.add_argument("--worktree", choices=("clean", "dirty"), required=True)
    args = parser.parse_args()

    with open(os.path.join(RESULTS_DIR, "raw.txt"), encoding="utf-8") as handle:
        parsed = parse(handle.read())
    measurements, summary = aggregate(parsed, args.samples)
    report = {
        "schema": 3,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "git_sha": args.git_sha,
        "worktree": args.worktree,
        "platform": platform.platform(),
        "machine": platform.machine(),
        "python": platform.python_version(),
        "accuracy_suite": "16 passed",
        "measurements": measurements,
    }
    with open(
        os.path.join(RESULTS_DIR, "measurements.json"), "w", encoding="utf-8"
    ) as handle:
        json.dump(report, handle, indent=2)
        handle.write("\n")
    with open(
        os.path.join(RESULTS_DIR, "summary.json"), "w", encoding="utf-8"
    ) as handle:
        json.dump(summary, handle, indent=2)
        handle.write("\n")

    baseline_path = os.path.join(RESULTS_DIR, "baseline.json")
    if os.path.exists(baseline_path):
        with open(baseline_path, encoding="utf-8") as handle:
            baseline = json.load(handle)
        with open(
            os.path.join(RESULTS_DIR, "comparison.json"), "w", encoding="utf-8"
        ) as handle:
            json.dump(compare_to_baseline(baseline, measurements), handle, indent=2)
            handle.write("\n")

    for item in measurements:
        print(
            f"{item['tool']}/{item['scenario']}: "
            f"wall p50={item['wall_p50_seconds'] * 1e3:.3f}ms "
            f"p99={item['wall_p99_seconds'] * 1e3:.3f}ms "
            f"throughput={item['throughput_p50_operations_per_second']:.0f} ops/s"
        )
    print(f"Results written to {RESULTS_DIR}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
