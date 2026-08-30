import tempfile
import unittest
from pathlib import Path

import render_html


def harness_summary(correct, cost, wall):
    return {
        "runs": 1,
        "successful": correct,
        "timeouts": 0,
        "wall_median_ms": wall,
        "ttft_median_ms": 100.0,
        "model_ttft_median_ms": 80.0,
        "total_tokens": 120,
        "tool_calls": 1,
        "turns": 1,
        "normalized_cost_usd": cost,
        "input_tokens": 100,
        "output_tokens": 20,
        "cache_read_tokens": 0,
        "cache_write_tokens": 0,
        "cache_read_share": 0.0,
        "reported_providers": ["openrouter"],
    }


def run_row(harness, success):
    return {
        "task": "lookup",
        "harness": harness,
        "success": success,
        "wall_ms": 500.0,
        "ttft_ms": 100.0,
        "input_tokens": 100,
        "output_tokens": 20,
        "cache_read_tokens": 0,
        "cache_write_tokens": 0,
        "total_tokens": 120,
        "tool_calls": 1,
        "turns": 1,
        "normalized_cost_usd": 0.001,
        "timed_out": False,
        "exit_code": 0,
        "answer": "answer=42" if success else "answer=41",
    }


class RenderHtmlTest(unittest.TestCase):
    def setUp(self):
        self.manifest = {
            "tasks": [
                {
                    "id": "lookup",
                    "mode": "read",
                    "category": "retrieval",
                    "prompt": "Find the answer.",
                    "expected": "answer=42",
                }
            ]
        }

    def render(self, harnesses):
        summary = {
            "model": "anthropic/claude-haiku-4.5",
            "effort": "low",
            "repetitions": 1,
            "harnesses": harnesses,
        }
        runs = [
            run_row(name, item["successful"] == item["runs"])
            for name, item in harnesses.items()
        ]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            return render_html.render(summary, runs, self.manifest, root / "report.html", root)

    def test_four_harness_report_includes_omp_everywhere(self):
        harnesses = {
            "orca": harness_summary(1, 0.001, 400.0),
            "pi": harness_summary(1, 0.002, 500.0),
            "omp": harness_summary(0, 0.003, 600.0),
            "claude": harness_summary(0, 0.004, 700.0),
        }
        report = self.render(harnesses)
        self.assertIn("Oh My Pi", report)
        self.assertIn("1 tasks × 4 harnesses × 1 repetition = 4 sequential attempts", report)
        self.assertEqual(report.count("class='pareto-point "), 4)

    def test_historical_three_harness_report_still_renders(self):
        harnesses = {
            "orca": harness_summary(1, 0.001, 400.0),
            "pi": harness_summary(1, 0.002, 500.0),
            "claude": harness_summary(0, 0.004, 700.0),
        }
        report = self.render(harnesses)
        self.assertNotIn("Oh My Pi", report)
        self.assertIn("1 tasks × 3 harnesses × 1 repetition = 3 sequential attempts", report)
        self.assertEqual(report.count("class='pareto-point "), 3)


if __name__ == "__main__":
    unittest.main()
