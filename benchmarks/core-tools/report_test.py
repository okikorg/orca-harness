import importlib.util
import pathlib
import unittest


PATH = pathlib.Path(__file__).with_name("report.py")
SPEC = importlib.util.spec_from_file_location("core_tools_report", PATH)
REPORT = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(REPORT)


class ReportTest(unittest.TestCase):
    def test_percentile_uses_nearest_rank(self):
        values = [float(value) for value in range(1, 21)]
        self.assertEqual(REPORT.percentile(values, 0.50), 10.0)
        self.assertEqual(REPORT.percentile(values, 0.99), 20.0)

    def test_parses_json_lines_and_aggregates_every_scenario(self):
        lines = []
        for tool in ("apply_patch", "multi_edit"):
            for scenario in REPORT.SCENARIOS:
                item = {
                    "tool": tool,
                    "scenario": scenario,
                    "calls": 32 if "distinct" in scenario else 1,
                    "files": 1 if scenario == "repeated_file" else 32,
                    "operations": 32,
                    "bytes_per_file": 1048576,
                    "wall_seconds": 0.016,
                }
                if "distinct" in scenario:
                    item.update(
                        {
                            "serial_seconds": 0.064,
                            "single_average_seconds": 0.002,
                            "speedup": 4.0,
                        }
                    )
                lines.append("[core-tools-bench] " + __import__("json").dumps(item))
        lines[1] = "." + lines[1]
        text = "\n".join(lines)
        parsed = REPORT.parse(text)
        measurements, summary = REPORT.aggregate(parsed, 1)
        self.assertEqual(len(measurements), 10)
        self.assertEqual(len(summary), 38)
        self.assertAlmostEqual(measurements[0]["single_average_p50_seconds"], 0.002)
        self.assertAlmostEqual(
            measurements[0]["throughput_p50_operations_per_second"], 32 / 0.016
        )

    def test_every_median_wall_metric_has_a_budget(self):
        """The gate looks metrics up by name; a rename on either side would
        silently stop enforcing them."""
        budgets_path = pathlib.Path(__file__).parents[1] / "shared" / "check_budgets.py"
        spec = importlib.util.spec_from_file_location("check_budgets", budgets_path)
        assert spec is not None and spec.loader is not None
        check_budgets = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(check_budgets)
        expected = {
            f"core-tools/{tool}/{scenario} wall p50"
            for tool in ("apply_patch", "multi_edit")
            for scenario in REPORT.SCENARIOS
        }
        self.assertEqual(expected, set(check_budgets.CORE_TOOLS_BUDGETS))

    def test_missing_samples_fails_loudly(self):
        with self.assertRaisesRegex(ValueError, "expected 2 samples"):
            REPORT.aggregate(
                REPORT.parse(
                    '[core-tools-bench] {"tool":"apply_patch","scenario":"distinct",'
                    '"calls":1,"files":1,"operations":1,"bytes_per_file":1,'
                    '"wall_seconds":0.001,"serial_seconds":0.001,'
                    '"single_average_seconds":0.001,"speedup":1.0}'
                ),
                2,
            )

    def test_compares_distinct_scenario_to_original_schema(self):
        baseline = {
            "measurements": [
                {
                    "tool": tool,
                    "single_average_p50_seconds": 2.0,
                    "concurrent_wall_p50_seconds": 4.0,
                    "throughput_p50_calls_per_second": 10.0,
                }
                for tool in ("apply_patch", "multi_edit")
            ]
        }
        current = [
            {
                "tool": tool,
                "scenario": "distinct",
                "single_average_p50_seconds": 1.0,
                "wall_p50_seconds": 2.0,
                "throughput_p50_operations_per_second": 20.0,
            }
            for tool in ("apply_patch", "multi_edit")
        ]
        comparison = REPORT.compare_to_baseline(baseline, current)
        self.assertEqual(
            comparison[0]["single_average_p50_seconds"]["change_percent"], -50.0
        )
        self.assertEqual(
            comparison[1]["throughput_p50_calls_per_second"]["change_percent"],
            100.0,
        )


if __name__ == "__main__":
    unittest.main()
