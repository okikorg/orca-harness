#!/usr/bin/env python3
"""Tests for the workflow benchmark analyzer."""

import io
import json
import os
import tempfile
import unittest
import unittest.mock
from contextlib import redirect_stdout

import analyze

HEADER = ",".join(analyze.FIELDS)

# shape,stages,run,wall,critical,nominal,svc50,svc95,delay,d50,d95,d99,
# limit,d50,d95,d99,peak_obs,peak_adm,ordering,duplicate,missing,template,failures
CLEAN = "wide-join,32,0,8619,7360,4000,3668,3896,2000,0,452,619,652,31,31,31,0,0,0,0,0"
SLOWER = "wide-join,32,1,9000,7400,4000,3700,3900,2000,0,500,700,800,31,31,31,0,0,0,0,0"
CHAIN = "chain,8,0,28196,26827,16000,3300,3400,2000,0,159,301,301,1,1,1,0,0,0,0,0"


def write(rows: list[str]) -> str:
    handle = tempfile.NamedTemporaryFile("w", suffix=".csv", delete=False, newline="")
    handle.write(HEADER + "\n")
    for row in rows:
        handle.write(row + "\n")
    handle.close()
    return handle.name


class ReadCsvTest(unittest.TestCase):
    def test_accepts_a_well_formed_row(self):
        rows = analyze.read_csv(write([CLEAN]))
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["shape"], "wide-join")
        self.assertEqual(rows[0]["peak_admitted"], 31)
        self.assertEqual(rows[0]["peak_running"], 31)

    def test_rejects_an_unexpected_header(self):
        handle = tempfile.NamedTemporaryFile("w", suffix=".csv", delete=False, newline="")
        handle.write("shape,stages\nwide-join,4\n")
        handle.close()
        with self.assertRaises(ValueError):
            analyze.read_csv(handle.name)

    def test_rejects_an_unknown_shape(self):
        with self.assertRaises(ValueError):
            analyze.read_csv(write([CLEAN.replace("wide-join", "spiral", 1)]))

    def test_rejects_out_of_order_percentiles(self):
        # dispatch p50 above p95 cannot come from a sorted sample.
        broken = CLEAN.replace(",452,619,", ",900,619,", 1)
        with self.assertRaises(ValueError):
            analyze.read_csv(write([broken]))

    def test_rejects_a_critical_path_longer_than_the_run(self):
        broken = CLEAN.replace("8619,7360", "7000,7360", 1)
        with self.assertRaises(ValueError):
            analyze.read_csv(write([broken]))

    def test_rejects_a_peak_above_the_stage_count(self):
        broken = CLEAN.replace(",31,31,31,", ",99,31,31,", 1)
        with self.assertRaises(ValueError):
            analyze.read_csv(write([broken]))

    def test_rejects_more_running_than_admitted(self):
        broken = CLEAN.replace(",31,31,31,", ",31,8,31,", 1)
        with self.assertRaises(ValueError):
            analyze.read_csv(write([broken]))

    def test_rejects_a_running_peak_above_the_concurrency_limit(self):
        broken = CLEAN.replace(",2000,0,452,", ",2000,8,452,", 1)
        with self.assertRaises(ValueError):
            analyze.read_csv(write([broken]))

    def test_rejects_an_empty_file(self):
        with self.assertRaises(ValueError):
            analyze.read_csv(write([]))


class AnalyzeTest(unittest.TestCase):
    def report(self, rows: list[str]) -> dict:
        report, _ = analyze.analyze([("suite", analyze.read_csv(write(rows)))])
        return report

    def test_groups_repetitions_of_one_case(self):
        report = self.report([CLEAN, SLOWER])
        self.assertEqual(len(report["cases"]), 1)
        case = report["cases"][0]
        self.assertEqual(case["repetitions"], 2)
        # Median of 452 and 500.
        self.assertEqual(case["dispatchP50UsMedian"], 476)
        self.assertEqual(case["dispatchP99UsMax"], 800)

    def test_separates_shapes_and_levels(self):
        report = self.report([CLEAN, CHAIN])
        self.assertEqual(
            [(case["shape"], case["stages"]) for case in report["cases"]],
            [("chain", 8), ("wide-join", 32)],
        )
        self.assertEqual(report["largestCase"], {"shape": "wide-join", "stages": 32})

    def test_scheduling_overhead_is_wall_less_critical_path(self):
        case = self.report([CLEAN])["cases"][0]
        self.assertEqual(case["schedulingOverheadUsMedian"], 8619 - 7360)

    def test_service_inflation_compares_service_to_the_model_delay(self):
        case = self.report([CLEAN])["cases"][0]
        # A 2000us delay observed as 3668us: the runtime stretched the call.
        self.assertAlmostEqual(case["serviceInflation"], 1.834, places=3)

    def test_a_clean_run_reports_no_defects(self):
        report = self.report([CLEAN, CHAIN])
        self.assertTrue(report["clean"])
        self.assertEqual(sum(report["defectsTotal"].values()), 0)
        self.assertEqual(report["stagesExecuted"], 40)

    def test_one_ordering_violation_makes_the_run_unclean(self):
        violated = CLEAN[::-1].replace("0", "1", 1)[::-1]  # last column: failures
        report = self.report([violated])
        self.assertFalse(report["clean"])
        self.assertEqual(report["defectsTotal"]["failures"], 1)

    def test_defects_are_summed_across_suites_and_reported_as_an_entry(self):
        rows = analyze.read_csv(write([CLEAN.replace(",0,0,0,0,0", ",2,0,1,0,0", 1)]))
        report, entries = analyze.analyze([("suite", rows)])
        self.assertEqual(report["defectsTotal"]["ordering_violations"], 2)
        self.assertEqual(report["defectsTotal"]["missing_stages"], 1)
        defects = next(entry for entry in entries if entry["name"] == "workflow defects")
        self.assertEqual(defects["value"], 3)
        self.assertEqual(json.loads(defects["extra"].split(";")[0])["ordering_violations"], 2)

    def test_the_concurrency_limit_separates_two_cases_of_one_shape(self):
        limited = CLEAN.replace(",2000,0,452,", ",2000,64,452,", 1)
        report = self.report([CLEAN, limited])
        self.assertEqual([case["limit"] for case in report["cases"]], [0, 64])

    def test_entries_carry_one_tracked_number_per_case_plus_defects(self):
        _, entries = analyze.analyze([("suite", analyze.read_csv(write([CLEAN, CHAIN])))])
        self.assertEqual(len(entries), 3)
        self.assertTrue(all(entry["unit"] in {"us", "count"} for entry in entries))


class MainTest(unittest.TestCase):
    def test_main_writes_both_documents_and_exits_zero_when_clean(self):
        csv_path = write([CLEAN, CHAIN])
        with tempfile.TemporaryDirectory() as out:
            argv = ["prog", f"suite={csv_path}", "--out", out]
            with unittest.mock.patch("sys.argv", argv), redirect_stdout(io.StringIO()):
                self.assertEqual(analyze.main(), 0)
            with open(os.path.join(out, "analysis.json")) as handle:
                self.assertTrue(json.load(handle)["clean"])
            with open(os.path.join(out, "summary.json")) as handle:
                self.assertEqual(len(json.load(handle)), 3)

    def test_main_exits_nonzero_on_a_defect(self):
        csv_path = write([CLEAN.replace(",0,0,0,0,0", ",0,0,0,1,0", 1)])
        with tempfile.TemporaryDirectory() as out:
            argv = ["prog", f"suite={csv_path}", "--out", out]
            with unittest.mock.patch("sys.argv", argv), redirect_stdout(io.StringIO()):
                self.assertEqual(analyze.main(), 1)


if __name__ == "__main__":
    unittest.main()
