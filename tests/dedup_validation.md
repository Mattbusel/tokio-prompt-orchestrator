# Deduplication Validation Report

**Date:** 2026-02-21
**Method:** MCP `batch_infer` + `pipeline_status` via JSON-RPC stdio, plus Rust integration tests
**Binary:** `target/release/mcp.exe --worker echo`

---

## Test Protocol

1. Built MCP server: `cargo build --features mcp --bin mcp --release`
2. Sent JSON-RPC messages over stdio to the MCP server:
   - `initialize` (MCP protocol handshake)
   - `notifications/initialized`
   - `tools/call` with `batch_infer`: 20 identical prompts (`"What is the capital of France?"`)
   - `tools/call` with `pipeline_status`
3. Ran Rust integration tests in `tests/dedup_validation.rs`

---

## MCP Results (batch_infer)

### batch_infer Response

```json
{
  "job_id": "195a87d2-f707-4456-b7be-e8cb5bc79094",
  "prompts_submitted": 20,
  "status": "accepted"
}
```

All 20 prompts were accepted into the pipeline.

### pipeline_status Response

```json
{
  "status": "healthy",
  "circuit_breakers": {
    "echo": "closed"
  },
  "channel_depths": {
    "rag_to_assemble": "0/512",
    "assemble_to_infer": "0/512",
    "infer_to_post": "0/1024",
    "post_to_stream": "0/512"
  },
  "dedup_stats": {
    "requests_total": 16,
    "inferences_total": 16,
    "savings_percent": 0.0,
    "cost_saved_usd": 0.0
  },
  "throughput_rps": 16
}
```

### MCP Dedup Rate: 0%

All 20 prompts were processed individually through all 5 pipeline stages.
`requests_total` shows 16 (4 still in-flight at query time); `inferences_total` equals `requests_total`; `savings_percent` is `0.0`.

---

## Root Cause Analysis

The `batch_infer` MCP tool (`src/bin/mcp.rs:266`) sends each prompt directly to `pipeline.input_tx.send()` without consulting the `Deduplicator`:

```rust
// Current batch_infer implementation (no dedup check):
for (i, prompt) in params.prompts.into_iter().enumerate() {
    let request = PromptRequest { ... };
    self.pipeline.input_tx.send(request).await?;
}
```

The `Deduplicator` component exists in `src/enhanced/dedup.rs` and works correctly, but it is **not wired into** either:
- The `batch_infer` tool path
- The `spawn_pipeline` stage chain

The pipeline stages (`src/stages.rs`) process every request through all 5 stages (RAG, Assemble, Inference, Post, Stream) unconditionally.

---

## Standalone Deduplicator Validation

The `Deduplicator` component achieves the expected 95% rate when used directly:

| Test | New | Cached | In-Progress | Dedup Rate |
|------|-----|--------|-------------|------------|
| Sequential (complete first, then 19 checks) | 1 | 19 | 0 | **95.0%** |
| Concurrent (20 simultaneous, before completion) | 1 | 0 | 19 | **95.0%** |
| Integrated simulation (dedup + pipeline) | 1 | 19 | 0 | **95.0%** |
| 20 different prompts | 20 | 0 | 0 | **0.0%** |

All tests pass: `cargo test --test dedup_validation`

### Key test: `test_dedup_20_identical_prompts_standalone_achieves_95_percent`

```
Deduplicator::check_and_register("key") x 20
  - Request  1: DeduplicationResult::New → processed, completed
  - Requests 2-20: DeduplicationResult::Cached → returned cached result
  Dedup rate: 19/20 = 95.0%
```

### Key test: `test_dedup_integrated_with_pipeline_achieves_95_percent`

Simulates the correct integration pattern (dedup check before pipeline send):

```
For each of 20 identical prompts:
  1. dedup.check_and_register(key)
  2. If New → send to pipeline, complete token
  3. If Cached → return cached result (skip pipeline)

  Result: 1 inference executed, 19 deduped = 95.0%
```

---

## Recommendation: Wire Deduplicator into batch_infer

The fix is to add a dedup check in `batch_infer` before sending to the pipeline:

```rust
// Proposed batch_infer with dedup integration:
async fn batch_infer(&self, params: BatchInferParams) -> String {
    let dedup = &self.deduplicator; // Add Deduplicator to OrchestratorMcp

    for (i, prompt) in params.prompts.into_iter().enumerate() {
        let key = dedup_key(&prompt, &HashMap::new());
        match dedup.check_and_register(&key).await {
            DeduplicationResult::New(token) => {
                // Send to pipeline
                let request = PromptRequest { ... };
                self.pipeline.input_tx.send(request).await?;
                // Complete token after pipeline returns
            }
            DeduplicationResult::Cached(_) | DeduplicationResult::InProgress => {
                // Skip pipeline — already processed or in-flight
            }
        }
    }
}
```

This would bring the MCP `batch_infer` dedup rate from 0% to 95% for identical prompts.

---

## Test Matrix

| Test File | Tests | Status |
|-----------|-------|--------|
| `tests/dedup_validation.rs` | 8 | All passing |
| `scripts/mcp_dedup_test.sh` | MCP stdio integration | Passing (0% dedup confirmed) |

```
$ cargo test --test dedup_validation -- --test-threads=1
running 8 tests
test test_dedup_20_identical_prompts_concurrent_first_batch ... ok
test test_dedup_20_identical_prompts_standalone_achieves_95_percent ... ok
test test_dedup_different_prompts_no_dedup ... ok
test test_dedup_integrated_with_pipeline_achieves_95_percent ... ok
test test_dedup_key_deterministic ... ok
test test_dedup_key_different_for_different_prompts ... ok
test test_dedup_stats_reflect_cached_count ... ok
test test_pipeline_20_identical_prompts_all_processed ... ok

test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
