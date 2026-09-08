#!/usr/bin/env bash
# Workflow graph benchmark. Informational for latency and scale; the defect
# counts are deterministic and the analyzer fails the run on any of them.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SUITE_DIR="${REPO_ROOT}/benchmarks/workflow"
RESULTS_DIR="${REPO_ROOT}/benchmarks/results/workflow"
BIN="${REPO_ROOT}/target/release/examples/bench_workflow_graph"
MODE="${1:---standard}"
if [ "$#" -gt 1 ]; then
  echo "usage: $0 [--quick|--standard|--boundary]" >&2
  exit 2
fi
case "$MODE" in
  --quick|--standard|--boundary) ;;
  *)
    echo "usage: $0 [--quick|--standard|--boundary]" >&2
    exit 2
    ;;
esac

mkdir -p "$RESULTS_DIR"
rm -f "$RESULTS_DIR"/*.csv "$RESULTS_DIR"/*.txt "$RESULTS_DIR"/*.json

echo "Building workflow graph probe (release)..."
(cd "$REPO_ROOT" && cargo build --release -p orca-harness-tools --example bench_workflow_graph)

metadata() {
  (cd "$REPO_ROOT" && python3 - "$RESULTS_DIR/metadata.json" "$MODE" <<'PY'
import json, os, platform, subprocess, sys, time

def command(*args):
    return subprocess.check_output(args, text=True).strip()

out, mode = sys.argv[1:]
data = {
    "schemaVersion": 1,
    "timestampUtc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    "mode": mode,
    "gitRevision": command("git", "rev-parse", "HEAD"),
    "gitDirty": bool(command("git", "status", "--porcelain")),
    "rustc": command("rustc", "--version"),
    "cargo": command("cargo", "--version"),
    "system": platform.system(),
    "release": platform.release(),
    "machine": platform.machine(),
    "logicalCpus": os.cpu_count(),
    "memoryBytes": int(command("sysctl", "-n", "hw.memsize")) if platform.system() == "Darwin" else None,
    "dispatchOrigin": "last dependency's model return to this stage's model entry",
    "criticalPath": "longest chain of measured stage service times",
    "percentileConvention": "round((n - 1) * p) empirical order statistic",
    "concurrencyLimit": "unbounded (LIMIT=0), set per case",
}
with open(out, "w") as handle:
    json.dump(data, handle, indent=2)
PY
  )
}

run_case() {
  local name="$1" delay="$2" repetitions="$3" shapes="$4" levels="$5" limit="$6"
  echo "--- $name: delay=${delay}ms repetitions=${repetitions} shapes=${shapes} levels=${levels} limit=${limit} ---"
  if [ "$(uname -s)" = Darwin ]; then
    /usr/bin/time -l env DELAY_MS="$delay" REPETITIONS="$repetitions" SHAPES="$shapes" \
      LEVELS="$levels" LIMIT="$limit" \
      "$BIN" > "$RESULTS_DIR/$name.csv" 2> "$RESULTS_DIR/$name.txt"
  else
    /usr/bin/time -v env DELAY_MS="$delay" REPETITIONS="$repetitions" SHAPES="$shapes" \
      LEVELS="$levels" LIMIT="$limit" \
      "$BIN" > "$RESULTS_DIR/$name.csv" 2> "$RESULTS_DIR/$name.txt"
  fi
}

metadata
case "$MODE" in
  --quick)
    run_case quick 2 2 chain,diamond,wide-join,fanout,mesh 8,32 0
    csv_args=("quick=${RESULTS_DIR}/quick.csv")
    ;;
  --standard)
    # Shapes at a size where the runtime is not saturated: this is the set
    # whose wall time is an engine measurement rather than a machine one.
    run_case shapes 2 5 chain,diamond,wide-join,fanout,mesh 8,32,128 0
    # Scale: only the shapes whose stage count is the independent variable.
    run_case scale 2 3 wide-join,fanout,mesh 256,512,1024 0
    # The concurrency limit as an independent variable, at one fixed shape.
    run_case limited 2 3 wide-join 256 8
    csv_args=(
      "shapes=${RESULTS_DIR}/shapes.csv"
      "scale=${RESULTS_DIR}/scale.csv"
      "limited=${RESULTS_DIR}/limited.csv"
    )
    ;;
  --boundary)
    echo "warning: boundary mode admits thousands of concurrent stage agents"
    run_case boundary 2 3 wide-join,fanout 2048,4096 0
    run_case deep 2 3 chain 256,512 0
    csv_args=(
      "boundary=${RESULTS_DIR}/boundary.csv"
      "deep=${RESULTS_DIR}/deep.csv"
    )
    ;;
esac

python3 "$SUITE_DIR/analyze.py" "${csv_args[@]}" --out "$RESULTS_DIR"
echo "Results written to $RESULTS_DIR"
