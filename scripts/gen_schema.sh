#!/usr/bin/env bash
# gen_schema.sh — Regenerate config.schema.json from the Rust type definitions.
#
# Run this script whenever a public config struct in src/config/mod.rs changes
# so that config.schema.json stays in sync with the code.
#
# Usage:
#   ./scripts/gen_schema.sh
#
# Requirements:
#   - Rust toolchain (cargo) in PATH
#   - Run from the repository root
#
# CI integration example (GitHub Actions):
#
#   - name: Regenerate JSON Schema
#     run: ./scripts/gen_schema.sh
#
#   - name: Fail if schema is out of date
#     run: git diff --exit-code config.schema.json

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

echo "Building gen_schema binary..."
cargo build --bin gen_schema --features schema

echo "Generating config.schema.json..."
cargo run --bin gen_schema --features schema

echo ""
echo "Done. config.schema.json has been updated."
echo "Commit it alongside any config struct changes so the schema stays in sync."
