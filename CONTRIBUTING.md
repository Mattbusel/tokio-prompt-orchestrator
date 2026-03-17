# Contributing to tokio-prompt-orchestrator

Thanks for your interest. This is a research-grade system — contributions that improve reliability, expand test coverage, or extend the resilience pipeline are most welcome.

## What we want

- **New resilience primitives** — bulkheads, hedged requests, adaptive timeouts
- **Provider adapters** — Mistral, Gemini, local llama.cpp HTTP
- **Test coverage** — the project enforces ≥1.5:1 test-to-production LOC ratio; new code must ship with tests
- **Benchmarks** — latency budget measurements for any new hot-path code
- **Bug reports** — especially anything that causes a panic or a channel deadlock

## What we don't want

- Removing error handling or replacing `Result` returns with `unwrap()`
- Dependencies that aren't justified (see `Cargo.toml` policy in `CLAUDE.md`)
- PRs with no tests for new production code

## How to contribute

1. Fork and clone
2. Install the pre-commit gate (one-time setup):
   ```bash
   git config core.hooksPath .githooks
   ```
   This runs `scripts/pre_commit.sh` automatically before every commit —
   covering fmt, clippy, tests, panic-scan, and ratio-check.
3. `cargo test --all-features` — must pass before and after your change
4. `./scripts/ratio_check.sh` — test ratio must stay ≥1.5:1
5. `cargo clippy --all-features -- -D warnings` — zero warnings
6. Open a PR with a description of *why*, not just *what*

## Commit format

```
[feat|fix|test|refactor|perf|docs] short description

Body explaining the motivation and any tradeoffs.
```

## Questions

Open a [Discussion](https://github.com/Mattbusel/tokio-prompt-orchestrator/discussions) — that's the right place for design questions, research ideas, or "would you accept a PR for X?"
