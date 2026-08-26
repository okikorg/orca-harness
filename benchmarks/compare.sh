#!/usr/bin/env bash
# Compatibility entry point; implementation lives with the comparison suite.
set -euo pipefail
exec "$(cd "$(dirname "$0")" && pwd)/compare/run.sh" "$@"
