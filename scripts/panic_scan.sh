#!/usr/bin/env bash
# panic_scan.sh
#
# Scans src/ (excluding src/bin/ and #[cfg(test)] blocks) for panic-inducing
# patterns:
#   - .unwrap()
#   - .expect(
#   - panic!()
#   - todo!()
#   - unreachable!()
#
# Reports counts per file and a grand total.
# Exits with code 1 if any unwrap() or expect() are found in production code.
# Exits with code 0 if none are found.
#
# Usage: ./scripts/panic_scan.sh [--verbose]

set -euo pipefail

VERBOSE=0
if [[ "${1:-}" == "--verbose" ]]; then
    VERBOSE=1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC_DIR="$REPO_ROOT/src"
BIN_DIR="$REPO_ROOT/src/bin"

# Patterns that may induce panics (Perl-compatible regex alternation)
PATTERN='\.unwrap\(\)|\.expect\(|panic!\(|todo!\(|unreachable!\('

# Stricter patterns that MUST NOT appear in production code (triggers exit 1)
STRICT_PATTERN='\.unwrap\(\)|\.expect\('

# ── awk helper: emit line numbers that live inside #[cfg(test)] blocks ──────
test_block_lines() {
    local file="$1"
    awk '
    BEGIN { in_test=0; pending_test=0; depth=0; test_depth=0 }
    {
        # Single-line form: #[cfg(test)]
        if (!in_test && !pending_test && /^[[:space:]]*#\[cfg\(test\)\]/) {
            pending_test = 1
        }
        opens = gsub(/{/, "{")
        closes = gsub(/}/, "}")
        depth = depth + opens - closes
        if (pending_test && opens > 0) {
            in_test    = 1
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

# ── main scan ───────────────────────────────────────────────────────────────

TOTAL_HITS=0
STRICT_HITS=0

# Associative map: file -> count
declare -A FILE_COUNT=()
declare -A FILE_STRICT_COUNT=()

while IFS= read -r -d '' rs_file; do
    # Skip src/bin/ — those are standalone binaries, not library code
    if [[ "$rs_file" == "$BIN_DIR"/* ]]; then
        continue
    fi

    rel="${rs_file#$REPO_ROOT/}"

    # Collect test-block line numbers for this file
    test_lines=$(test_block_lines "$rs_file")

    file_hits=0
    file_strict=0
    lineno=0

    while IFS= read -r raw_line; do
        ((lineno++)) || true

        # Quick pre-filter: skip if no relevant keyword present
        if ! printf '%s' "$raw_line" | grep -qP "$PATTERN"; then
            continue
        fi

        # Skip lines inside #[cfg(test)] blocks
        if printf '%s' "$test_lines" | grep -qx "$lineno"; then
            continue
        fi

        # Skip comment-only lines (// …)
        trimmed="${raw_line#"${raw_line%%[![:space:]]*}"}"
        if [[ "$trimmed" == //* ]]; then
            continue
        fi

        ((file_hits++)) || true
        TOTAL_HITS=$((TOTAL_HITS + 1))

        # Check for strict violations (unwrap / expect)
        if printf '%s' "$raw_line" | grep -qP "$STRICT_PATTERN"; then
            ((file_strict++)) || true
            STRICT_HITS=$((STRICT_HITS + 1))
        fi

        if [[ $VERBOSE -eq 1 ]]; then
            echo "  [PANIC] ${rel}:${lineno}: ${trimmed}"
        fi
    done < "$rs_file"

    if [[ $file_hits -gt 0 ]]; then
        FILE_COUNT["$rel"]=$file_hits
        FILE_STRICT_COUNT["$rel"]=$file_strict
    fi
done < <(find "$SRC_DIR" -name "*.rs" -print0 | sort -z)

# ── summary ─────────────────────────────────────────────────────────────────

echo "=========================================="
echo " PANIC SCAN REPORT"
echo "=========================================="
echo " Scanned: $SRC_DIR"
echo " Excluded: src/bin/  (standalone binaries)"
echo " Excluded: #[cfg(test)] blocks"
echo "=========================================="

if [[ ${#FILE_COUNT[@]} -eq 0 ]]; then
    echo " No panic-inducing patterns found."
    echo " Status: PASS"
    echo "=========================================="
    exit 0
fi

echo " Counts per file:"
echo "------------------------------------------"

# Sort keys for deterministic output
while IFS= read -r rel; do
    count=${FILE_COUNT[$rel]}
    strict=${FILE_STRICT_COUNT[$rel]:-0}
    if [[ $strict -gt 0 ]]; then
        echo "  $rel  ->  $count hit(s)  [$strict strict: unwrap/expect]"
    else
        echo "  $rel  ->  $count hit(s)"
    fi
done < <(printf '%s\n' "${!FILE_COUNT[@]}" | sort)

echo "------------------------------------------"
echo " Total panic hits   : $TOTAL_HITS"
echo " Strict (unwrap/expect): $STRICT_HITS"
echo "=========================================="

if [[ $STRICT_HITS -gt 0 ]]; then
    echo " Status: FAIL  — unwrap()/expect() found in production code."
    echo " Fix: replace with proper Result/Option propagation (? operator)."
    echo "=========================================="
    exit 1
else
    echo " Status: WARN  — only todo!/unreachable!/panic! found (no unwrap/expect)."
    echo " Consider removing placeholders before release."
    echo "=========================================="
    exit 0
fi
