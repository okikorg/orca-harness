#!/usr/bin/env python3
"""Report Rust source size by file and flag files over the LoC limit.

The audit intentionally measures physical lines for the limit, matching the
existing ``ci/check-source-size.sh`` check: a file may have at most 599 lines
(it must stay below 600).  Non-blank lines are included as a second useful
measure when reviewing a large file.

Exit status is 1 when any file is over the limit, so CI can run it as a gate;
``--top`` keeps the report to the largest files (every flagged file is always
listed) so the log shows what is approaching the limit without every file.

Usage:
    python3 scripts/audit_crates.py
    python3 scripts/audit_crates.py --top 25
    python3 scripts/audit_crates.py --root /path/to/orca-harness --limit 599
"""

from __future__ import annotations

import argparse
import sys
from dataclasses import dataclass
from pathlib import Path


# Same number as ci/check-source-size.sh: more than this many lines fails.
DEFAULT_LIMIT = 599


@dataclass(frozen=True)
class FileStats:
    path: Path
    total_lines: int
    non_blank_lines: int


def count_lines(path: Path) -> tuple[int, int]:
    """Return physical and non-blank line counts for a UTF-8 source file."""
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines()
    return len(lines), sum(bool(line.strip()) for line in lines)


def audit_crates(root: Path, limit: int) -> list[FileStats]:
    """Collect statistics for Rust files below ``root/crates``."""
    crates = root / "crates"
    if not crates.is_dir():
        raise FileNotFoundError(f"crates directory not found: {crates}")

    stats = []
    for path in sorted(crates.rglob("*.rs")):
        if not path.is_file():
            continue
        total, non_blank = count_lines(path)
        stats.append(FileStats(path.relative_to(root), total, non_blank))

    return stats


def print_report(
    root: Path, stats: list[FileStats], limit: int, top: int | None = None
) -> int:
    """Print the report and return the number of files above ``limit``.

    ``top`` limits the table to the ``top`` largest files; flagged files are
    never dropped, since they are the reason the report exists.
    """
    ordered = sorted(stats, key=lambda entry: (-entry.total_lines, str(entry.path)))
    flagged = sum(item.total_lines > limit for item in ordered)
    shown = ordered if top is None else ordered[: max(top, flagged)]

    print(f"Crate Rust files: {len(stats)}")
    print(f"Limit:            > {limit} physical lines")
    if len(shown) < len(ordered):
        print(f"Showing:          {len(shown)} largest of {len(ordered)}")
    print()
    print(f"{'Status':10} {'Lines':>7} {'Non-blank':>10}  File")
    print("-" * 72)

    for item in shown:
        status = "OVER_LIMIT" if item.total_lines > limit else "ok"
        print(
            f"{status:10} {item.total_lines:7} {item.non_blank_lines:10}  "
            f"{item.path}"
        )

    print()
    if flagged:
        print(f"Flagged {flagged} file(s) over {limit} lines.", file=sys.stderr)
    else:
        print(f"No files over {limit} lines.")
    return flagged


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
        help="repository root (default: inferred from this script)",
    )
    parser.add_argument(
        "--limit",
        type=int,
        default=DEFAULT_LIMIT,
        help=f"flag files with more than this many lines (default: {DEFAULT_LIMIT})",
    )
    parser.add_argument(
        "--top",
        type=int,
        default=None,
        help="list only the N largest files (flagged files are always listed)",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.limit < 0:
        print("error: --limit must be non-negative", file=sys.stderr)
        return 2
    if args.top is not None and args.top < 0:
        print("error: --top must be non-negative", file=sys.stderr)
        return 2
    try:
        stats = audit_crates(args.root, args.limit)
    except (FileNotFoundError, UnicodeDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    return 1 if print_report(args.root, stats, args.limit, args.top) else 0


if __name__ == "__main__":
    raise SystemExit(main())
