import unittest

import pareto


class McpSearchParetoTest(unittest.TestCase):
    def test_frontier_contains_only_limits_that_buy_coverage(self):
        points = pareto.curve()
        efficient = pareto.frontier(points)

        self.assertEqual([point.limit for point in efficient], [1, 2, 3])
        self.assertEqual(
            [point.coverage for point in efficient],
            [33 / 38, 35 / 38, 1.0],
        )
        self.assertEqual(
            [point.average_results for point in efficient],
            [1.0, 52 / 38, 65 / 38],
        )

    def test_knee_is_cheapest_maximum_coverage_point(self):
        selected = pareto.knee(pareto.curve())
        self.assertEqual(selected.limit, 3)
        self.assertEqual(selected.coverage, 1.0)
        self.assertEqual(selected.average_results, 65 / 38)

    def test_higher_limits_are_dominated_by_limit_three(self):
        points = pareto.curve()
        limit_three = points[2]
        self.assertTrue(
            all(pareto.dominates(limit_three, point) for point in points[3:])
        )


if __name__ == "__main__":
    unittest.main()
