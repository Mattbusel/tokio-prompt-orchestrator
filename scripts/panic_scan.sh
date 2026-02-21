#!/usr/bin/env bash
# panic_scan.sh
# Scans src/ for panic-inducing patterns outside #[cfg(test)] blocks.
# Exit code 0 = none found, 1 = panics found.
# Usage: ./scripts/panic_scan.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC_DIR="$REPO_ROOT/src"

# Patterns that indicate potential panics (PCRE — parentheses must be escaped)
PANIC_PATTERNS=(
    '\.unwrap\(\)'
    '\.expect\('
    'panic!\('
    'unreachable!\('
    'todo!\('
)

# Build single grep pattern
PATTERN=$(IFS='|'; echo "${PANIC_PATTERNS[*]}")

FOUND=0
declare -a HITS=()

# For each .rs file, find lines with panic patterns that are NOT in test blocks
while IFS= read -r -d '' rs_file; do
    rel="${rs_file#$REPO_ROOT/}"

    # Get test-block line numbers using awk.
    # Uses pending_test flag so that #[cfg(test)] sets pending state and
    # in_test only activates on the line that opens the first brace.
    test_lines=$(awk '
    BEGIN { in_test=0; pending_test=0; depth=0; test_depth=0 }
    {
        if (!in_test && !pending_test && /^[[:space:]]*#\[cfg\(test\)\]/) {
            pending_test = 1
        }
        opens = 0; closes = 0
        s = $0
        while (match(s, /\{/)) { opens++; s = substr(s, RSTART+1) }
        s = $0
        while (match(s, /\}/)) { closes++; s = substr(s, RSTART+1) }
        depth = depth + opens - closes
        if (pending_test && opens > 0) {
            in_test = 1
            pending_test = 0
            test_depth = depth
        }
        if (in_test) {
            print NR
            if (depth < test_depth) {
                in_test = 0
            }
        }
    }
    ' "$rs_file")

    # Find lines with panic patterns
    lineno=0
    while IFS= read -r raw_line; do
        ((lineno++)) || true

        # Check if this line matches a panic pattern
        if ! echo "$raw_line" | grep -qP "$PATTERN"; then
            continue
        fi

        # Skip if in test block
        if echo "$test_lines" | grep -qx "$lineno"; then
            continue
        fi

        # Skip comment-only lines (pattern in a comment is fine)
        trimmed="${raw_line#"${raw_line%%[![:space:]]*}"}"
        if [[ "$trimmed" == //* ]]; then
            continue
        fi

        HITS+=("${rel}:${lineno}: ${trimmed}")
        FOUND=1
    done < "$rs_file"
done < <(find "$SRC_DIR" -name "*.rs" -print0 | sort -z)

if [[ ${#HITS[@]} -gt 0 ]]; then
    echo "PANIC SCAN: Found production-path panics!"
    echo ""
    for hit in "${HITS[@]}"; do
        echo "  [PRODUCTION] $hit"
    done
    echo ""
    echo "Total: ${#HITS[@]} production panic(s) found."
    echo "Fix: convert to Result propagation before committing."
    exit 1
else
    echo "PANIC SCAN: No production panics found. ✓"
    exit 0
fi
