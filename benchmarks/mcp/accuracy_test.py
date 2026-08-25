import unittest

import accuracy


class McpSearchAccuracyTest(unittest.TestCase):
    def test_accuracy_by_query_quality_is_stable(self):
        scores = accuracy.grouped_metrics()

        deliberate = scores["deliberate"]
        self.assertEqual(deliberate.cases, 18)
        self.assertEqual(deliberate.recall, 1.0)
        self.assertEqual(deliberate.top1, 1.0)
        self.assertEqual(deliberate.mrr, 1.0)
        self.assertAlmostEqual(deliberate.precision, 35 / 36)

        short = scores["short"]
        self.assertEqual(short.cases, 16)
        self.assertEqual(short.recall, 1.0)
        self.assertAlmostEqual(short.top1, 15 / 16)
        self.assertAlmostEqual(short.mrr, 31 / 32)

        underspecified = scores["underspecified"]
        self.assertEqual(underspecified.cases, 4)
        self.assertEqual(underspecified.recall, 1.0)
        self.assertEqual(underspecified.top1, 0.0)

    def test_overall_metrics_include_plural_recall_and_rank_improvements(self):
        score = accuracy.metrics(accuracy.rows())
        self.assertEqual(score.recall, 1.0)
        self.assertAlmostEqual(score.top1, 33 / 38)
        self.assertAlmostEqual(score.mrr, 35 / 38)
        self.assertAlmostEqual(score.precision, 0.7466905901116427)
        self.assertAlmostEqual(score.average_results, 2.0)
        self.assertAlmostEqual(score.ambiguity, 14 / 38)


if __name__ == "__main__":
    unittest.main()
