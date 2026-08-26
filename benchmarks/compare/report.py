#!/usr/bin/env python3
"""Group the comparison runs by the work they do and print the deltas.

Raw wall clock includes the host's process-launch cost, which belongs to
neither binary. Subtracting the `/usr/bin/true` baseline measured in the
same session leaves the work each program actually does — the only figure
that survives being quoted somewhere else.

Commands are grouped into tiers rather than paired one-to-one, because
`FX_BENCH=1 fx` and `ORCA_BENCH=1 orcacode` do not measure the same thing.
See the header of compare/run.sh; the note is repeated in the output so a
pasted table carries its own caveat.
"""

import argparse
import glob
import json
import os
import sys

BENCH_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

BASELINE = "process baseline"

TIERS = [
    (
        "argv parsed, no config read",
        [
            ("fx (arg parse)", "parses argv, exits; reads nothing"),
            ("orcacode --help", "parses argv, formats and writes usage"),
        ],
    ),
    (
        "settings loaded, filesystem touched, exit",
        [
            ("fx status --json", "settings, auth, workspace probe, JSON out"),
            ("fx doctor --json", "settings plus system checks"),
            ("orcacode (startup)", "config, 6 skill roots, prompt, registries, agent"),
        ],
    ),
]


def load(results_dir: str) -> dict[str, dict]:
    out = {}
    for path in sorted(glob.glob(os.path.join(results_dir, "*.json"))):
        if os.path.basename(path) == "summary.json":
            continue
        with open(path) as handle:
            result = json.load(handle)["results"][0]
        out[result["command"]] = result
    return out


def ms(seconds: float) -> str:
    return f"{seconds * 1000:.2f}ms"


def report(results_dir: str) -> int:
    results = load(results_dir)
    if BASELINE not in results:
        print(f"error: no {BASELINE!r} result in {results_dir}", file=sys.stderr)
        return 1
    floor = results[BASELINE]["mean"]

    print(f"Work above the {ms(floor)} process-launch floor on this host.\n")
    for tier, commands in TIERS:
        print(f"  {tier}")
        for name, what in commands:
            if name not in results:
                print(f"    {name:<22} {'(not measured)':>10}")
                continue
            work = results[name]["mean"] - floor
            print(f"    {name:<22} {ms(work):>10}   {what}")
        print()

    print(
        "Tiers group commands by the work they do, and even inside a tier the\n"
        "work is not identical.\n\n"
        "  - `FX_BENCH=1 fx` parses argv and exits without reading settings, so\n"
        "    it is NOT the counterpart to ORCA_BENCH. That pairing would compare\n"
        "    orcacode's whole startup against fx's argument parser.\n"
        "  - fx has no command that exits after a full interactive-launch\n"
        "    startup, so tier 2 substitutes status/doctor, which probe auth and\n"
        "    the system — work orcacode's startup does not do. fx's own budget\n"
        "    file evaluates them on Linux only; on macOS they cost far more, and\n"
        "    most of it is in-process CPU rather than I/O. Tier 2 is a data\n"
        "    point about those commands, not a verdict about startup.\n\n"
        "Quote build modes and platform with any of these numbers."
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dir", default=os.path.join(BENCH_DIR, "results", "compare"))
    args = parser.parse_args()
    return report(args.dir)


if __name__ == "__main__":
    sys.exit(main())
