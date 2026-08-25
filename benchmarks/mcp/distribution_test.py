import unittest

import distribution
from corpus import TOOLS, rows


class McpSearchDistributionTest(unittest.TestCase):
    def test_current_live_snapshot_favors_the_smallest_tested_limit(self):
        measured = rows()
        self.assertEqual(len(TOOLS), 18)
        self.assertEqual(len(measured), 38)

        coverage = {}
        average_results = {}
        for limit in distribution.LIMITS:
            hits = sum(rank is not None and rank <= limit for _, rank, _ in measured)
            coverage[limit] = hits / len(measured)
            average_results[limit] = sum(
                min(match_count, limit) for _, _, match_count in measured
            ) / len(measured)

        self.assertEqual(coverage[10], 1.0)
        self.assertTrue(all(value == coverage[10] for value in coverage.values()))
        self.assertLess(average_results[10], average_results[20])
        self.assertTrue(
            all(value == average_results[20] for limit, value in average_results.items() if limit >= 20)
        )
        self.assertEqual(min(distribution.LIMITS), 10)

    def test_rank_histogram_is_stable(self):
        ranks = [rank for _, rank, _ in rows()]
        self.assertEqual(ranks.count(1), 33)
        self.assertEqual(ranks.count(2), 2)
        self.assertEqual(ranks.count(3), 3)
        self.assertEqual(ranks.count(None), 0)


if __name__ == "__main__":
    unittest.main()
