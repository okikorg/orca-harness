#!/usr/bin/env bash
# Compatibility entry point; implementation lives with the startup suite.
set -euo pipefail
exec "$(cd "$(dirname "$0")" && pwd)/startup/run.sh" "$@"
