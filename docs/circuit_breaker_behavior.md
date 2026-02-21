# Circuit Breaker Behavior Under Rapid Load

> Test date: 2026-02-21
> Source: `tests/circuit_breaker_50req.rs`
> Pipeline: tokio-prompt-orchestrator, worker=llama_cpp (simulated via `SimulatedWorker`)

---

## Configuration

| Parameter | Value | Description |
|---|---|---|
| `failure_threshold` | **5** | Consecutive failures before circuit opens |
| `success_threshold` | **0.80** (80%) | Success rate required to close from half-open |
| `recovery_timeout` | **200ms** | Time in open state before testing recovery |
| `window_size` | **100** | Sliding window for success-rate calculation |

Default MCP server configuration uses `circuit_breaker_threshold: 5`.

---

## Test Protocol

1. Created a `CircuitBreaker::new(5, 0.8, Duration::from_millis(200))`
2. Injected a `SimulatedWorker` that **fails on calls 8..27** (20 failures), succeeds otherwise
3. Sent **50 rapid sequential requests** through `breaker.call()`
4. Logged circuit breaker state after every batch of 10
5. After request #29, slept 250ms to allow half-open recovery probe

---

## Results: Batch Snapshots

| Batch | Requests | CB State | Failures | Successes | Success Rate | Elapsed |
|-------|----------|----------|----------|-----------|-------------|---------|
| 1 | 10 | **Closed** | 2 | 8 | 80.0% | 0ms |
| 2 | 20 | **Open** | 5 | 8 | 61.5% | 0ms |
| 3 | 30 | **Open** | 5 | 8 | 61.5% | 0ms |
| 4 | 40 | **Open** | 6 | 8 | 0.0% | 265ms |
| 5 | 50 | **Open** | 6 | 8 | 0.0% | 265ms |

---

## Per-Request Trace

```
[00] CLOSED    | failures=0 success_rate=100% | OK: response for call 0
[01] CLOSED    | failures=0 success_rate=100% | OK: response for call 1
[02] CLOSED    | failures=0 success_rate=100% | OK: response for call 2
[03] CLOSED    | failures=0 success_rate=100% | OK: response for call 3
[04] CLOSED    | failures=0 success_rate=100% | OK: response for call 4
[05] CLOSED    | failures=0 success_rate=100% | OK: response for call 5
[06] CLOSED    | failures=0 success_rate=100% | OK: response for call 6
[07] CLOSED    | failures=0 success_rate=100% | OK: response for call 7
[08] CLOSED    | failures=1 success_rate= 89% | FAILED: simulated failure at call 8
[09] CLOSED    | failures=2 success_rate= 80% | FAILED: simulated failure at call 9
[10] CLOSED    | failures=3 success_rate= 73% | FAILED: simulated failure at call 10
[11] CLOSED    | failures=4 success_rate= 67% | FAILED: simulated failure at call 11
[12] OPEN      | failures=5 success_rate= 62% | FAILED: simulated failure at call 12  ← CIRCUIT OPENS HERE
[13] OPEN      | failures=5 success_rate= 62% | REJECTED: circuit open
[14] OPEN      | failures=5 success_rate= 62% | REJECTED: circuit open
  ...requests 15-29 all REJECTED (circuit open, fast-fail)...
[30] OPEN      | failures=6 success_rate=  0% | FAILED: simulated failure at call 13  ← half-open probe fails
[31] OPEN      | failures=6 success_rate=  0% | REJECTED: circuit open
  ...requests 32-49 all REJECTED (circuit remains open)...
```

---

## Key Findings

### 1. Circuit Opens at Request #12 (Threshold = 5 consecutive failures)

The circuit breaker transitions from **Closed** to **Open** on the 5th consecutive failure:

- Requests 0-7: All succeed (failures counter = 0)
- Request 8: First failure (failures = 1)
- Request 9: Second failure (failures = 2)
- Request 10: Third failure (failures = 3)
- Request 11: Fourth failure (failures = 4)
- **Request 12: Fifth failure (failures = 5) — CIRCUIT OPENS**

The state machine correctly counts **consecutive** failures. Any success during the failure sequence resets the counter to 0 (`record_success` sets `failures = 0` when Closed).

### 2. Open State Provides Instant Fast-Fail

Once open, requests 13-29 are **immediately rejected** without reaching the worker. This is the core value proposition — protecting a failing downstream service from further load while giving it time to recover.

- Zero worker calls during open state
- Rejection latency: sub-microsecond (no async work)

### 3. Half-Open Recovery Probe

After the 200ms `recovery_timeout` elapses (triggered by the sleep at request #29), the circuit transitions to **HalfOpen** and allows one probe request through:

- **Request #30**: Probe reaches the worker — but the worker is still failing (call 13, within fail range 8..27)
- The probe fails, circuit **immediately reopens**
- Requests 31-49 are rejected again

### 4. Success-Rate-Based Closing

The circuit requires **80% success rate** in the sliding window to close from HalfOpen. Since the recovery probe failed, the window contained only failures, so the circuit could not close.

In a real scenario with a recovered service:
1. Timeout elapses → HalfOpen
2. Probe succeeds → success_rate recalculated on cleared window
3. If success_rate >= 0.8 → Closed
4. If probe fails → immediately back to Open

---

## State Machine Summary

```
                      5 consecutive failures
    ┌────────┐  ─────────────────────────────►  ┌────────┐
    │ CLOSED │                                   │  OPEN  │
    └────────┘  ◄──────────────────────────────  └────────┘
                  success_rate >= 80%                │
                  (from HalfOpen)                    │ timeout elapsed
                        ▲                            ▼
                        │                      ┌───────────┐
                        └──────────────────────│ HALF-OPEN │
                          probe succeeds       └───────────┘
                          + rate >= 80%              │
                                                     │ probe fails
                                                     ▼
                                                ┌────────┐
                                                │  OPEN  │
                                                └────────┘
```

---

## Performance Impact

| Metric | Value |
|---|---|
| 50 requests total elapsed | **265ms** (includes 250ms deliberate sleep) |
| Requests actually reaching worker | **14 of 50** (8 success + 5 failure + 1 probe) |
| Requests fast-failed by circuit | **36 of 50** (72%) |
| Worker load reduction | **72%** — circuit breaker shielded the failing service |

---

## Operational Notes

- **Default MCP threshold**: The MCP server's `PipelineConfig::default()` sets `circuit_breaker_threshold: 5`, matching this test
- **Configurable at runtime**: Use `configure_pipeline` MCP tool to adjust `circuit_breaker_threshold`
- **Manual recovery**: `CircuitBreaker::reset()` immediately closes the circuit
- **Manual trip**: `CircuitBreaker::trip()` immediately opens the circuit
- **Thread-safe**: All operations use `Arc<RwLock<>>` — safe for concurrent pipeline stages
- **MCP status gap**: The current `pipeline_status` MCP tool reports hardcoded "closed" states rather than querying the real `CircuitBreaker` instances — this should be wired up for production observability
