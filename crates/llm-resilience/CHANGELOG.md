# Changelog

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
