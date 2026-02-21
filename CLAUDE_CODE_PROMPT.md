# Claude Code Kickoff Prompt
## Paste this verbatim into Claude Code to begin Phase 0

---

You are working on `tokio-prompt-orchestrator`, a production-grade, multi-stage LLM pipeline orchestrator written in Rust. This codebase is being developed to aerospace reliability standards.

Before writing a single line of code, read the following files in this exact order:

1. `CLAUDE.md` — your governing rules, never violated
2. `AGENTS.md` — your coordination protocol
3. `ROADMAP.md` — the phased plan; you are starting Phase 0

After reading all three, confirm you have read them by outputting:
```
PROTOCOL ACKNOWLEDGED
- Ratio target: ≥1.5:1 (test lines : production lines)
- Panic policy: zero unwrap/expect in src/ outside test blocks
- Phase: 0 — Hardening
- First task: Panic audit and script infrastructure
```

Then begin Phase 0 in this exact order:

---

## TASK 0.1 — Panic Audit

Scan every file in `src/` for calls to `.unwrap()`, `.expect()`, `panic!()`, `unreachable!()`, and `todo!()`. 

For each one found:
- Print its file path, line number, and the surrounding context (5 lines)
- Categorize it: `[TEST_BLOCK]` (acceptable) or `[PRODUCTION]` (must fix)
- Count the total production-path panics found

Output a summary table:
```
File                        | Line | Type       | Context
src/enhanced/dedup.rs       | 142  | PRODUCTION | self.map.get(&key).unwrap()
src/pipeline/stages/rag.rs  | 89   | PRODUCTION | result.expect("rag failed")
...

Total production panics found: N
```

Do NOT fix them yet — just audit and report.

---

## TASK 0.2 — Ratio Baseline

Count lines of code in `src/` (production) and in `#[cfg(test)]` blocks plus `tests/` directory (test code). Exclude blank lines and comment-only lines from both counts.

Output:
```
Production LOC (src/, excl. blanks/comments): N
Test LOC (#[cfg(test)] + tests/, excl. blanks/comments): N
Current ratio: X.XX:1
Status: PASS (≥1.5:1) / FAIL (below 1.5:1)

Modules below 1.5:1 individually:
  src/enhanced/circuit_breaker.rs  — 1.2:1
  src/web/rest.rs                  — 0.9:1
  ...
```

---

## TASK 0.3 — Script Infrastructure

Create the following shell scripts in `scripts/`. Each must be executable (`chmod +x`) and work from the repository root.

### scripts/ratio_check.sh
```bash
#!/bin/bash
# Counts production vs test LOC and verifies ≥1.5:1 ratio
# Exit code 0 = pass, 1 = fail
# Usage: ./scripts/ratio_check.sh [--verbose]
```

Requirements:
- Count `src/` files (exclude blank lines and comment-only lines)
- Count `#[cfg(test)]` blocks in `src/` files AND all files under `tests/`
- Print the ratio to 2 decimal places
- Exit 1 if ratio < 1.5
- With `--verbose`, print per-file breakdown

### scripts/panic_scan.sh
```bash
#!/bin/bash
# Scans src/ for panic-inducing patterns outside #[cfg(test)] blocks
# Exit code 0 = none found, 1 = panics found
# Usage: ./scripts/panic_scan.sh
```

Requirements:
- Detect: `.unwrap()`, `.expect(`, `panic!(`, `unreachable!(`, `todo!(`
- Exclude lines within `#[cfg(test)]` blocks
- Print file:line for each occurrence
- Exit 1 if any found in production paths

### scripts/pre_commit.sh
```bash
#!/bin/bash
# Full pre-commit gate — runs all checks in sequence
# Exit code 0 = all pass, 1 = at least one failed
# Usage: ./scripts/pre_commit.sh
```

Requirements:
- Run in order: `cargo fmt --check`, `cargo clippy --all-features -- -D warnings`, `cargo test --all-features`, `./scripts/panic_scan.sh`, `./scripts/ratio_check.sh`
- Stop on first failure and print which check failed
- On success, print: "✓ All checks passed — safe to commit"

### scripts/bench_all.sh
```bash
#!/bin/bash
# Runs all benchmarks and fails if any exceed their documented budget
# Exit code 0 = all within budget, 1 = budget exceeded
```

### scripts/coverage.sh
```bash
#!/bin/bash
# Generates HTML coverage report using cargo-tarpaulin or cargo-llvm-cov
# Opens report in browser if --open flag is passed
```

After creating the scripts, install `pre_commit.sh` as a git hook:
```bash
cp scripts/pre_commit.sh .git/hooks/pre-commit
chmod +x .git/hooks/pre-commit
```

---

## TASK 0.4 — Clippy Lint Enforcement

Add the following to the top of `src/lib.rs`:

```rust
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![deny(clippy::todo)]
#![deny(missing_docs)]
#![warn(clippy::pedantic)]
```

Then run `cargo clippy --all-features -- -D warnings` and report all violations found. Do NOT fix them yet — list them so we know the scope of work.

---

## TASK 0.5 — Phase 0 Status Report

After completing tasks 0.1–0.4, output a structured status report:

```
=== PHASE 0 STATUS REPORT ===

Panic Audit:
  Production panics found: N
  In test blocks (acceptable): N
  
Ratio Baseline:
  Production LOC: N
  Test LOC: N
  Ratio: X.XX:1
  Status: PASS/FAIL
  Modules below threshold: [list]

Script Infrastructure:
  scripts/ratio_check.sh: ✓ Created / ✗ Missing
  scripts/panic_scan.sh: ✓ Created / ✗ Missing
  scripts/pre_commit.sh: ✓ Created / ✗ Missing
  scripts/bench_all.sh: ✓ Created / ✗ Missing
  scripts/coverage.sh: ✓ Created / ✗ Missing
  git hook installed: ✓ Yes / ✗ No

Clippy Violations:
  Total violations found: N
  Categories: [list top 5 violation types]

PHASE 0 GATE STATUS: OPEN — N items remaining before Phase 1 can begin

Next actions required:
1. Fix N production panics (convert to Result propagation)
2. Add N test lines to bring undertested modules to 1.5:1
3. Resolve N clippy violations
4. Verify all scripts pass on clean run
```

---

## Standing Orders

These apply for the entire duration of this project, not just Phase 0:

1. **Never commit with failing tests.** If a test fails, fix it before writing any new code.
2. **Never let the ratio drop below 1.5:1.** If it drops, stop feature work and write tests.
3. **Never use `.unwrap()` or `.expect()` in `src/` outside `#[cfg(test)]` blocks.** Every error must be explicit.
4. **Name tests descriptively:** `test_{what}_{condition}_{expected_outcome}`
5. **Every new error variant needs a test that triggers it.**
6. **Every public function needs a doc comment with `# Panics` section** (which must say "This function never panics." if that's true).
7. **When in doubt, write the test first.** TDD is not optional here.

You are building flight-critical infrastructure. Act accordingly.
