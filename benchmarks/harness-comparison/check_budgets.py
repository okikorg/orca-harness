#!/usr/bin/env python3
"""Gate a harness-comparison run against the ceilings orcacode must hold.

The suite bench.yml runs is deterministic and offline. This one is not: it
spends real money against a live gateway, so it runs on its own schedule
rather than on pull requests, and its budgets are set the way
benchmarks/shared/check_budgets.py sets its own -- far enough above a quiet
run to survive gateway variance, close enough to catch a cliff.

What each ceiling is for:

  answers        a correctness collapse, not the one flaky task; the
                 observed range is 43-45 of 48
  cost/success   the single most stable scalar here, because it divides out
                 the run-to-run spread in how many turns a task takes
  total tokens   an exploration blowup: a prompt change that sends the model
                 wandering shows up here first
  turns          the same blowup seen from the other side
  e2e TTFT       a blocking call landing in front of the loop; the
                 deterministic startup suite gates cold start at 12ms, and
                 this is the end-to-end backstop for anything that suite
                 cannot see. Gateway latency moves it, so the ceiling is
                 loose: it caught a 4.2s startup regression at 5150ms
                 against an observed range of 698-966ms.
  cache share    a prompt prefix that stopped being byte-stable, which
                 silently reprices cached reads as fresh input

Budgets apply to orcacode. Other harnesses in the run are reported for
comparison and never gated -- their regressions are not ours to fix.
"""

import argparse
import json
import os
import sys

HARNESS = "orca"

MINIMA = {
    "answer_correct": 40,
    "cache_read_share": 0.10,
}

MAXIMA = {
    "normalized_cost_per_success_usd": 0.015,
    "total_tokens": 550_000,
    "turns": 210,
    "timeouts": 2,
    "ttft_median_ms": 2500.0,
}


def check(summary):
    orca = summary["harnesses"][HARNESS]
    failures = []
    for key, floor in MINIMA.items():
        value = orca.get(key)
        if value is None:
            failures.append(f"{key}: missing from the summary")
        elif value < floor:
            failures.append(f"{key}: {value} below the floor of {floor}")
    for key, ceiling in MAXIMA.items():
        value = orca.get(key)
        if value is None:
            failures.append(f"{key}: missing from the summary")
        elif value > ceiling:
            failures.append(f"{key}: {value} over the budget of {ceiling}")
    return failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run_dir", help="a directory written by run.py and analyzed")
    args = parser.parse_args()

    path = os.path.join(args.run_dir, "summary.json")
    with open(path, encoding="utf-8") as handle:
        summary = json.load(handle)

    failures = check(summary)
    orca = summary["harnesses"][HARNESS]
    print(f"harness-comparison budgets for {HARNESS} ({args.run_dir})")
    for key in list(MINIMA) + list(MAXIMA):
        print(f"  {key}: {orca.get(key)}")
    if failures:
        print("\nover budget:")
        for failure in failures:
            print(f"  {failure}")
        return 1
    print("\nall budgets held")
    return 0


if __name__ == "__main__":
    sys.exit(main())
