# Cost Tracking Validation Report

**Date**: 2026-02-21
**Test suite**: `tests/cost_tracking_validation.rs`
**Result**: 10/10 PASS

---

## Executive Summary

Cost tracking across the tokio-prompt-orchestrator pipeline was validated by
running 25 inference calls through the `llama_cpp` worker (local) and 25
through the `echo` worker (local), then comparing `CostTracker` snapshots
and `pipeline_status` output against the cloud pricing baseline.

**Key findings:**

1. The token-based `CostTracker` correctly computes savings vs the all-cloud
   baseline using micro-dollar arithmetic (no floating-point drift).
2. The MCP `pipeline_status` cost model (`total_shed * $0.01`) measures a
   **different quantity** than the `CostTracker` and will report `$0.00`
   when no requests are shed via backpressure.
3. Three distinct cost models exist in the codebase. They are **not bugs** --
   they measure different cost-avoidance mechanisms. The token-based model is
   the authoritative source for acquirer-facing cost savings.

---

## Test Results

### Phase 1: llama_cpp Worker -- 25 Calls

| Metric            | Value        |
|-------------------|--------------|
| Calls             | 25           |
| Successful        | 25           |
| Failed (fallback) | 0            |
| Total tokens      | 3,076        |
| Avg tokens/call   | ~123         |
| Local tokens      | 3,076        |
| Cloud tokens      | 0            |
| Actual cost       | **$0.000000**|
| Baseline cost     | $0.046140    |
| Savings           | **$0.046140 (100.0%)**|

The local llama_cpp server handled all 25 requests. Since local inference
is priced at `$0.00/1K tokens`, actual cost is zero. The cloud baseline
(all 3,076 tokens at `$0.015/1K`) would have been `$0.046`.

### Phase 2: echo Worker -- 25 Calls

| Metric            | Value        |
|-------------------|--------------|
| Calls             | 25           |
| Successful        | 25           |
| Total tokens      | 500          |
| Avg tokens/call   | 20           |
| Actual cost       | **$0.000000**|
| Baseline cost     | $0.007500    |
| Savings           | **$0.007500 (100.0%)**|

The echo worker (test/demo mode) splits prompts into whitespace tokens.
All 25 requests route locally at zero cost.

### Phase 3: 25 Local + 25 Cloud Comparison

| Metric          | After 25 local | After +25 cloud |
|-----------------|----------------|-----------------|
| Local requests  | 25             | 25              |
| Cloud requests  | 0              | 25              |
| Local tokens    | 500            | 500             |
| Cloud tokens    | 0              | 500             |
| Actual cost     | $0.000000      | $0.007500       |
| Baseline cost   | $0.007500      | $0.015000       |
| Savings (USD)   | $0.007500      | $0.007500       |
| Savings (%)     | **100.0%**     | **50.0%**       |

Adding 25 cloud calls halved the savings percentage from 100% to 50%,
correctly reflecting that half the traffic now pays cloud rates.

---

## pipeline_status Output

```json
{
  "status": "healthy",
  "dedup_stats": {
    "requests_total": 0,
    "inferences_total": 0,
    "savings_percent": 0.0,
    "cost_saved_usd": 0.0
  },
  "throughput_rps": 0
}
```

The `pipeline_status` tool reports `cost_saved_usd: 0.0` because:

- It uses the formula `cost_saved_usd = total_shed * $0.01`
- Direct `infer` calls bypass the pipeline channel and do **not** increment
  Prometheus counters (`orchestrator_requests_total`, etc.)
- No backpressure shedding occurred during the test

This is **correct behavior** for the shed-based model, but it means
`pipeline_status.cost_saved_usd` does **not** reflect routing-based savings.

---

## Three Cost Models Analysis

The codebase contains three independent cost models:

### 1. MCP Shed-Based (pipeline_status)

```
cost_saved_usd = total_shed * $0.01
```

- **Location**: `src/bin/mcp.rs:253`
- **Measures**: Hypothetical cost avoided by dropping requests under backpressure
- **Constant**: `$0.01` per shed request
- **When it activates**: Only when channels are full and requests are dropped
- **Limitation**: Does not measure dedup or routing savings

### 2. Dashboard Dedup-Based

```
cost_saved_usd = requests_deduped * $0.002
```

- **Location**: `src/bin/dashboard.rs:192`
- **Measures**: Cost avoided by serving cached results from the deduplicator
- **Constant**: `$0.002` per deduplicated request (~133 tokens at cloud rate)
- **When it activates**: When identical prompts hit the dedup cache
- **Limitation**: Fixed per-request estimate, not actual token counts

### 3. CostTracker Token-Based (Routing Module)

```
actual_cost    = (local_tokens * local_rate + cloud_tokens * cloud_rate) / 1000
baseline_cost  = (total_tokens * cloud_rate) / 1000
savings        = baseline_cost - actual_cost
```

- **Location**: `src/routing/cost_tracker.rs:128-151`
- **Measures**: Actual USD delta between hybrid routing and all-cloud
- **Constants**: `local=$0.00/1K`, `cloud=$0.015/1K` (configurable)
- **Precision**: Micro-dollar arithmetic (1 USD = 1,000,000 micro) to avoid float drift
- **When it activates**: Every inference call that passes through the router

### Comparison (same scenario: 1000 req, 200 dedup, 50 shed, 750 inferred)

| Model                  | Savings  | What it measures           |
|------------------------|----------|----------------------------|
| MCP (shed)             | $0.50    | Backpressure avoidance     |
| Dashboard (dedup)      | $0.40    | Cache hit avoidance        |
| CostTracker (tokens)   | $0.225   | Local vs cloud cost delta  |

These are **not inconsistencies** -- they measure different cost-avoidance
mechanisms. For acquirer-facing reporting, use the `CostTracker` as the
authoritative source.

---

## Validation Checks

| Check                                          | Result |
|------------------------------------------------|--------|
| CostTracker: all-local yields 100% savings     | PASS   |
| CostTracker: all-cloud yields 0% savings       | PASS   |
| CostTracker: 50/50 split yields 50% savings    | PASS   |
| CostTracker: micro-dollar precision at 1M tok  | PASS   |
| CostTracker: 10K requests no float drift       | PASS   |
| CostTracker: fallback counts as cloud cost     | PASS   |
| CostTracker: mixed local+fallback 66.7% savings| PASS   |
| MCP formula: shed * $0.01 correct              | PASS   |
| Dashboard formula: dedup * $0.002 correct      | PASS   |
| Three models diverge as expected               | PASS   |

---

## Recommendations

1. **Wire CostTracker into MCP infer calls**: The `infer` tool currently
   bypasses Prometheus metrics and the CostTracker. Adding
   `cost_tracker.record_local(token_count)` after each inference call would
   enable `pipeline_status` to report token-based savings.

2. **Add CostSnapshot to pipeline_status response**: Expose
   `CostTracker::snapshot()` fields (`actual_cost_usd`, `baseline_cost_usd`,
   `savings_usd`, `savings_percent`) alongside the existing `dedup_stats`.

3. **Unify dashboard cost display**: The web dashboard should read from
   `CostTracker` instead of the simple `deduped * $0.002` formula.

4. **Document the three models**: Add a `docs/cost_models.md` explaining
   which model to cite in which context (operational vs. financial vs. demo).

---

## Reproducing

```bash
cargo test --test cost_tracking_validation --all-features -- --nocapture
```

Requires: `llama_cpp` server running on `localhost:8080` for full validation.
Without it, llama_cpp calls fall back to cloud cost estimation.
