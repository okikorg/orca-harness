#!/usr/bin/env python3
"""Analyze MCP metadata-search accuracy by query quality."""

from collections import Counter
from dataclasses import dataclass

from corpus import CASES, TOOLS, rows


@dataclass(frozen=True)
class Metrics:
    cases: int
    recall: float
    top1: float
    mrr: float
    precision: float
    average_results: float
    ambiguity: float


def metrics(measured) -> Metrics:
    count = len(measured)
    return Metrics(
        cases=count,
        recall=sum(rank is not None for _, rank, _ in measured) / count,
        top1=sum(rank == 1 for _, rank, _ in measured) / count,
        mrr=sum(0 if rank is None else 1 / rank for _, rank, _ in measured) / count,
        # One labeled relevant tool per case: precision is 1/result_count when
        # the target is present, otherwise zero.
        precision=sum(
            0 if rank is None else 1 / result_count
            for _, rank, result_count in measured
        )
        / count,
        average_results=sum(result_count for _, _, result_count in measured) / count,
        ambiguity=sum(result_count > 1 for _, _, result_count in measured) / count,
    )


def grouped_metrics():
    measured = rows()
    qualities = dict.fromkeys(case.quality for case in CASES)
    return {
        quality: metrics([row for row in measured if row[0].quality == quality])
        for quality in qualities
    }


def percent(value: float) -> str:
    return f"{100 * value:.1f}%"


def report() -> str:
    measured = rows()
    rank_counts = Counter(rank if rank is not None else "miss" for _, rank, _ in measured)
    lines = [
        f"Corpus: {len(TOOLS)} tools, {len(CASES)} labeled queries",
        "",
        "Accuracy by query quality",
        "quality        cases  recall   top-1     MRR  precision  avg results  ambiguous",
    ]
    for quality, score in grouped_metrics().items():
        lines.append(
            f"{quality:<14} {score.cases:>5}  {percent(score.recall):>6}  "
            f"{percent(score.top1):>6}  {score.mrr:>6.3f}  "
            f"{percent(score.precision):>9}  {score.average_results:>11.2f}  "
            f"{percent(score.ambiguity):>9}"
        )
    overall = metrics(measured)
    lines.append(
        f"{'overall':<14} {overall.cases:>5}  {percent(overall.recall):>6}  "
        f"{percent(overall.top1):>6}  {overall.mrr:>6.3f}  "
        f"{percent(overall.precision):>9}  {overall.average_results:>11.2f}  "
        f"{percent(overall.ambiguity):>9}"
    )
    lines.extend(["", "Result-rank histogram"])
    for bucket in (1, 2, 3, "miss"):
        count = rank_counts[bucket]
        lines.append(f"{str(bucket):>5} | {'█' * count} {count}")

    lines.extend(["", "Problem queries"])
    problems = [
        row for row in measured if row[1] is None or row[1] > 1 or row[2] > 1
    ]
    for case, rank, result_count in problems:
        shown_rank = "miss" if rank is None else str(rank)
        lines.append(
            f"- [{case.quality}] {case.query!r}: target={case.target}, "
            f"rank={shown_rank}, results={result_count}"
        )
    return "\n".join(lines)


if __name__ == "__main__":
    print(report())
