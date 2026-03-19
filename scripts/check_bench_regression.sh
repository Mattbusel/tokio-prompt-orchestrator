#!/usr/bin/env bash
# check_bench_regression.sh
#
# Runs `cargo bench` (libtest-harness benches) and compares against
# .bench_baseline.txt if it exists.  Fails with a non-zero exit code if any
# benchmark regresses by more than 15%.
#
# Usage:
#   ./scripts/check_bench_regression.sh [--save-baseline]
#
# Options:
#   --save-baseline   After running, write the results to .bench_baseline.txt
#                     (use this to update the baseline after intentional perf
#                     improvements).
#
# Expected output format (libtest bench):
#   test <name> ... bench: <ns>/iter (+/- <stddev>)
#
# Exit codes:
#   0 — all benchmarks within the 15% threshold (or no baseline exists)
#   1 — one or more benchmarks regressed by >15%

set -euo pipefail

BASELINE_FILE=".bench_baseline.txt"
RESULTS_FILE=$(mktemp /tmp/bench_results.XXXXXX.txt)
SAVE_BASELINE=false
REGRESSION_THRESHOLD=0.15   # 15%
REGRESSION_FOUND=false

for arg in "$@"; do
  case "$arg" in
    --save-baseline) SAVE_BASELINE=true ;;
  esac
done

echo "[bench] Running cargo bench..."
# Run all benches with the libtest harness output.
# Criterion writes its own HTML/JSON but also prints a summary line we can parse.
cargo bench 2>&1 | tee "$RESULTS_FILE"

# ── Parse results ──────────────────────────────────────────────────────────────
# Extract lines matching:  test <name> ... bench:   <number> ns/iter (+/- <number>)
parse_ns() {
  local file="$1"
  # Output: <name> <ns>
  grep -E '^test .+ \.\.\. bench:' "$file" \
    | sed -E 's/^test (.+) \.\.\. bench:[[:space:]]+([0-9_]+) ns\/iter.*$/\1 \2/' \
    | sed 's/_//g' \
    || true
}

# ── Compare against baseline ───────────────────────────────────────────────────
if [[ -f "$BASELINE_FILE" ]]; then
  echo ""
  echo "[bench] Comparing against baseline: $BASELINE_FILE"
  echo "-------------------------------------------------------"

  while IFS=' ' read -r bench_name current_ns; do
    # Look up this benchmark in the baseline
    baseline_ns=$(grep -E "^$bench_name " <(parse_ns "$BASELINE_FILE") | awk '{print $2}' || true)

    if [[ -z "$baseline_ns" ]]; then
      echo "  NEW   $bench_name — no baseline entry, skipping"
      continue
    fi

    if [[ "$baseline_ns" -eq 0 ]]; then
      echo "  SKIP  $bench_name — baseline is 0 ns"
      continue
    fi

    # Calculate percentage change: (current - baseline) / baseline
    # Use awk for floating-point arithmetic
    pct_change=$(awk "BEGIN { printf \"%.4f\", ($current_ns - $baseline_ns) / $baseline_ns }")
    pct_display=$(awk "BEGIN { printf \"%.1f\", ($current_ns - $baseline_ns) / $baseline_ns * 100 }")

    regressed=$(awk "BEGIN { print ($pct_change > $REGRESSION_THRESHOLD) ? \"yes\" : \"no\" }")

    if [[ "$regressed" == "yes" ]]; then
      echo "  FAIL  $bench_name: ${current_ns} ns vs baseline ${baseline_ns} ns (+${pct_display}%)"
      REGRESSION_FOUND=true
    else
      echo "  OK    $bench_name: ${current_ns} ns vs baseline ${baseline_ns} ns (${pct_display}%)"
    fi
  done < <(parse_ns "$RESULTS_FILE")

  echo "-------------------------------------------------------"
else
  echo "[bench] No baseline file found ($BASELINE_FILE) — skipping regression check."
  echo "[bench] Run with --save-baseline to create one."
fi

# ── Optionally save new baseline ───────────────────────────────────────────────
if [[ "$SAVE_BASELINE" == "true" ]]; then
  cp "$RESULTS_FILE" "$BASELINE_FILE"
  echo "[bench] Baseline saved to $BASELINE_FILE"
fi

rm -f "$RESULTS_FILE"

# ── Exit ───────────────────────────────────────────────────────────────────────
if [[ "$REGRESSION_FOUND" == "true" ]]; then
  echo ""
  echo "[bench] ERROR: One or more benchmarks regressed by more than 15%." >&2
  exit 1
fi

echo "[bench] All benchmarks within the 15% regression threshold."
exit 0
