#!/usr/bin/env bash
#
# Kernel overhead benchmarks — the numbers that actually matter here.
#
# The harness is judged on what it adds between a model emitting tool calls
# and those tools doing work, not on how fast the binary starts. This runs
# the dispatch probes and turns their output into machine-readable results
# under benchmarks/results/kernel/.
#
#   fanout_probe      no-op tools: pure dispatch and fan-out overhead
#   tool_fanout_perf  the real write_file / read_file / shell tools through
#                     the same dispatcher, so overhead is measured against
#                     genuine tool latency
#
# Usage:
#   ./benchmarks/kernel.sh              # probes only
#   ./benchmarks/kernel.sh --quick      # fewer iterations, skip real tools
#   ./benchmarks/kernel.sh --criterion  # also run and record cargo bench
#   ./benchmarks/kernel.sh --ci         # skip the build, skip real tools
#
# Requires: python3

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH_DIR="${REPO_ROOT}/benchmarks"
RESULTS_DIR="${BENCH_DIR}/results/kernel"

# Every gated metric is a p99, so the iteration count decides whether that
# is a percentile or just the second-worst sample. A probe iteration costs
# microseconds; CI runs the most of them precisely because a shared runner
# is where scheduler noise lands.
ITERS=2000
RUN_TOOLS=true
RUN_CRITERION=false
SKIP_BUILD=false
case "${1:-}" in
  --quick)     ITERS=200;  RUN_TOOLS=false ;;
  --criterion) RUN_CRITERION=true ;;
  --ci)        ITERS=5000; RUN_TOOLS=false; SKIP_BUILD=true ;;
esac

mkdir -p "$RESULTS_DIR"
rm -f "${RESULTS_DIR}"/*.txt "${RESULTS_DIR}"/summary.json

cd "$REPO_ROOT"

if [ "$SKIP_BUILD" = false ]; then
  echo "Building probes (release)..."
  cargo build --release -p orca-harness-core --example fanout_probe
  if [ "$RUN_TOOLS" = true ]; then
    cargo build --release -p orca-harness-tools --example tool_fanout_perf
  fi
fi

echo "=== orca-harness kernel benchmarks ==="
echo "iterations: $ITERS"
echo ""

# Batch sizes: 1 is the single-call floor (no cross-thread handoff at all),
# 10 and 100 are the fan-out shapes the README publishes.
for n in 1 10 100; do
  echo "--- fanout_probe n=${n} ---"
  cargo run --release -q -p orca-harness-core --example fanout_probe -- "$n" "$ITERS" \
    | tee "${RESULTS_DIR}/fanout-$(printf '%03d' "$n").txt"
  echo ""
done

if [ "$RUN_TOOLS" = true ]; then
  echo "--- tool_fanout_perf (real tools) ---"
  cargo run --release -q -p orca-harness-tools --example tool_fanout_perf \
    | tee "${RESULTS_DIR}/tools.txt"
  echo ""
fi

REPORT_ARGS=()
if [ "$RUN_CRITERION" = true ]; then
  echo "--- cargo bench ---"
  cargo bench --workspace
  REPORT_ARGS+=(--criterion)
  echo ""
fi

echo "--- summary ---"
python3 "${BENCH_DIR}/kernel_report.py" ${REPORT_ARGS[@]+"${REPORT_ARGS[@]}"}
python3 "${BENCH_DIR}/check_budgets.py" --suite kernel
