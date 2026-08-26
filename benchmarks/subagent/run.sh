#!/usr/bin/env bash
# Fake-subagent concurrency benchmark. Informational: no CI budget.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SUITE_DIR="${REPO_ROOT}/benchmarks/subagent"
RESULTS_DIR="${REPO_ROOT}/benchmarks/results/subagent"
BIN="${REPO_ROOT}/target/release/examples/bench_subagent_concurrency"
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

echo "Building fake-subagent probe (release)..."
(cd "$REPO_ROOT" && cargo build --release -p orca-harness-tools --example bench_subagent_concurrency)

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
    "latencyOrigin": "spawned task first poll",
    "percentileConvention": "round((n - 1) * p) empirical order statistic",
}
with open(out, "w") as handle:
    json.dump(data, handle, indent=2)
PY
  )
}

run_case() {
  local name="$1" delay="$2" repetitions="$3" levels="$4"
  echo "--- $name: delay=${delay}ms repetitions=${repetitions} levels=${levels} ---"
  if [ "$(uname -s)" = Darwin ]; then
    /usr/bin/time -l env DELAY_MS="$delay" REPETITIONS="$repetitions" LEVELS="$levels" \
      "$BIN" > "$RESULTS_DIR/$name.csv" 2> "$RESULTS_DIR/$name.txt"
  else
    /usr/bin/time -v env DELAY_MS="$delay" REPETITIONS="$repetitions" LEVELS="$levels" \
      "$BIN" > "$RESULTS_DIR/$name.csv" 2> "$RESULTS_DIR/$name.txt"
  fi
}

metadata
case "$MODE" in
  --quick)
    run_case quick-10ms 10 3 64,256,1024
    csv_args=("quick=${RESULTS_DIR}/quick-10ms.csv")
    ;;
  --standard)
    run_case sweep-1ms 1 5 1,2,4,8,16,32,64,128,256,512,1024,2048,4096,8192,16384
    run_case hold-100ms 100 5 1024,2048,4096,8192,16384,32768,65536
    csv_args=("sweep=${RESULTS_DIR}/sweep-1ms.csv" "hold=${RESULTS_DIR}/hold-100ms.csv")
    ;;
  --boundary)
    echo "warning: boundary mode may use more than 5 GiB of memory"
    run_case boundary-500ms 500 3 65536,131072
    csv_args=("boundary=${RESULTS_DIR}/boundary-500ms.csv")
    ;;
esac

python3 "$SUITE_DIR/analyze.py" "${csv_args[@]}" --out "$RESULTS_DIR"
echo "Results written to $RESULTS_DIR"
