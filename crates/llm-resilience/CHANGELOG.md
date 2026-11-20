# Changelog

## 0.2.1 (2026-11-20)

- crates.io description now says the side the README already takes: you pay for the same prompt twice, and retries capped by count bill a full context each time. No code change.

## [0.2.0] - 2026-11-19

You pay for the same prompt twice, and retries are how bills triple. This release refuses both
by default.

### Changed (breaking default)

- `ResilienceMiddleware::new()` now replays a completed 2xx answer to an identical inference
  request (a JSON `POST` whose body has `messages`, `prompt`, `input` or `contents`) for 5
  minutes, whatever its `temperature`. Before, only requests in flight at the same moment
  shared a call, so a retry wrapper, a double-submitting UI or an agent loop that re-sent the
  same request a second later paid twice. Replayed answers carry `x-llm-resilience: replayed`.
  Ask for a fresh sample per request with `Cache-Control: no-cache` (or `no-store`, or
  `SkipDedup`); restore the old behaviour with `.with_replay_window(Duration::ZERO)`. Failures
  and non-inference requests (GET, file uploads, anything without a prompt) are never replayed.
  `without_dedup()` turns replay off too.

### Added

- `RetrySpend`: a retry budget in USD. `try_retry(estimated_usd)` books a retry only if it fits
  in a floor plus 10% (`with_ratio`) of real spend recorded with `record_spend`, per hour
  (`with_window`). Clones share one budget. A retry estimated at $0 always fits.
- `retryable_status(u16)`: 408, 409, 425, 429, 500, 502, 503, 504 and 529 are worth retrying;
  every other status is not.
- `ResilienceMiddleware::with_replay_window(Duration)`, `replayed()`, `DEFAULT_REPLAY_WINDOW`,
  `REPLAY_HEADER`.

## [0.1.1] - 2026-11-18

### Added

- `CircuitBreaker::try_acquire()` and the public `CallPermit`, for callers that decide
  themselves which outcomes count. Report `success()` or `failure()` on the permit, or drop it
  to record nothing (a cancelled call, or an error that says nothing about the dependency's
  health, such as a bad request). Admission happens under the same lock as the state
  transitions, so the half-open probe limit holds, and the permit carries its generation, so a
  call admitted before a state change cannot move the breaker when it finishes.

## [0.1.0] - 2026-11-17

First release, split out of tokio-prompt-orchestrator 2.5.0, which re-exports
every type here at its old path.

- `Dedup<K, V>`: single-flight deduplication for any key and cloneable value,
  cancellation-safe, with `claim()` for a leader that shares something other
  than what it returns.
- `CircuitBreaker`: consecutive-failure threshold, success-rate close,
  exponential probe backoff (up to 64x), one probe at a time while half-open
  (`with_max_probes`), generation-tagged outcomes, `retry_after_sync()` for a
  `Retry-After` header, event hooks per breaker and process-wide.
- `ResilienceLayer` / `ResilienceService` (feature `tower`).
- `ResilienceMiddleware` (feature `reqwest-middleware`): dedup keyed on method,
  URL, headers and body, streaming requests never deduplicated, one breaker per
  host by default.
