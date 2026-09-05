#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
RESULTS_DIR="${REPO_ROOT}/benchmarks/results/core-tools"
SAMPLES=20

case "${1:-}" in
  "") ;;
  --quick) SAMPLES=3 ;;
  *) echo "usage: $0 [--quick]" >&2; exit 2 ;;
esac

mkdir -p "$RESULTS_DIR"
if [[ -f "${RESULTS_DIR}/measurements.json" && ! -f "${RESULTS_DIR}/baseline.json" ]]; then
  cp "${RESULTS_DIR}/measurements.json" "${RESULTS_DIR}/baseline.json"
fi
rm -f "${RESULTS_DIR}/accuracy.txt" "${RESULTS_DIR}/raw.txt" \
  "${RESULTS_DIR}/measurements.json" "${RESULTS_DIR}/summary.json" \
  "${RESULTS_DIR}/comparison.json"

cd "$REPO_ROOT"

echo "=== core tools accuracy ==="
cargo test -p orca-harness-tools --test core_tools 2>&1 \
  | tee "${RESULTS_DIR}/accuracy.txt"

echo
echo "=== core tools release benchmark ==="
echo "samples: $SAMPLES"
for sample in $(seq 1 "$SAMPLES"); do
  echo "--- sample ${sample}/${SAMPLES} ---" | tee -a "${RESULTS_DIR}/raw.txt"
  cargo test --release -q -p orca-harness-tools --test core_tools_perf \
    -- --nocapture --test-threads=1 2>&1 | tee -a "${RESULTS_DIR}/raw.txt"
done

python3 "${REPO_ROOT}/benchmarks/core-tools/report.py" \
  --samples "$SAMPLES" \
  --git-sha "$(git rev-parse HEAD)" \
  --worktree "$(if git diff --quiet && git diff --cached --quiet; then echo clean; else echo dirty; fi)"

echo
echo "=== core tools budgets ==="
python3 "${REPO_ROOT}/benchmarks/shared/check_budgets.py" --suite core-tools
