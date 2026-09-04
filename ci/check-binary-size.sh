#!/usr/bin/env bash
set -euo pipefail

binary="${1:-target/release/orcacode}"
limit=8500000

if [[ ! -f "$binary" ]]; then
  echo "missing release binary: $binary" >&2
  exit 1
fi

size=$(wc -c < "$binary" | tr -d ' ')
echo "$binary: $size bytes (limit: <$limit bytes)"
if (( size >= limit )); then
  echo "release binary exceeds the total size limit" >&2
  exit 1
fi
