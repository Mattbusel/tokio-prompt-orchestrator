# Changelog

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
