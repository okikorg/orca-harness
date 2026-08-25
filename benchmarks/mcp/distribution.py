#!/usr/bin/env python3
"""Measure MCP metadata-search cutoff coverage from limit 10 through 100."""

from collections import Counter

from corpus import CASES, TOOLS, rows

LIMITS = range(10, 101, 10)


def report() -> str:
    measured = rows()
    rank_counts = Counter(rank if rank is not None else "no match" for _, rank, _ in measured)
    lines = [
        f"Corpus: {len(TOOLS)} tools, {len(CASES)} query-target cases",
        "",
        "Required result rank histogram",
    ]
    for bucket in (1, 2, 3, "no match"):
        count = rank_counts[bucket]
        lines.append(f"{str(bucket):>8} | {'█' * count} {count}")
    lines.extend(["", "Limit distribution", "limit | target coverage | avg results returned | histogram"])
    for limit in LIMITS:
        hits = sum(rank is not None and rank <= limit for _, rank, _ in measured)
        returned = sum(min(match_count, limit) for _, _, match_count in measured)
        coverage = 100 * hits / len(measured)
        average = returned / len(measured)
        lines.append(f"{limit:>5} | {coverage:>14.1f}% | {average:>20.2f} | {'█' * round(coverage / 5)}")
    misses = [case.query for case, rank, _ in measured if rank is None]
    miss_text = ", ".join(repr(query) for query in misses) or "none"
    lines.extend(["", f"Unmatched at every limit: {miss_text}"])
    return "\n".join(lines)


if __name__ == "__main__":
    print(report())
