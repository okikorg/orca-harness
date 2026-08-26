#!/usr/bin/env python3
"""Parse the kernel probes into benchmark-action format and print a table.

The probes print human-readable tables; this turns them into the same
summary.json shape summarize.py writes for hyperfine, so both suites feed
one CI chart and one budget checker.

Inputs (whichever exist under benchmarks/results/kernel/):

  fanout-<n>.txt   examples/fanout_probe — dispatch and fan-out percentiles
                   with no-op tools, i.e. pure harness overhead
  tools.txt        examples/tool_fanout_perf — the same dispatcher driving
                   the real write_file / read_file / shell tools
  target/criterion the criterion suite's own estimates, when --criterion
                   asked for them
"""

import argparse
import glob
import json
import os
import re
import sys

SUITE_DIR = os.path.dirname(os.path.abspath(__file__))
BENCH_DIR = os.path.dirname(SUITE_DIR)
REPO_ROOT = os.path.dirname(BENCH_DIR)

# "dispatch (T1-T0) n=   1  p50=     0.4µs  p90=..."
PROBE_LINE = re.compile(
    r"^(?P<name>\S.*?)\s+n=\s*(?P<n>\d+)\s+"
    r"p50=\s*(?P<p50>[\d.]+)µs\s+p90=\s*(?P<p90>[\d.]+)µs\s+"
    r"p99=\s*(?P<p99>[\d.]+)µs\s+max=\s*(?P<max>[\d.]+)µs"
)
# "── write_file → distinct paths  (n=100) ──"
CASE_LINE = re.compile(r"^──\s*(?P<label>.+?)\s+\(n=(?P<n>\d+)\)\s*──")
# "   total wall clock          p50=  3100.0µs  p99=  4200.0µs"
CASE_METRIC = re.compile(
    r"^\s+(?P<metric>fan-out overhead \(T2-T0\)|total wall clock)\s+"
    r"p50=\s*(?P<p50>[\d.]+)µs\s+p99=\s*(?P<p99>[\d.]+)µs"
)
CASE_THROUGHPUT = re.compile(r"^\s+throughput\s+(?P<value>[\d.]+)\s+tool calls/sec")
CASE_SPEEDUP = re.compile(r"effective speedup\s+(?P<value>[\d.]+)×")


def entry(name: str, seconds: float, extra: str) -> dict:
    return {
        "name": name,
        "unit": "s",
        "value": round(seconds, 9),
        "range": "± 0",
        "extra": extra,
    }


def parse_fanout(text: str) -> list[dict]:
    entries = []
    for line in text.splitlines():
        match = PROBE_LINE.match(line)
        if not match:
            continue
        base = f"{match['name']} n={match['n']}"
        extra = f"p50={match['p50']}µs p90={match['p90']}µs max={match['max']}µs"
        for percentile in ("p50", "p99"):
            entries.append(
                entry(f"{base} {percentile}", float(match[percentile]) / 1e6, extra)
            )
    return entries


def parse_tools(text: str) -> list[dict]:
    entries = []
    case = None
    for line in text.splitlines():
        header = CASE_LINE.match(line)
        if header:
            case = f"tools/{header['label']} n={header['n']}"
            continue
        if case is None:
            continue
        metric = CASE_METRIC.match(line)
        if metric:
            label = "fan-out" if metric["metric"].startswith("fan-out") else "wall"
            for percentile in ("p50", "p99"):
                entries.append(
                    entry(
                        f"{case} {label} {percentile}",
                        float(metric[percentile]) / 1e6,
                        f"{metric['metric']} p50={metric['p50']}µs p99={metric['p99']}µs",
                    )
                )
            continue
        throughput = CASE_THROUGHPUT.match(line)
        if throughput:
            entries.append(
                {
                    "name": f"{case} throughput",
                    "unit": "calls/sec",
                    "value": float(throughput["value"]),
                    "range": "± 0",
                    "extra": "higher is better",
                }
            )
        speedup = CASE_SPEEDUP.search(line)
        if speedup:
            entries.append(
                {
                    "name": f"{case} speedup vs serial",
                    "unit": "x",
                    "value": float(speedup["value"]),
                    "range": "± 0",
                    "extra": "higher is better",
                }
            )
    return entries


def parse_criterion(criterion_dir: str) -> list[dict]:
    """Criterion's own estimates, so `cargo bench` results land in the same
    summary instead of only in target/criterion HTML."""
    entries = []
    for path in sorted(glob.glob(os.path.join(criterion_dir, "**", "new", "estimates.json"), recursive=True)):
        bench_dir = os.path.dirname(path)
        name_path = os.path.join(bench_dir, "benchmark.json")
        if os.path.isfile(name_path):
            with open(name_path) as handle:
                name = json.load(handle).get("full_id")
        else:
            name = os.path.relpath(os.path.dirname(bench_dir), criterion_dir)
        with open(path) as handle:
            estimates = json.load(handle)
        mean_ns = estimates["mean"]["point_estimate"]
        median_ns = estimates.get("median", {}).get("point_estimate", mean_ns)
        entries.append(
            entry(
                f"criterion/{name}",
                mean_ns / 1e9,
                f"median={median_ns / 1e3:.3f}µs",
            )
        )
    return entries


def render(entries: list[dict]) -> None:
    print(f"{'METRIC':<48} {'VALUE':>14}")
    print(f"{'-' * 48} {'-' * 14:>14}")
    for item in entries:
        if item["unit"] == "s":
            value = f"{item['value'] * 1e6:.1f}µs"
        elif item["unit"] == "calls/sec":
            value = f"{item['value']:.0f}/s"
        else:
            value = f"{item['value']:.1f}{item['unit']}"
        print(f"{item['name']:<48} {value:>14}")
    print()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dir", default=os.path.join(BENCH_DIR, "results", "kernel"))
    parser.add_argument(
        "--criterion",
        action="store_true",
        help="also fold target/criterion estimates into the summary",
    )
    args = parser.parse_args()

    entries: list[dict] = []
    for path in sorted(glob.glob(os.path.join(args.dir, "fanout-*.txt"))):
        with open(path) as handle:
            entries.extend(parse_fanout(handle.read()))
    tools_path = os.path.join(args.dir, "tools.txt")
    if os.path.isfile(tools_path):
        with open(tools_path) as handle:
            entries.extend(parse_tools(handle.read()))
    if args.criterion:
        entries.extend(parse_criterion(os.path.join(REPO_ROOT, "target", "criterion")))

    if not entries:
        print(f"error: no probe output to parse in {args.dir}", file=sys.stderr)
        return 1

    render(entries)
    out_path = os.path.join(args.dir, "summary.json")
    with open(out_path, "w") as handle:
        json.dump(entries, handle, indent=2)
    print(f"Results written to {out_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
