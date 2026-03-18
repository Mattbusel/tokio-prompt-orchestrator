#!/usr/bin/env bash
# ratio_check.sh
#
# Checks the test-to-code line ratio across the project.
#
# Production code  = src/ .rs files, excluding:
#   - src/bin/         (standalone binaries)
#   - #[cfg(test)] blocks inside any .rs file
#   - blank lines and comment-only lines (// …)
#
# Test code = #[cfg(test)] blocks inside src/ .rs files
#           + all .rs files in tests/
#           (blank lines and comment-only lines excluded)
#
# Required ratio: test LOC / prod LOC >= 1.5
#
# Exit code 0 = ratio is acceptable (>= 1.5)
# Exit code 1 = ratio is below threshold
#
# Usage: ./scripts/ratio_check.sh [--verbose]

set -euo pipefail

VERBOSE=0
if [[ "${1:-}" == "--verbose" ]]; then
    VERBOSE=1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC_DIR="$REPO_ROOT/src"
BIN_DIR="$REPO_ROOT/src/bin"
TESTS_DIR="$REPO_ROOT/tests"

# ── awk helper: emit line numbers inside #[cfg(test)] blocks ────────────────
#
# Uses a pending_test flag so that the attribute line itself does not affect
# depth counting — only the first opening brace after the attribute activates
# in_test mode.  This correctly handles:
#
#   #[cfg(test)]
#   mod tests {        <- depth goes 1 here; test_depth = 1
#       ...
#   }                  <- depth drops to 0 < test_depth=1 -> exit
#
extract_test_ranges() {
    local file="$1"
    awk '
    BEGIN { in_test=0; pending_test=0; pending_cfg=0; depth=0; test_depth=0 }
    {
        # Single-line form: #[cfg(test)]
        if (!in_test && !pending_test && /^[[:space:]]*#\[cfg\(test\)\]/) {
            pending_test = 1
        }
        # Multi-line opening: #[cfg(  (closing paren on later line)
        if (!in_test && !pending_test && !pending_cfg \
            && /^[[:space:]]*#\[cfg\(/ && !/test\)/) {
            pending_cfg = 1
        }
        # Inner line of multi-line form that just says "test"
        if (!in_test && !pending_test && pending_cfg \
            && /^[[:space:]]*test[[:space:]]*$/) {
            pending_cfg  = 0
            pending_test = 1
        }
        # Cancel pending_cfg if the line does not look like a continuation
        if (pending_cfg \
            && !/^[[:space:]]*#\[cfg\(/ \
            && !/^[[:space:]]*test[[:space:]]*$/ \
            && !/^[[:space:]]*\)\]/) {
            pending_cfg = 0
        }

        opens  = gsub(/{/, "{")
        closes = gsub(/}/, "}")
        depth  = depth + opens - closes

        if (pending_test && opens > 0) {
            in_test      = 1
            pending_test = 0
            test_depth   = depth
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

# ── count meaningful (non-blank, non-comment) lines in a file ───────────────
# Optionally restricted to a set of line numbers passed on stdin.
count_meaningful_lines() {
    local file="$1"
    local -a allowed_lines=("${@:2}")  # optional list; empty = all lines

    local total=0
    local lineno=0

    while IFS= read -r raw_line; do
        ((lineno++)) || true

        # If a whitelist of lines was provided, skip lines not in it
        if [[ ${#allowed_lines[@]} -gt 0 ]]; then
            local in_list=0
            for ln in "${allowed_lines[@]}"; do
                if [[ "$ln" == "$lineno" ]]; then
                    in_list=1
                    break
                fi
            done
            [[ $in_list -eq 0 ]] && continue
        fi

        local trimmed="${raw_line#"${raw_line%%[![:space:]]*}"}"
        [[ -z "$trimmed" ]] && continue
        [[ "$trimmed" == //* ]] && continue
        ((total++)) || true
    done < "$file"

    echo "$total"
}

# ── count prod + test LOC for a single .rs file ─────────────────────────────
count_file() {
    local file="$1"

    # Collect test-block line numbers as an array
    local -a tlines=()
    while IFS= read -r ln; do
        tlines+=("$ln")
    done < <(extract_test_ranges "$file")

    local prod=0
    local test_loc=0
    local lineno=0

    while IFS= read -r raw_line; do
        ((lineno++)) || true

        local trimmed="${raw_line#"${raw_line%%[![:space:]]*}"}"
        [[ -z "$trimmed" ]] && continue
        [[ "$trimmed" == //* ]] && continue

        # Check if this line is in a test block
        local in_test=0
        for ln in "${tlines[@]}"; do
            if [[ "$ln" == "$lineno" ]]; then
                in_test=1
                break
            fi
        done

        if [[ $in_test -eq 1 ]]; then
            ((test_loc++)) || true
        else
            ((prod++)) || true
        fi
    done < "$file"

    echo "$prod $test_loc"
}

# ── main ─────────────────────────────────────────────────────────────────────

TOTAL_PROD=0
TOTAL_TEST=0

declare -a FILE_RESULTS=()
declare -a BELOW=()

while IFS= read -r -d '' rs_file; do
    # Exclude src/bin/ from production counts — binaries are not library code
    if [[ "$rs_file" == "$BIN_DIR"/* ]]; then
        continue
    fi

    rel="${rs_file#$REPO_ROOT/}"

    read -r prod test_loc < <(count_file "$rs_file")

    TOTAL_PROD=$((TOTAL_PROD + prod))
    TOTAL_TEST=$((TOTAL_TEST + test_loc))

    if [[ $prod -gt 0 ]]; then
        ratio=$(awk "BEGIN { printf \"%.2f\", $test_loc / $prod }")
        is_below=$(awk "BEGIN { print ($test_loc / $prod < 1.5) ? \"yes\" : \"no\" }")
        FILE_RESULTS+=("$(printf '  %-60s prod=%-5d test=%-5d ratio=%s:1' "$rel" "$prod" "$test_loc" "$ratio")")
        if [[ "$is_below" == "yes" ]]; then
            BELOW+=("$rel  (prod=$prod, test=$test_loc, ratio=${ratio}:1)")
        fi
    else
        FILE_RESULTS+=("$(printf '  %-60s prod=%-5d test=%-5d ratio=N/A' "$rel" "$prod" "$test_loc")")
    fi
done < <(find "$SRC_DIR" -name "*.rs" -print0 | sort -z)

# Count all .rs files in tests/ as pure test LOC
if [[ -d "$TESTS_DIR" ]]; then
    while IFS= read -r -d '' rs_file; do
        # count_file would work here too; use simpler grep since all lines are test code
        t=$(grep -cP '^\s*[^/\s]|^\s*/\*' "$rs_file" 2>/dev/null || true)
        TOTAL_TEST=$((TOTAL_TEST + t))
    done < <(find "$TESTS_DIR" -name "*.rs" -print0)
fi

# ── output ───────────────────────────────────────────────────────────────────

if [[ $VERBOSE -eq 1 ]] && [[ ${#FILE_RESULTS[@]} -gt 0 ]]; then
    echo "=========================================="
    echo " PER-FILE BREAKDOWN"
    echo "=========================================="
    for line in "${FILE_RESULTS[@]}"; do
        echo "$line"
    done
    echo ""
fi

OVERALL_RATIO=$(awk "BEGIN { if ($TOTAL_PROD > 0) printf \"%.2f\", $TOTAL_TEST / $TOTAL_PROD; else print \"N/A\" }")
IS_PASS=$(awk "BEGIN { print ($TOTAL_PROD > 0 && $TOTAL_TEST / $TOTAL_PROD >= 1.5) ? \"yes\" : \"no\" }")

echo "=========================================="
echo " TEST:CODE RATIO CHECK"
echo "=========================================="
echo " Production LOC  (src/, excl. src/bin/, blanks, comments): $TOTAL_PROD"
echo " Test LOC        (#[cfg(test)] blocks + tests/ directory):  $TOTAL_TEST"
echo " Required ratio  : 1.50:1"
echo " Current ratio   : ${OVERALL_RATIO}:1"
echo "=========================================="

if [[ "$IS_PASS" == "yes" ]]; then
    echo " Status: PASS"
    echo "=========================================="
    EXIT_CODE=0
else
    echo " Status: FAIL  — ratio is below 1.5:1"
    echo " Add more tests to bring coverage up to the required threshold."
    echo "=========================================="
    EXIT_CODE=1
fi

if [[ ${#BELOW[@]} -gt 0 ]]; then
    echo ""
    echo " Modules individually below 1.5:1:"
    for m in "${BELOW[@]}"; do
        echo "  $m"
    done
    echo ""
fi

exit $EXIT_CODE
