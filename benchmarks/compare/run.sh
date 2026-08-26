#!/usr/bin/env bash
#
# Head-to-head startup comparison: orcacode against fx
# (https://github.com/vercel-labs/fx), the other native-binary agent CLI
# this project measures itself against.
#
# READ THIS BEFORE QUOTING ANY NUMBER FROM IT.
#
# The obvious pairing is wrong. Both binaries have a benchmark env var,
# but they do not mean the same thing:
#
#   ORCA_BENCH=1 orcacode   runs the whole cold start — config, six skill
#                           roots, system prompt, tool and extension
#                           registries, agent build — then exits before the
#                           terminal UI.
#   FX_BENCH=1 fx           parses argv and exits. It never reads settings:
#                           point it at a corrupt settings.json and it
#                           still exits 0, while `fx status --json` reports
#                           malformed_settings. In fx's source this is
#                           `shouldRunBenchmarkNoArgRaw` in src/main.zig,
#                           which calls cli_surface.parse and exitFast(0).
#
# Pairing those two would compare orcacode's full startup against fx's
# argument parser and make fx look ~2.5x faster than the comparison
# supports. So this script groups commands by the work they actually do:
#
#   tier 1  argv parsed, no config read
#   tier 2  settings loaded, filesystem touched, then exit
#
# Even within a tier the work is not identical — fx's status probes auth
# and the workspace, orcacode's startup builds an agent — so treat this as
# "what each product's cold start costs", never as "this path is better
# engineered". Report the build mode of both binaries alongside any number.
#
# Session listing is deliberately left out: a fair version needs fx's own
# schema-v3 session fixture, and comparing against an empty directory would
# measure nothing.
#
# Usage:
#   ./benchmarks/compare/run.sh                     # whatever `fx` is on PATH
#   FX_BIN=/path/to/fx ./benchmarks/compare/run.sh  # a specific build
#   ./benchmarks/compare/run.sh --quick             # 20 runs instead of 100
#
# Requires: hyperfine, python3, an fx binary

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BENCH_DIR="${REPO_ROOT}/benchmarks"
ORCA_BIN="${ORCA_BIN:-${REPO_ROOT}/target/release/orcacode}"
FX_BIN="${FX_BIN:-$(command -v fx || true)}"
RESULTS_DIR="${BENCH_DIR}/results/compare"
FIXTURE_ROOT="${TMPDIR:-/tmp}/orca-compare-benchmark-$$"

cleanup() {
  rm -rf "$FIXTURE_ROOT"
}
trap cleanup EXIT

if ! command -v hyperfine &>/dev/null; then
  echo "error: hyperfine is not installed (brew install hyperfine)"
  exit 1
fi
if [ -z "$FX_BIN" ] || [ ! -x "$FX_BIN" ]; then
  echo "error: no fx binary found — set FX_BIN=/path/to/fx"
  exit 1
fi
if [ ! -x "$ORCA_BIN" ]; then
  echo "error: orcacode binary not found at $ORCA_BIN (cargo build --release -p orcacode)"
  exit 1
fi

RUNS=100
WARMUP=10
case "${1:-}" in
  --quick) RUNS=20; WARMUP=3 ;;
esac

mkdir -p "$RESULTS_DIR"
rm -f "${RESULTS_DIR}"/*.json

eval "$(python3 "${BENCH_DIR}/shared/fixtures.py" --root "$FIXTURE_ROOT" --skills 0)"

# fx's own benchmark fixture, from its benchmarks/startup.sh: a settings
# file at the permissions it expects, and nothing else.
FX_HOME="${FIXTURE_ROOT}/fx-home"
mkdir -p "${FX_HOME}/.fx"
chmod 700 "${FX_HOME}/.fx"
printf '%s\n' \
  '{"model":"openai/gpt-5.4","effort":"high","fast_mode":false,"startup_scrollback":true,"prompt_history":{"enabled":true},"statusLine":{"sandbox":true,"context":true}}' \
  > "${FX_HOME}/.fx/settings.json"
chmod 600 "${FX_HOME}/.fx/settings.json"

if [ -x /usr/bin/true ]; then
  TRUE_BIN=/usr/bin/true
else
  TRUE_BIN=/bin/true
fi

# Both binaries get the same treatment: a private HOME, no XDG override, no
# API keys. Both run in the same empty workspace so any cwd scanning sees
# identical contents.
common_env=(
  env
  -u XDG_CONFIG_HOME
  -u OPENAI_API_KEY
  -u OPENROUTER_API_KEY
  -u ANTHROPIC_API_KEY
  -u AI_GATEWAY_API_KEY
  -u FIRECRAWL_API_KEY
)

quoted() {
  printf "'%s'" "$1"
}

# bench <display name> <result stem> <command string> [extra env...]
bench() {
  local name="$1" stem="$2" command="$3"
  shift 3
  echo "--- ${name} ---"
  (
    cd "$FIXTURE_WORKSPACE"
    "${common_env[@]}" "$@" \
      hyperfine \
      --shell=none \
      --runs "$RUNS" \
      --warmup "$WARMUP" \
      --export-json "${RESULTS_DIR}/${stem}.json" \
      --command-name "$name" \
      "$command"
  )
  echo ""
}

fx_env=("HOME=${FX_HOME}")
orca_env=("HOME=${FIXTURE_HOME}" "ORCA_CONFIG_DIR=${FIXTURE_CONFIG}")

echo "=== orcacode vs fx: cold start ==="
echo "orcacode: $ORCA_BIN"
echo "fx:       $FX_BIN"
echo "runs:     $RUNS (warmup: $WARMUP)"
echo ""

bench "process baseline" 0-baseline "$TRUE_BIN"

# Tier 1: argv parsed, nothing read.
bench "fx (arg parse)" 1-fx-parse "$(quoted "$FX_BIN")" "${fx_env[@]}" FX_BENCH=1
bench "orcacode --help" 2-orca-help "$(quoted "$ORCA_BIN") --help" "${orca_env[@]}"

# Tier 2: settings loaded, filesystem touched, exit.
bench "fx status --json" 3-fx-status "$(quoted "$FX_BIN") status --json" "${fx_env[@]}"
bench "fx doctor --json" 4-fx-doctor "$(quoted "$FX_BIN") doctor --json" "${fx_env[@]}"
bench "orcacode (startup)" 5-orca-startup \
  "$(quoted "$ORCA_BIN") --no-session --workspace $(quoted "$FIXTURE_WORKSPACE")" \
  "${orca_env[@]}" ORCA_BENCH=1

echo "--- binary size ---"
python3 - "$ORCA_BIN" "$FX_BIN" <<'PY'
import os
import sys

for path in sys.argv[1:]:
    print(f"  {os.path.basename(path):<12} {os.path.getsize(path) / 1e6:>7.1f} MB  {path}")
print("\n  Optimization level is not detectable from the file — state it yourself.")
PY
echo ""

echo "--- summary ---"
python3 "${BENCH_DIR}/shared/summarize.py" --dir "$RESULTS_DIR"
python3 "${BENCH_DIR}/compare/report.py" --dir "$RESULTS_DIR"
