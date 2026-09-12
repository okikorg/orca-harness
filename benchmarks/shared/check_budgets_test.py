#!/usr/bin/env python3
"""Tests for the budget gate: `python3 benchmarks/shared/check_budgets_test.py`."""

import contextlib
import importlib.util
import io
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest

MODULE_PATH = pathlib.Path(__file__).with_name("check_budgets.py")
SPEC = importlib.util.spec_from_file_location("check_budgets", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
check_budgets = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(check_budgets)


def hyperfine_result(command: str, mean: float) -> dict:
    return {
        "results": [
            {
                "command": command,
                "mean": mean,
                "stddev": mean / 20,
                "median": mean,
                "min": mean * 0.9,
                "max": mean * 1.1,
                "times": [mean] * 20,
            }
        ]
    }


class GateTests(unittest.TestCase):
    """The gate is what CI trusts, so exit codes are the contract."""

    def run_gate(self, suite: str, files: dict[str, dict], enforce: bool = True,
                 complete_startup: bool = True):
        files = dict(files)
        if suite == "startup" and files and complete_startup:
            present = {
                body["results"][0]["command"]
                for name, body in files.items() if name != "summary.json"
            }
            for index, name in enumerate(sorted(check_budgets.STRICT_BUDGETS - present)):
                files[f"required-{index}.json"] = hyperfine_result(name, 0.003)
        with tempfile.TemporaryDirectory() as tmp:
            directory = pathlib.Path(tmp)
            for name, body in files.items():
                (directory / name).write_text(json.dumps(body))
            env = os.environ.copy()
            env["ORCA_BENCH_ENFORCE"] = "1" if enforce else "0"
            return subprocess.run(
                [sys.executable, str(MODULE_PATH), "--suite", suite, "--dir", str(directory)],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )

    def test_startup_within_budget_passes(self) -> None:
        result = self.run_gate(
            "startup", {"startup.json": hyperfine_result("orcacode (startup)", 0.003)}
        )
        self.assertEqual(0, result.returncode, result.stdout + result.stderr)
        self.assertIn("PASS", result.stdout)

    def test_startup_over_budget_fails(self) -> None:
        result = self.run_gate(
            "startup", {"startup.json": hyperfine_result("orcacode (startup)", 0.5)}
        )
        self.assertEqual(1, result.returncode, result.stdout)
        self.assertIn("FAIL", result.stdout)
        self.assertIn("Latency budget exceeded", result.stdout)

    def test_unbudgeted_startup_command_falls_back_to_the_default(self) -> None:
        result = self.run_gate(
            "startup", {"new.json": hyperfine_result("orcacode (something new)", 0.5)}
        )
        self.assertEqual(1, result.returncode, result.stdout)

    def test_baseline_is_reported_but_never_gated(self) -> None:
        result = self.run_gate(
            "startup", {"baseline.json": hyperfine_result("process baseline", 9.0)}
        )
        self.assertEqual(0, result.returncode, result.stdout)
        self.assertIn("BASE", result.stdout)

    def test_summary_json_is_not_read_as_a_result(self) -> None:
        result = self.run_gate(
            "startup",
            {
                "startup.json": hyperfine_result("orcacode (startup)", 0.003),
                # Written by summarize.py into the same directory; it has no
                # "results" key and would crash a naive glob.
                "summary.json": [{"name": "orcacode (startup)", "unit": "s", "value": 0.003}],
            },
        )
        self.assertEqual(0, result.returncode, result.stdout + result.stderr)

    def test_kernel_over_budget_fails(self) -> None:
        result = self.run_gate(
            "kernel",
            {
                "summary.json": [
                    {
                        "name": "fan-out (T2-T0) n=100 p99",
                        "unit": "s",
                        "value": 0.05,
                        "extra": "",
                    }
                ]
            },
        )
        self.assertEqual(1, result.returncode, result.stdout)
        self.assertIn("FAIL", result.stdout)

    def test_kernel_ignores_non_latency_units(self) -> None:
        """Throughput is higher-is-better; gating it against a latency
        ceiling would pass for exactly the wrong reason."""
        result = self.run_gate(
            "kernel",
            {
                "summary.json": [
                    {"name": "tools/x n=100 throughput", "unit": "calls/sec", "value": 32000.0}
                ]
            },
        )
        self.assertEqual(1, result.returncode, result.stdout)
        self.assertIn("No kernel results", result.stdout)

    def test_kernel_metric_without_a_target_is_informational(self) -> None:
        result = self.run_gate(
            "kernel",
            {"summary.json": [{"name": "fan-out (T2-T0) n=100 p50", "unit": "s", "value": 9.0}]},
        )
        self.assertEqual(0, result.returncode, result.stdout)
        self.assertIn("INFO", result.stdout)

    def test_criterion_case_in_the_kernel_summary_is_gated(self) -> None:
        """kernel/report.py --criterion folds cargo bench estimates into the
        same summary; they are held to their own table, not left INFO."""
        result = self.run_gate(
            "kernel",
            {
                "summary.json": [
                    {
                        "name": "criterion/dispatch/noop_calls/100",
                        "unit": "s",
                        "value": 0.5,
                        "extra": "median=180.000µs",
                    }
                ]
            },
        )
        self.assertEqual(1, result.returncode, result.stdout)
        self.assertIn("FAIL", result.stdout)

    def test_criterion_sqlite_floor_is_informational(self) -> None:
        result = self.run_gate(
            "kernel",
            {
                "summary.json": [
                    {"name": "criterion/memory_fts_50k/raw_sql/broad", "unit": "s", "value": 9.0}
                ]
            },
        )
        self.assertEqual(0, result.returncode, result.stdout)
        self.assertIn("INFO", result.stdout)

    def test_core_tools_median_over_budget_fails(self) -> None:
        result = self.run_gate(
            "core-tools",
            {
                "summary.json": [
                    {
                        "name": "core-tools/apply_patch/distinct wall p50",
                        "unit": "s",
                        "value": 2.0,
                        "extra": "32 calls, 32 files",
                    }
                ]
            },
        )
        self.assertEqual(1, result.returncode, result.stdout)
        self.assertIn("FAIL", result.stdout)

    def test_core_tools_p99_and_throughput_are_not_gated(self) -> None:
        """Twenty samples make a p99 the maximum, and throughput is
        higher-is-better; both are context, not a verdict."""
        result = self.run_gate(
            "core-tools",
            {
                "summary.json": [
                    {"name": "core-tools/apply_patch/distinct wall p50", "unit": "s", "value": 0.007},
                    {"name": "core-tools/apply_patch/distinct wall p99", "unit": "s", "value": 9.0},
                    {
                        "name": "core-tools/apply_patch/distinct throughput p50",
                        "unit": "operations/sec",
                        "value": 1.0,
                    },
                ]
            },
        )
        self.assertEqual(0, result.returncode, result.stdout)
        self.assertIn("PASS", result.stdout)
        self.assertIn("INFO", result.stdout)
        self.assertNotIn("throughput", result.stdout)

    def test_missing_results_fail(self) -> None:
        result = self.run_gate("startup", {})
        self.assertEqual(1, result.returncode, result.stdout)
        self.assertIn("No startup results found", result.stdout)

    def test_missing_required_startup_measurement_fails(self) -> None:
        for name in sorted(check_budgets.STRICT_BUDGETS):
            with self.subTest(name=name):
                result = self.run_gate(
                    "startup", {"one.json": hyperfine_result(name, 0.003)},
                    complete_startup=False,
                )
                self.assertEqual(1, result.returncode, result.stdout)
                self.assertIn("Missing required startup measurements", result.stdout)

    def test_budgets_are_not_enforced_off_linux_by_default(self) -> None:
        result = self.run_gate(
            "startup", {"startup.json": hyperfine_result("orcacode (startup)", 0.5)}, enforce=False
        )
        expected = 1 if check_budgets.platform.system() == "Linux" else 0
        self.assertEqual(expected, result.returncode, result.stdout)


class UnitTests(unittest.TestCase):
    def gate(self, value: float) -> bool:
        measurement = check_budgets.Measurement("orcacode (startup)", value, "")
        with contextlib.redirect_stdout(io.StringIO()):
            return check_budgets.check([measurement], check_budgets.STARTUP_BUDGETS, None, True)

    def test_startup_budget_is_strict(self) -> None:
        self.assertTrue(self.gate(0.003269))
        self.assertFalse(self.gate(0.00327))
        self.assertFalse(self.gate(0.003271))

    def test_invalid_startup_values_fail(self) -> None:
        for value in [-1, float("nan"), float("inf")]:
            with self.subTest(value=value):
                self.assertFalse(self.gate(value))

    def test_new_session_budget_is_strict(self) -> None:
        for value, expected in [(0.003269, True), (0.00327, False), (0.003271, False)]:
            with self.subTest(value=value), contextlib.redirect_stdout(io.StringIO()):
                measurement = check_budgets.Measurement("orcacode (startup, new session)", value, "")
                self.assertEqual(expected, check_budgets.check(
                    [measurement], check_budgets.STARTUP_BUDGETS, None, True
                ))

    def test_other_budgets_remain_inclusive(self) -> None:
        with contextlib.redirect_stdout(io.StringIO()):
            measurement = check_budgets.Measurement("orcacode (resume)", 0.020, "")
            self.assertTrue(check_budgets.check(
                [measurement], check_budgets.STARTUP_BUDGETS, None, True
            ))

    def test_durations_switch_units_below_a_millisecond(self) -> None:
        self.assertEqual("4.8µs", check_budgets.duration(0.0000048))
        self.assertEqual("2.80ms", check_budgets.duration(0.0028))

    def test_enforcement_follows_platform_unless_overridden(self) -> None:
        os.environ.pop("ORCA_BENCH_ENFORCE", None)
        self.assertTrue(check_budgets.enforcing("Linux"))
        self.assertFalse(check_budgets.enforcing("Darwin"))
        os.environ["ORCA_BENCH_ENFORCE"] = "1"
        try:
            self.assertTrue(check_budgets.enforcing("Darwin"))
        finally:
            os.environ.pop("ORCA_BENCH_ENFORCE", None)


if __name__ == "__main__":
    unittest.main()
