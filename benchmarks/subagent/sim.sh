#!/usr/bin/env bash
# Realistic background-subagent capacity simulation. Informational: no CI budget.
#
#   ./benchmarks/subagent/sim.sh              # background 1..64, raw 128..512
#   ./benchmarks/subagent/sim.sh --quick      # scaled-down model timing, small levels
#   ./benchmarks/subagent/sim.sh --ceiling    # raw 1024, 2048, 4096: find the harness knee
#   ./benchmarks/subagent/sim.sh --provider N # simulate a provider allowing N concurrent streams
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
RESULTS_DIR="${REPO_ROOT}/benchmarks/results/subagent"
BIN="${REPO_ROOT}/target/release/examples/sim_background_subagents"
MODE="${1:---standard}"

mkdir -p "$RESULTS_DIR"
echo "Building realistic simulation (release)..."
(cd "$REPO_ROOT" && cargo build --release -p orca-harness-tools --example sim_background_subagents)

run_case() {
  local name="$1"
  shift
  echo "--- $name ($*) ---"
  echo "fd limit: $(ulimit -n) · process limit: $(ulimit -u)" > "$RESULTS_DIR/$name.txt"
  if [ "$(uname -s)" = Darwin ]; then
    /usr/bin/time -l env "$@" "$BIN" > "$RESULTS_DIR/$name.csv" 2>> "$RESULTS_DIR/$name.txt"
  else
    /usr/bin/time -v env "$@" "$BIN" > "$RESULTS_DIR/$name.csv" 2>> "$RESULTS_DIR/$name.txt"
  fi
  grep -E "^(mode |background |raw |knee|no knee|provider admission)" "$RESULTS_DIR/$name.txt"
}

case "$MODE" in
  --quick)
    run_case sim-quick SIM_SCALE=0.2 SIM_LEVELS=1,4,16 SIM_RAW_LEVELS=64 SIM_REPEAT=1
    ;;
  --standard)
    run_case sim-realistic SIM_SCALE=1.0
    ;;
  --ceiling)
    run_case sim-raw-high SIM_SCALE=1.0 SIM_LEVELS=1 SIM_REPEAT=2 SIM_RAW_LEVELS=1024,2048,4096
    ;;
  --provider)
    cap="${2:?usage: $0 --provider <concurrent streams>}"
    run_case "sim-provider-cap${cap}" SIM_SCALE=1.0 SIM_PROVIDER_CAP="$cap" SIM_MODEL_CONCURRENCY="${SIM_MODEL_CONCURRENCY:-$cap}" \
      SIM_LEVELS=8,16,24,32,64 SIM_RAW_LEVELS= SIM_REPEAT=1
    ;;
  *)
    echo "usage: $0 [--quick|--standard|--ceiling|--provider N]" >&2
    exit 2
    ;;
esac
echo "Results written to $RESULTS_DIR"
