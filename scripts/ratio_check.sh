#!/usr/bin/env bash
# ratio_check.sh
# Counts production vs test LOC and verifies >=1.5:1 ratio.
# Exit code 0 = pass, 1 = fail.
# Usage: ./scripts/ratio_check.sh [--verbose]

set -euo pipefail

VERBOSE=0
if [[ "${1:-}" == "--verbose" ]]; then
    VERBOSE=1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC_DIR="$REPO_ROOT/src"

# ── helpers ────────────────────────────────────────────────────────────────

# Count meaningful lines: non-blank, non-comment-only
count_meaningful() {
    local file="$1"
    grep -cP '^\s*[^/\s]|^\s*/\*' "$file" 2>/dev/null || echo 0
}

# Returns 0 (true) if a line number is inside a #[cfg(test)] block
# We use awk to track brace depth per file and emit line ranges.
#
# Fix: use a `pending_test` flag so that `#[cfg(test)]` sets pending state,
# and `in_test` only activates on the line that opens the first brace.
# Previously the script set test_depth=depth+1 on the attribute line (no
# braces), then immediately exited because depth < test_depth.
extract_test_ranges() {
    local file="$1"
    awk '
    BEGIN { in_test=0; pending_test=0; depth=0; test_depth=0 }
    {
        if (!in_test && !pending_test && /^[[:space:]]*#\[cfg\(test\)\]/) {
            pending_test = 1
        }
        opens = gsub(/{/, "{")
        closes = gsub(/}/, "}")
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
    ' "$file"
}

# Count production LOC in a .rs file (lines NOT in test blocks, meaningful)
count_prod_loc() {
    local file="$1"
    # Get set of test-block line numbers
    local test_lines
    test_lines=$(extract_test_ranges "$file")

    local total=0
    local lineno=0
    while IFS= read -r raw_line; do
        ((lineno++)) || true
        # Skip if in test block
        if echo "$test_lines" | grep -qx "$lineno"; then
            continue
        fi
        # Skip blank
        local trimmed="${raw_line#"${raw_line%%[![:space:]]*}"}"
        [[ -z "$trimmed" ]] && continue
        # Skip comment-only lines
        [[ "$trimmed" == //* ]] && continue
        ((total++)) || true
    done < "$file"
    echo "$total"
}

# Count test LOC in a .rs file (lines in #[cfg(test)] blocks, meaningful)
count_test_loc_in_file() {
    local file="$1"
    local test_lines
    test_lines=$(extract_test_ranges "$file")

    local total=0
    local lineno=0
    while IFS= read -r raw_line; do
        ((lineno++)) || true
        # Only process test-block lines
        if ! echo "$test_lines" | grep -qx "$lineno"; then
            continue
        fi
        local trimmed="${raw_line#"${raw_line%%[![:space:]]*}"}"
        [[ -z "$trimmed" ]] && continue
        [[ "$trimmed" == //* ]] && continue
        ((total++)) || true
    done < "$file"
    echo "$total"
}

# ── main ───────────────────────────────────────────────────────────────────

TOTAL_PROD=0
TOTAL_TEST=0
BELOW_THRESHOLD=()

declare -a FILE_RESULTS=()

while IFS= read -r -d '' rs_file; do
    rel="${rs_file#$REPO_ROOT/}"

    prod=$(count_prod_loc "$rs_file")
    test=$(count_test_loc_in_file "$rs_file")

    TOTAL_PROD=$((TOTAL_PROD + prod))
    TOTAL_TEST=$((TOTAL_TEST + test))

    if [[ $prod -gt 0 ]]; then
        # Compute ratio to 2dp using awk
        ratio=$(awk "BEGIN { printf \"%.2f\", $test / $prod }")
        is_below=$(awk "BEGIN { print ($test / $prod < 1.5) ? 1 : 0 }")
        if [[ "$is_below" == "1" ]]; then
            BELOW_THRESHOLD+=("$rel  —  ${ratio}:1  (prod=$prod, test=$test)")
        fi
        FILE_RESULTS+=("$rel | prod=$prod | test=$test | ratio=${ratio}:1")
    else
        FILE_RESULTS+=("$rel | prod=$prod | test=$test | ratio=N/A")
    fi
done < <(find "$SRC_DIR" -name "*.rs" -print0 | sort -z)

# Also count tests/ directory if it exists
TESTS_DIR="$REPO_ROOT/tests"
if [[ -d "$TESTS_DIR" ]]; then
    while IFS= read -r -d '' rs_file; do
        test=$(count_meaningful "$rs_file")
        TOTAL_TEST=$((TOTAL_TEST + test))
    done < <(find "$TESTS_DIR" -name "*.rs" -print0)
fi

# ── output ─────────────────────────────────────────────────────────────────

if [[ $VERBOSE -eq 1 ]]; then
    echo "Per-file breakdown:"
    echo "────────────────────────────────────────────────────────────────────"
    for line in "${FILE_RESULTS[@]}"; do
        echo "  $line"
    done
    echo ""
fi

OVERALL_RATIO=$(awk "BEGIN { printf \"%.2f\", $TOTAL_TEST / $TOTAL_PROD }")
IS_PASS=$(awk "BEGIN { print ($TOTAL_TEST / $TOTAL_PROD >= 1.5) ? 1 : 0 }")

echo "Production LOC (src/, excl. blanks/comments): $TOTAL_PROD"
echo "Test LOC (#[cfg(test)] + tests/, excl. blanks/comments): $TOTAL_TEST"
echo "Current ratio: ${OVERALL_RATIO}:1"

if [[ "$IS_PASS" == "1" ]]; then
    echo "Status: PASS (>=1.5:1)"
    EXIT_CODE=0
else
    echo "Status: FAIL (below 1.5:1)"
    EXIT_CODE=1
fi

if [[ ${#BELOW_THRESHOLD[@]} -gt 0 ]]; then
    echo ""
    echo "Modules below 1.5:1 individually:"
    for m in "${BELOW_THRESHOLD[@]}"; do
        echo "  $m"
    done
fi

exit $EXIT_CODE
