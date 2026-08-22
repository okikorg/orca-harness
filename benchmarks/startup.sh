#!/usr/bin/env bash
#
# Startup latency benchmarks for orcacode.
#
# Measures wall-clock time for the cold-start path with hyperfine. Results
# are written to benchmarks/results/startup/ as JSON for CI consumption.
#
# Startup is not the metric orca-harness optimizes for — dispatch overhead
# is, and that lives in kernel.sh. This suite exists so the host binary's
# fixed cost stays visible: config load, skills discovery, session I/O and
# registry construction all run before a user can type anything.
#
# Usage:
#   ./benchmarks/startup.sh              # 100 runs
#   ./benchmarks/startup.sh --quick      # 20 runs
#   ./benchmarks/startup.sh --ci         # 100 runs, skip the build
#
# Requires: hyperfine (https://github.com/sharkdp/hyperfine), python3

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BENCH_DIR="${REPO_ROOT}/benchmarks"
ORCA_BIN="${ORCA_BIN:-${REPO_ROOT}/target/release/orcacode}"
RESULTS_DIR="${BENCH_DIR}/results/startup"
FIXTURE_ROOT="${TMPDIR:-/tmp}/orca-startup-benchmark-$$"

cleanup() {
  rm -rf "$FIXTURE_ROOT"
}
trap cleanup EXIT

if ! command -v hyperfine &>/dev/null; then
  echo "error: hyperfine is not installed"
  echo "       brew install hyperfine    (macOS)"
  echo "       apt install hyperfine     (Debian/Ubuntu)"
  echo "       cargo install hyperfine   (any)"
  exit 1
fi

RUNS=100
WARMUP=10
SKIP_BUILD=false
case "${1:-}" in
  --quick) RUNS=20;  WARMUP=3 ;;
  --ci)    RUNS=100; WARMUP=10; SKIP_BUILD=true ;;
esac

if [ "$SKIP_BUILD" = false ]; then
  echo "Building orcacode (release)..."
  (cd "$REPO_ROOT" && cargo build --release -p orcacode)
fi

if [ ! -x "$ORCA_BIN" ]; then
  echo "error: orcacode binary not found at $ORCA_BIN"
  exit 1
fi

# Stale JSON from an earlier shape of this suite would be summarized and
# budget-checked as if it were current.
mkdir -p "$RESULTS_DIR"
rm -f "${RESULTS_DIR}"/*.json

eval "$(python3 "${BENCH_DIR}/fixtures.py" --root "$FIXTURE_ROOT")"

if [ -x /usr/bin/true ]; then
  TRUE_BIN=/usr/bin/true
elif [ -x /bin/true ]; then
  TRUE_BIN=/bin/true
else
  TRUE_BIN=true
fi

# Everything the binary could otherwise pick up from the developer's own
# machine. ORCA_CONFIG_DIR wins over XDG_CONFIG_HOME and HOME in
# config_path(), but HOME is still read directly for the ~/.claude/skills
# compatibility roots, so all three are pinned. The API-key variables are
# unset so provider resolution cannot vary between machines.
bench_env=(
  env
  -u XDG_CONFIG_HOME
  -u OPENAI_API_KEY
  -u OPENROUTER_API_KEY
  -u FIRECRAWL_API_KEY
  -u ORCA_MODEL
  -u ORCA_BASE_URL
  -u ORCA_THEME
  -u ORCA_SUBAGENT_DEPTH
  "HOME=${FIXTURE_HOME}"
  "ORCA_CONFIG_DIR=${FIXTURE_CONFIG}"
)

# hyperfine runs with --shell=none, so each command is one string it splits
# itself; paths are quoted for that splitter, not for bash.
quoted() {
  printf "'%s'" "$1"
}

# run_bench <display name> <result file stem> <command string> [extra env...]
run_bench() {
  local name="$1" stem="$2" command="$3"
  shift 3
  echo "--- ${name} ---"
  "${bench_env[@]}" "$@" \
    hyperfine \
    --shell=none \
    --runs "$RUNS" \
    --warmup "$WARMUP" \
    ${PREPARE[@]+"${PREPARE[@]}"} \
    --export-json "${RESULTS_DIR}/${stem}.json" \
    --command-name "$name" \
    "$command"
  echo ""
}

echo "=== orcacode startup benchmarks ==="
echo "binary: $ORCA_BIN"
echo "runs:   $RUNS (warmup: $WARMUP)"
echo ""

PREPARE=()

# Baseline: the process-launch floor on this host. Reported for context;
# the budget checker still holds each command to its own wall clock.
run_bench "process baseline" baseline "$TRUE_BIN"

# Argument parsing only — the floor the binary itself imposes (image load,
# dynamic linking, tokio runtime construction) before any work happens.
run_bench "orcacode --help" help "$(quoted "$ORCA_BIN") --help"

# The cold-start path with recording off: config load, six skill roots
# scanned, system prompt built, tool registry and extensions constructed,
# agent assembled. ORCA_BENCH stops the binary right there, before the TUI.
run_bench "orcacode (startup)" startup \
  "$(quoted "$ORCA_BIN") --no-session --workspace $(quoted "$FIXTURE_WORKSPACE")" \
  ORCA_BENCH=1

# Session list plus a full transcript replayed off disk: read_dir over the
# fixture sessions, header parse for each, then every line of the newest
# deserialized back into Context. Resume appends nothing, so the fixture is
# identical at the end of the run.
run_bench "orcacode (resume)" resume \
  "$(quoted "$ORCA_BIN") --continue --workspace $(quoted "$FIXTURE_WORKSPACE")" \
  ORCA_BENCH=1

# Discovery cost with a populated .orca/skills: the delta against
# "orcacode (startup)" is what a workspace full of skills costs.
run_bench "orcacode (startup, skills)" startup-skills \
  "$(quoted "$ORCA_BIN") --no-session --workspace $(quoted "$FIXTURE_WORKSPACE_SKILLS")" \
  ORCA_BENCH=1

# The default path as a user actually gets it: a new session file created
# and its header written. Every run would otherwise leave one behind, so
# the directory is wiped before each — it is this workspace's own, never
# the one the resume benchmark reads.
PREPARE=(--prepare "rm -rf $(quoted "$FIXTURE_FRESH_SESSIONS")")
run_bench "orcacode (startup, new session)" session-create \
  "$(quoted "$ORCA_BIN") --workspace $(quoted "$FIXTURE_WORKSPACE_FRESH")" \
  ORCA_BENCH=1
PREPARE=()

echo "--- summary ---"
python3 "${BENCH_DIR}/summarize.py" --suite startup
python3 "${BENCH_DIR}/check_budgets.py" --suite startup
