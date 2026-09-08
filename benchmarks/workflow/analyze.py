#!/usr/bin/env python3
"""Analyze workflow graph CSVs and write benchmark summary JSON.

Latency and scale are reported as distributions. Correctness is not: with a
deterministic model every ordering, exactly-once, template and join check is
exactly decidable, so any nonzero count is a defect rather than noise, and
this analyzer fails the run on one.
"""

import argparse
import csv
import json
import math
import os
import statistics

FIELDS = {
    "shape": str,
    "stages": int,
    "run": int,
    "wall_us": int,
    "critical_path_us": int,
    "nominal_path_us": int,
    "service_p50_us": int,
    "service_p95_us": int,
    "delay_us": int,
    "limit": int,
    "dispatch_p50_us": int,
    "dispatch_p95_us": int,
    "dispatch_p99_us": int,
    "peak_observed": int,
    "peak_admitted": int,
    "peak_running": int,
    "ordering_violations": int,
    "duplicate_stages": int,
    "missing_stages": int,
    "template_mismatches": int,
    "failures": int,
}

SHAPES = {"chain", "diamond", "wide-join", "fanout", "mesh"}

# Every column whose only acceptable value is zero.
DEFECTS = (
    "ordering_violations",
    "duplicate_stages",
    "missing_stages",
    "template_mismatches",
    "failures",
)


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
            if parsed["shape"] not in SHAPES:
                raise ValueError(f"unknown shape in {path}:{line}: {parsed['shape']}")
            if parsed["stages"] <= 0 or parsed["run"] < 0:
                raise ValueError(f"invalid stages/run in {path}:{line}")
            for name, value in parsed.items():
                if name != "shape" and value < 0:
                    raise ValueError(f"negative {name} in {path}:{line}")
            if parsed["wall_us"] <= 0:
                raise ValueError(f"non-positive wall time in {path}:{line}")
            if not parsed["dispatch_p50_us"] <= parsed["dispatch_p95_us"] <= parsed["dispatch_p99_us"]:
                raise ValueError(f"invalid dispatch ordering in {path}:{line}")
            if parsed["delay_us"] <= 0:
                raise ValueError(f"non-positive model delay in {path}:{line}")
            if parsed["service_p50_us"] > parsed["service_p95_us"]:
                raise ValueError(f"invalid service ordering in {path}:{line}")
            # The critical path is a sub-path of the run; it cannot exceed it.
            if parsed["critical_path_us"] > parsed["wall_us"]:
                raise ValueError(f"critical path exceeds wall time in {path}:{line}")
            for name in ("peak_observed", "peak_admitted", "peak_running"):
                if parsed[name] > parsed["stages"]:
                    raise ValueError(f"{name} exceeds stage count in {path}:{line}")
            # A slot is held for the whole of a stage's execution, so the
            # harness can never have more running than it admitted.
            if parsed["peak_running"] > parsed["peak_admitted"]:
                raise ValueError(f"running peak above admitted peak in {path}:{line}")
            if parsed["limit"] and parsed["peak_running"] > parsed["limit"]:
                raise ValueError(f"running peak above the concurrency limit in {path}:{line}")
            rows.append(parsed)
    if not rows:
        raise ValueError(f"no benchmark rows in {path}")
    return rows


def defects(rows: list[dict]) -> dict:
    return {name: sum(row[name] for row in rows) for name in DEFECTS}


def case_report(rows: list[dict]) -> dict:
    """One shape at one stage count, across its repetitions."""
    wall = statistics.median(row["wall_us"] for row in rows)
    critical = statistics.median(row["critical_path_us"] for row in rows)
    nominal = rows[0]["nominal_path_us"]
    service = statistics.median(row["service_p50_us"] for row in rows)
    # How far a fixed-delay model call stretched under load. Well above 1 means
    # the runtime is saturated and wall time stops being an engine measurement:
    # the delay lands inside the model call rather than between stages.
    delay = rows[0]["delay_us"]
    inflation = service / delay if delay else 0.0
    return {
        "shape": rows[0]["shape"],
        "stages": rows[0]["stages"],
        # 0 is the unbounded limit the engine treats as "no cap".
        "limit": rows[0]["limit"],
        "repetitions": len(rows),
        "wallUsMedian": wall,
        "criticalPathUsMedian": critical,
        "nominalPathUs": nominal,
        "schedulingOverheadUsMedian": max(0.0, wall - critical),
        "dispatchP50UsMedian": statistics.median(row["dispatch_p50_us"] for row in rows),
        "dispatchP95UsMedian": statistics.median(row["dispatch_p95_us"] for row in rows),
        "dispatchP99UsMax": max(row["dispatch_p99_us"] for row in rows),
        "serviceP50UsMedian": service,
        "serviceInflation": round(inflation, 3) if inflation else 0.0,
        "peakObservedMax": max(row["peak_observed"] for row in rows),
        "peakAdmittedMax": max(row["peak_admitted"] for row in rows),
        "peakRunningMax": max(row["peak_running"] for row in rows),
        "defects": defects(rows),
    }


def analyze(suites: list[tuple[str, list[dict]]]) -> tuple[dict, list[dict]]:
    all_rows = [row for _, rows in suites for row in rows]
    # The concurrency limit is an independent variable, not a repetition of
    # the same case: grouping without it would average two different runs.
    cases = []
    keys = sorted({(row["shape"], row["stages"], row["limit"]) for row in all_rows})
    for shape, stages, limit in keys:
        matching = [
            row
            for row in all_rows
            if (row["shape"], row["stages"], row["limit"]) == (shape, stages, limit)
        ]
        cases.append(case_report(matching))

    totals = defects(all_rows)
    stages_run = sum(row["stages"] for row in all_rows)
    largest = max(cases, key=lambda case: case["stages"])
    # Peak concurrency the harness itself admitted, which is the scale claim.
    peak = max(row["peak_admitted"] for row in all_rows)
    report = {
        "schemaVersion": 1,
        "runs": len(all_rows),
        "stagesExecuted": stages_run,
        "defectsTotal": totals,
        "clean": all(count == 0 for count in totals.values()),
        "peakAdmitted": peak,
        "largestCase": {"shape": largest["shape"], "stages": largest["stages"]},
        "cases": cases,
        "suites": {name: len(rows) for name, rows in suites},
    }

    # Only shape-independent, comparable numbers become tracked entries: a
    # per-shape dispatch median, and the run-wide correctness total.
    entries = [
        {
            "name": (
                f"workflow dispatch p50 · {case['shape']} {case['stages']}"
                + (f" limit {case['limit']}" if case["limit"] else "")
            ),
            "unit": "us",
            "value": case["dispatchP50UsMedian"],
            "range": "± 0",
            "extra": (
                f"{case['repetitions']} runs; wall {case['wallUsMedian']:.0f}us; "
                f"critical {case['criticalPathUsMedian']:.0f}us; "
                f"peak admitted {case['peakAdmittedMax']}"
            ),
        }
        for case in cases
    ]
    entries.append(
        {
            "name": "workflow defects",
            "unit": "count",
            "value": sum(totals.values()),
            "range": "± 0",
            "extra": json.dumps(totals, sort_keys=True) + f"; {stages_run} stages executed",
        }
    )
    return report, entries


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("suites", nargs="+", help="name=path.csv")
    parser.add_argument("--out", required=True, help="output directory")
    args = parser.parse_args()

    suites = []
    for suite in args.suites:
        if "=" not in suite:
            raise SystemExit(f"expected name=path, got {suite}")
        name, path = suite.split("=", 1)
        suites.append((name, read_csv(path)))

    report, entries = analyze(suites)
    os.makedirs(args.out, exist_ok=True)
    with open(os.path.join(args.out, "analysis.json"), "w") as handle:
        json.dump(report, handle, indent=2)
        handle.write("\n")
    with open(os.path.join(args.out, "summary.json"), "w") as handle:
        json.dump(entries, handle, indent=2)
        handle.write("\n")

    for case in report["cases"]:
        print(
            f"{case['shape']:>10} {case['stages']:>6} stages  "
            f"{('limit ' + str(case['limit'])) if case['limit'] else 'unbounded':>9}  "
            f"wall {case['wallUsMedian'] / 1000:8.1f}ms  "
            f"critical {case['criticalPathUsMedian'] / 1000:8.1f}ms  "
            f"dispatch p50 {case['dispatchP50UsMedian']:>8.0f}us  "
            f"p99max {case['dispatchP99UsMax']:>9}us  "
            f"peak {case['peakAdmittedMax']:>5} admitted "
            f"{case['peakRunningMax']:>5} running"
        )
    print(
        f"\n{report['runs']} runs, {report['stagesExecuted']} stages executed, "
        f"peak admitted {report['peakAdmitted']}"
    )
    if not report["clean"]:
        print(f"DEFECTS: {report['defectsTotal']}")
        return 1
    print("no ordering, duplicate, missing, template, join or failure defects")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
