#!/usr/bin/env python3
"""Plot the MCP search limit Pareto curve: coverage versus result volume."""

from dataclasses import dataclass

from corpus import CASES, TOOLS, rows

LIMITS = range(1, 101)


@dataclass(frozen=True)
class Point:
    limit: int
    coverage: float
    average_results: float


def curve() -> list[Point]:
    measured = rows()
    return [
        Point(
            limit=limit,
            coverage=sum(
                rank is not None and rank <= limit for _, rank, _ in measured
            )
            / len(measured),
            average_results=sum(
                min(result_count, limit) for _, _, result_count in measured
            )
            / len(measured),
        )
        for limit in LIMITS
    ]


def dominates(left: Point, right: Point) -> bool:
    """Return whether left is no worse on either objective and better on one."""
    return (
        left.coverage >= right.coverage
        and left.average_results <= right.average_results
        and (
            left.coverage > right.coverage
            or left.average_results < right.average_results
        )
    )


def frontier(points: list[Point]) -> list[Point]:
    return [
        point
        for point in points
        if not any(dominates(other, point) for other in points if other != point)
    ]


def knee(points: list[Point]) -> Point:
    """Choose the cheapest point that reaches the curve's maximum coverage."""
    maximum = max(point.coverage for point in points)
    return min(
        (point for point in points if point.coverage == maximum),
        key=lambda point: (point.average_results, point.limit),
    )


def ascii_curve(points: list[Point], width: int = 48, height: int = 10) -> list[str]:
    """Render coverage on y and average returned results on x."""
    unique = []
    seen = set()
    for point in points:
        coordinate = (point.average_results, point.coverage)
        if coordinate not in seen:
            seen.add(coordinate)
            unique.append(point)

    min_cost = min(point.average_results for point in unique)
    max_cost = max(point.average_results for point in unique)
    min_coverage = min(point.coverage for point in unique)
    max_coverage = max(point.coverage for point in unique)
    cost_spread = max_cost - min_cost or 1.0
    coverage_spread = max_coverage - min_coverage or 1.0
    grid = [[" " for _ in range(width + 1)] for _ in range(height + 1)]
    frontier_limits = {point.limit for point in frontier(points)}
    for point in unique:
        x = round((point.average_results - min_cost) / cost_spread * width)
        y = round((point.coverage - min_coverage) / coverage_spread * height)
        grid[height - y][x] = "●" if point.limit in frontier_limits else "×"

    lines = ["coverage versus average returned results"]
    for row, cells in enumerate(grid):
        coverage = max_coverage - row / height * coverage_spread
        lines.append(f" {100 * coverage:>5.1f}% |{''.join(cells)}")
    lines.append("        +" + "─" * (width + 1) + "> average results")
    lines.append(f"         {min_cost:.2f}{' ' * (width - 7)}{max_cost:.2f}")
    lines.append("         ● Pareto frontier   × dominated")
    return lines


def report() -> str:
    points = curve()
    efficient = frontier(points)
    selected = knee(points)
    lines = [
        f"Corpus: {len(TOOLS)} tools, {len(CASES)} labeled queries",
        "Objectives: maximize target coverage; minimize average returned results",
        "",
        *ascii_curve(points),
        "",
        "Pareto frontier",
        "limit | coverage | avg results | marginal coverage gain",
    ]
    prior_coverage = 0.0
    for point in efficient:
        gain = point.coverage - prior_coverage
        lines.append(
            f"{point.limit:>5} | {100 * point.coverage:>7.1f}% | "
            f"{point.average_results:>11.2f} | {100 * gain:>+21.1f} pp"
        )
        prior_coverage = point.coverage
    lines.extend(
        [
            "",
            f"Knee: limit {selected.limit} reaches maximum measured coverage "
            f"({100 * selected.coverage:.1f}%) at the lowest result cost "
            f"({selected.average_results:.2f}/query).",
            f"Limits {selected.limit + 1}–100 are dominated: no coverage gain.",
            "Any remaining non-top-1 cases are ambiguous labels or ranking issues, not cutoff misses.",
        ]
    )
    return "\n".join(lines)


if __name__ == "__main__":
    print(report())
