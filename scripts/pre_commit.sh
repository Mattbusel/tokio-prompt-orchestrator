#!/usr/bin/env bash
# pre_commit.sh
# Full pre-commit gate — runs all checks in sequence.
# Exit code 0 = all pass, 1 = at least one failed.
# Usage: ./scripts/pre_commit.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

PASS=1
FAILED_CHECK=""

run_check() {
    local name="$1"
    shift
    echo "── Running: $name ──────────────────────────────────────────"
    if "$@"; then
        echo "✓ $name passed"
        echo ""
    else
        echo "✗ $name FAILED"
        PASS=0
        FAILED_CHECK="$name"
        return 1
    fi
}

# 1. Formatting
run_check "cargo fmt --check" cargo fmt --check --all || {
    echo ""
    echo "Run 'cargo fmt --all' to fix formatting."
    echo "GATE FAILED at: cargo fmt --check"
    exit 1
}

# 2. Clippy (all features, deny warnings)
run_check "cargo clippy" cargo clippy --all-features -- -D warnings || {
    echo ""
    echo "GATE FAILED at: cargo clippy"
    exit 1
}

# 3. Tests
run_check "cargo test" cargo test --all-features || {
    echo ""
    echo "GATE FAILED at: cargo test"
    exit 1
}

# 4. Panic scan
run_check "panic_scan.sh" ./scripts/panic_scan.sh || {
    echo ""
    echo "GATE FAILED at: panic_scan.sh"
    echo "Zero panics in production code required before commit."
    exit 1
}

# 5. Ratio check
run_check "ratio_check.sh" ./scripts/ratio_check.sh || {
    echo ""
    echo "GATE FAILED at: ratio_check.sh"
    echo "Test ratio must be >=1.5:1 before commit."
    exit 1
}

echo "════════════════════════════════════════════════════════════"
echo "✓ All checks passed — safe to commit"
echo "════════════════════════════════════════════════════════════"
exit 0
