#!/usr/bin/env bash
# Compatibility entry point; implementation lives with the kernel suite.
set -euo pipefail
exec "$(cd "$(dirname "$0")" && pwd)/kernel/run.sh" "$@"
