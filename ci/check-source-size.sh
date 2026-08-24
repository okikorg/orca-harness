#!/bin/sh
set -eu

limit=599
failed=0

while IFS= read -r file; do
    lines=$(wc -l < "$file" | tr -d ' ')
    if [ "$lines" -gt "$limit" ]; then
        printf '%s: %s lines (maximum %s; files must stay below 600)\n' "$file" "$lines" "$limit" >&2
        failed=1
    fi
done <<EOF
$(find crates -type f \( -name '*.rs' -o -name '*.inc' -o -name '*.part' \) -print | LC_ALL=C sort)
EOF

exit "$failed"
