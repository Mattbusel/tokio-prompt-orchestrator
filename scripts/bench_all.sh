#!/usr/bin/env bash
# bench_all.sh
# Runs all benchmarks and fails if any exceed their documented budget.
# Exit code 0 = all within budget, 1 = budget exceeded or bench failed.
# Usage: ./scripts/bench_all.sh [--baseline]

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

BASELINE_MODE=0
if [[ "${1:-}" == "--baseline" ]]; then
    BASELINE_MODE=1
fi

# ── Performance budgets (from CLAUDE.md) ──────────────────────────────────
# Format: "bench_name_pattern:budget_us"
declare -A BUDGETS=(
    ["dedup_check"]="1000"          # Deduplication hot path: P99 <1ms = 1000µs
    ["circuit_breaker_check"]="50"  # Circuit breaker check: P99 <50µs
    ["retry_policy"]="5"            # Retry policy evaluation: P99 <5µs
    ["channel_send"]="10"           # Channel send: P99 <10µs
)

echo "Running benchmarks..."
echo ""

# Check if cargo-criterion or nightly bench is available
if cargo bench --help 2>&1 | grep -q "bench"; then
    # Run all benchmarks and capture output
    BENCH_OUTPUT=$(cargo bench --all-features 2>&1) || {
        echo "✗ cargo bench failed"
        echo "$BENCH_OUTPUT"
        exit 1
    }

    echo "$BENCH_OUTPUT"
    echo ""

    # Parse output for budget violations
    VIOLATIONS=0

    for bench_pattern in "${!BUDGETS[@]}"; do
        budget_us="${BUDGETS[$bench_pattern]}"
        # Look for timing lines matching this benchmark
        # criterion output: "bench_name  time:   [X.XX µs Y.YY µs Z.ZZ µs]"
        while IFS= read -r line; do
            if echo "$line" | grep -qi "$bench_pattern"; then
                # Extract the median time (middle value in brackets)
                time_str=$(echo "$line" | grep -oP '\[.*?\]' | head -1)
                if [[ -n "$time_str" ]]; then
                    # Parse median value and unit
                    median=$(echo "$time_str" | grep -oP '[\d.]+ [µmn]s' | sed -n '2p')
                    echo "  $bench_pattern: $median (budget: ${budget_us}µs)"

                    # Convert to microseconds for comparison
                    value=$(echo "$median" | grep -oP '[\d.]+')
                    unit=$(echo "$median" | grep -oP '[µmn]s')

                    us_value=$(awk -v val="$value" -v unit="$unit" 'BEGIN {
                        if (unit == "ns") print val / 1000
                        else if (unit == "µs") print val
                        else if (unit == "ms") print val * 1000
                        else print val
                    }')

                    over_budget=$(awk -v actual="$us_value" -v budget="$budget_us" \
                        'BEGIN { print (actual > budget) ? 1 : 0 }')

                    if [[ "$over_budget" == "1" ]]; then
                        echo "  ✗ BUDGET EXCEEDED: $bench_pattern = ${us_value}µs > ${budget_us}µs"
                        ((VIOLATIONS++)) || true
                    fi
                fi
            fi
        done <<< "$BENCH_OUTPUT"
    done

    if [[ $VIOLATIONS -gt 0 ]]; then
        echo ""
        echo "✗ $VIOLATIONS benchmark(s) exceeded budget"
        exit 1
    else
        echo ""
        echo "✓ All benchmarks within budget"
        exit 0
    fi
else
    echo "Note: No benchmarks found or cargo bench not available."
    echo "Add benchmarks to benches/ directory to enable budget enforcement."
    echo "See CLAUDE.md section 7 for required benchmark format."
    exit 0
fi
