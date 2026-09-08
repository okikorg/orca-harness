"""The gate decides whether a paid nightly run is a pass, so it is tested
against summaries shaped like the ones analyze.py writes -- including the
two real regressions it was built from."""

import json
import os
import unittest

from check_budgets import MAXIMA, MINIMA, check


def summary(**overrides):
    orca = {
        "answer_correct": 44,
        "cache_read_share": 0.163,
        "normalized_cost_per_success_usd": 0.0097,
        "total_tokens": 390_294,
        "turns": 172,
        "timeouts": 0,
        "ttft_median_ms": 952.3,
    }
    orca.update(overrides)
    return {"harnesses": {"orca": orca, "pi": {"answer_correct": 1}}}


class CheckBudgets(unittest.TestCase):
    def test_a_healthy_run_reports_no_failures(self):
        self.assertEqual(check(summary()), [])

    def test_the_blocking_catalog_fetch_regression_is_caught(self):
        # The 20260908T094415Z run: a network round trip landed in front of
        # the loop and pushed end-to-end TTFT to 5150ms.
        failures = check(summary(ttft_median_ms=5150.4, cache_read_share=0.0))
        self.assertEqual(len(failures), 2)
        self.assertTrue(any("ttft_median_ms" in f for f in failures))
        self.assertTrue(any("cache_read_share" in f for f in failures))

    def test_an_exploration_blowup_is_caught(self):
        # The 20260908T114413Z prompt revision: correct answers held, but
        # the model wandered and tokens went to 498k.
        failures = check(summary(total_tokens=620_000, turns=240))
        self.assertTrue(any("total_tokens" in f for f in failures))
        self.assertTrue(any("turns" in f for f in failures))

    def test_a_correctness_collapse_is_caught(self):
        failures = check(summary(answer_correct=31))
        self.assertEqual(len(failures), 1)
        self.assertIn("below the floor", failures[0])

    def test_the_observed_flaky_task_alone_does_not_trip_the_gate(self):
        # role-permissions is 1-of-3 on the baseline itself; 43/48 is a
        # normal run, not a regression.
        self.assertEqual(check(summary(answer_correct=43)), [])

    def test_a_missing_metric_fails_rather_than_passing_silently(self):
        broken = summary()
        del broken["harnesses"]["orca"]["total_tokens"]
        failures = check(broken)
        self.assertEqual(failures, ["total_tokens: missing from the summary"])

    def test_every_budget_names_a_metric_the_analyzer_really_writes(self):
        # Checked against a recorded run rather than the fixture above: a
        # budget keyed on a name analyze.py does not emit reads as "missing"
        # and fails every night, which is how startup_median_ms was caught.
        recorded = os.path.join(
            os.path.dirname(os.path.abspath(__file__)),
            "..",
            "results",
            "harness-comparison",
            "20260908T115748Z",
            "summary.json",
        )
        if not os.path.exists(recorded):
            self.skipTest("no recorded run to check the budget keys against")
        with open(recorded, encoding="utf-8") as handle:
            written = set(json.load(handle)["harnesses"]["orca"])
        self.assertEqual(set(), (set(MINIMA) | set(MAXIMA)) - written)


if __name__ == "__main__":
    unittest.main()
