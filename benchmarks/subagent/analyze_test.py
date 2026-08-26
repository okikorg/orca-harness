#!/usr/bin/env python3

import csv
import os
import tempfile
import unittest

import analyze


class AnalyzeTest(unittest.TestCase):
    def test_wilson_zero_failures_matches_reported_bound(self):
        low, high = analyze.wilson(0, 1_444_844)
        self.assertEqual(low, 0)
        self.assertAlmostEqual(high, 2.658728975097685e-6)

    def test_bootstrap_is_seeded_and_bounded_by_observations(self):
        values = [140_836.928, 169_495.865, 176_245.242]
        first = analyze.bootstrap_median(values, samples=2_000)
        second = analyze.bootstrap_median(values, samples=2_000)
        self.assertEqual(first, second)
        self.assertGreaterEqual(first[0], min(values))
        self.assertLessEqual(first[1], max(values))

    def test_analysis_counts_calls_failures_and_true_peak_batches(self):
        rows = [
            {"fanout": 4, "run": 0, "wall_us": 10, "throughput_per_s": 4.0,
             "p50_us": 5, "p95_us": 8, "p99_us": 9, "failures": 0, "peak_active": 4},
            {"fanout": 8, "run": 0, "wall_us": 20, "throughput_per_s": 8.0,
             "p50_us": 10, "p95_us": 18, "p99_us": 19, "failures": 1, "peak_active": 7},
        ]
        report, _ = analyze.analyze([("test", rows)], bootstrap_samples=100)
        self.assertEqual(report["calls"], 12)
        self.assertEqual(report["failures"], 1)
        self.assertEqual(report["maximumObservedActive"], 7)
        self.assertEqual(report["maximumObservedRuns"], 1)
        self.assertEqual(report["statisticsFanout"], 4)
        self.assertEqual(report["statisticsRuns"], 1)

    def test_analysis_reports_no_statistics_fanout_without_full_overlap(self):
        rows = [
            {"fanout": 4, "run": 0, "wall_us": 10, "throughput_per_s": 4.0,
             "p50_us": 5, "p95_us": 8, "p99_us": 9, "failures": 0, "peak_active": 3},
            {"fanout": 8, "run": 0, "wall_us": 20, "throughput_per_s": 8.0,
             "p50_us": 10, "p95_us": 18, "p99_us": 19, "failures": 0, "peak_active": 7},
        ]
        report, summary = analyze.analyze([("test", rows)], bootstrap_samples=100)
        self.assertIsNone(report["statisticsFanout"])
        self.assertEqual(report["statisticsRuns"], 0)
        self.assertIsNone(report["maximumObservedMedianThroughputPerSecond"])
        self.assertFalse(any("throughput" in entry["name"] for entry in summary))

    def test_csv_rejects_extra_invalid_and_non_finite_values(self):
        header = list(analyze.FIELDS)
        invalid = [
            header + ["extra"],
            [1, 0, 10, 1.0, 1, 2, 3, 0, 1, "extra"],
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "extra.csv")
            with open(path, "w", newline="") as handle:
                csv.writer(handle).writerows(invalid)
            with self.assertRaises(ValueError):
                analyze.read_csv(path)

            for name, value in [("fanout", "0"), ("throughput_per_s", "nan"),
                                ("throughput_per_s", "-1"), ("peak_active", "2")]:
                row = ["1", "0", "10", "1.0", "1", "2", "3", "0", "1"]
                row[header.index(name)] = value
                with open(path, "w", newline="") as handle:
                    csv.writer(handle).writerows([header, row])
                with self.assertRaises(ValueError, msg=name):
                    analyze.read_csv(path)

    def test_csv_header_is_strict(self):
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "bad.csv")
            with open(path, "w", newline="") as handle:
                csv.writer(handle).writerows([["fanout", "run"], [1, 0]])
            with self.assertRaises(ValueError):
                analyze.read_csv(path)


if __name__ == "__main__":
    unittest.main()
