#!/usr/bin/env bash
# coverage.sh
# Generates HTML coverage report using cargo-llvm-cov (preferred) or cargo-tarpaulin.
# Exit code 0 = report generated, 1 = failed.
# Usage: ./scripts/coverage.sh [--open]

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

OPEN_BROWSER=0
if [[ "${1:-}" == "--open" ]]; then
    OPEN_BROWSER=1
fi

REPORT_DIR="$REPO_ROOT/target/coverage"
mkdir -p "$REPORT_DIR"

echo "Generating coverage report..."
echo ""

# Try cargo-llvm-cov first (preferred — faster, more accurate)
if command -v cargo-llvm-cov &>/dev/null || cargo llvm-cov --version &>/dev/null 2>&1; then
    echo "Using cargo-llvm-cov..."
    cargo llvm-cov \
        --all-features \
        --html \
        --output-dir "$REPORT_DIR" \
        2>&1

    REPORT_FILE="$REPORT_DIR/index.html"
    echo ""
    echo "✓ Coverage report generated: $REPORT_FILE"

    # Print summary to stdout
    cargo llvm-cov \
        --all-features \
        --summary-only \
        2>&1 || true

elif command -v cargo-tarpaulin &>/dev/null || cargo tarpaulin --version &>/dev/null 2>&1; then
    echo "Using cargo-tarpaulin..."
    cargo tarpaulin \
        --all-features \
        --out Html \
        --output-dir "$REPORT_DIR" \
        --timeout 300 \
        2>&1

    REPORT_FILE="$REPORT_DIR/tarpaulin-report.html"
    echo ""
    echo "✓ Coverage report generated: $REPORT_FILE"

else
    echo "✗ No coverage tool found."
    echo ""
    echo "Install one of:"
    echo "  cargo install cargo-llvm-cov    (recommended)"
    echo "  cargo install cargo-tarpaulin   (alternative)"
    echo ""
    echo "For cargo-llvm-cov, also install the LLVM tools component:"
    echo "  rustup component add llvm-tools-preview"
    exit 1
fi

# Open in browser if requested
if [[ $OPEN_BROWSER -eq 1 && -f "${REPORT_FILE:-}" ]]; then
    echo ""
    echo "Opening report in browser..."
    case "$(uname -s)" in
        Darwin) open "$REPORT_FILE" ;;
        Linux)  xdg-open "$REPORT_FILE" ;;
        CYGWIN*|MINGW*|MSYS*) start "$REPORT_FILE" ;;
        *) echo "Cannot auto-open on this OS. Open manually: $REPORT_FILE" ;;
    esac
fi

exit 0
