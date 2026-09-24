#!/usr/bin/env bash
set -euo pipefail

binary="${1:-target/release/orcacode}"
# Static musl builds carry their own libc and allocator, about 2 MB more
# than the macOS binary; 0.6.2 shipped at 8.62 MB.
case "$binary" in
  *-linux-musl/*) limit=9000000 ;;
  *) limit=6700000 ;;
esac

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
