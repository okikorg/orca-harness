#!/usr/bin/env python3
"""Tests for the probe parser: `python3 benchmarks/kernel_report_test.py`.

The parser reads the probes' printed tables. Those are `println!` format
strings in examples/fanout_probe.rs and examples/tool_fanout_perf.rs, so
these tests pin the shapes this file has to keep understanding — change a
probe's output and one of these fails instead of the summary going quietly
empty.
"""

import importlib.util
import pathlib
import unittest

MODULE_PATH = pathlib.Path(__file__).with_name("kernel_report.py")
SPEC = importlib.util.spec_from_file_location("kernel_report", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
kernel_report = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(kernel_report)

FANOUT_OUTPUT = """\
dispatch (T1-T0)   n= 100  p50=    19.4µs  p90=    29.5µs  p99=    36.4µs  max=    43.4µs
fan-out (T2-T0)    n= 100  p50=   174.8µs  p90=   241.5µs  p99=   277.8µs  max=   303.8µs
total round-trip   n= 100  p50=   189.4µs  p90=   258.7µs  p99=   324.6µs  max=   330.8µs
"""

TOOLS_OUTPUT = """\

Real core-tool fan-out perf — 4 worker threads, 50 iterations/case

── write_file → distinct paths  (n=100) ──
   fan-out overhead (T2-T0)  p50=   180.0µs  p99=   420.0µs
   total wall clock          p50=  3100.0µs  p99=  4200.0µs
   throughput                     32258 tool calls/sec

── shell → 20ms subprocess each  (n=64) ──
   fan-out overhead (T2-T0)  p50= 38000.0µs  p99= 44000.0µs
   total wall clock          p50= 66000.0µs  p99= 71000.0µs
   serial lower bound 1280.0ms → effective speedup  19.4×
   throughput                       970 tool calls/sec
"""


def by_name(entries: list[dict]) -> dict[str, dict]:
    return {entry["name"]: entry for entry in entries}


class FanoutProbeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.entries = by_name(kernel_report.parse_fanout(FANOUT_OUTPUT))

    def test_every_metric_is_reported_at_p50_and_p99(self) -> None:
        self.assertEqual(6, len(self.entries))
        self.assertIn("dispatch (T1-T0) n=100 p50", self.entries)
        self.assertIn("total round-trip n=100 p99", self.entries)

    def test_microseconds_are_converted_to_seconds(self) -> None:
        self.assertAlmostEqual(0.0002778, self.entries["fan-out (T2-T0) n=100 p99"]["value"])
        self.assertEqual("s", self.entries["fan-out (T2-T0) n=100 p99"]["unit"])

    def test_names_match_the_budget_table(self) -> None:
        """The gate looks metrics up by name; a rename on either side would
        silently stop enforcing them."""
        budgets_path = pathlib.Path(__file__).with_name("check_budgets.py")
        spec = importlib.util.spec_from_file_location("check_budgets", budgets_path)
        assert spec is not None and spec.loader is not None
        check_budgets = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(check_budgets)
        for name in ("dispatch (T1-T0) n=100 p99", "fan-out (T2-T0) n=100 p99"):
            self.assertIn(name, self.entries)
            self.assertIn(name, check_budgets.KERNEL_BUDGETS)


class ToolProbeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.entries = by_name(kernel_report.parse_tools(TOOLS_OUTPUT))

    def test_each_case_is_namespaced_by_label_and_batch_size(self) -> None:
        self.assertIn("tools/write_file → distinct paths n=100 wall p50", self.entries)
        self.assertIn("tools/shell → 20ms subprocess each n=64 wall p99", self.entries)

    def test_wall_clock_is_seconds(self) -> None:
        self.assertAlmostEqual(0.0031, self.entries["tools/write_file → distinct paths n=100 wall p50"]["value"])

    def test_throughput_and_speedup_keep_their_own_units(self) -> None:
        throughput = self.entries["tools/write_file → distinct paths n=100 throughput"]
        self.assertEqual("calls/sec", throughput["unit"])
        self.assertEqual(32258.0, throughput["value"])
        speedup = self.entries["tools/shell → 20ms subprocess each n=64 speedup vs serial"]
        self.assertEqual("x", speedup["unit"])
        self.assertEqual(19.4, speedup["value"])

    def test_lines_before_the_first_case_are_ignored(self) -> None:
        self.assertEqual([], kernel_report.parse_tools("Real core-tool fan-out perf\n"))


if __name__ == "__main__":
    unittest.main()
