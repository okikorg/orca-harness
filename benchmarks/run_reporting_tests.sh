#!/usr/bin/env bash
#
# Run every benchmark reporting test: the parsers, analyzers and the budget
# gate that turn probe output into a CI verdict. Pure python3, no build.
#
# Each suite directory is discovered on its own so a suite's tests import
# their own siblings (report.py, corpus.py, ...) without a shared package.
#
# Usage:
#   ./benchmarks/run_reporting_tests.sh

set -euo pipefail

BENCH_DIR="$(cd "$(dirname "$0")" && pwd)"

failed=0
for suite in "$BENCH_DIR"/*/; do
  suite="${suite%/}"
  if ! compgen -G "$suite/*_test.py" > /dev/null; then
    continue
  fi
  echo "=== $(basename "$suite") ==="
  if ! python3 -m unittest discover -s "$suite" -p '*_test.py' -v; then
    failed=1
  fi
  echo ""
done

if [ "$failed" -ne 0 ]; then
  echo "benchmark reporting tests failed" >&2
fi
exit "$failed"
