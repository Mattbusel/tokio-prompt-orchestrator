# Architecture

tokio-prompt-orchestrator implements a staged event-driven architecture (SEDA)
for LLM inference pipelines. Each stage is an independent async Tokio task
communicating via bounded MPSC channels.

## Pipeline Stages

Five stages connected by bounded channels with backpressure:

  [Client]
     |
  [RAG stage]          -- retrieves context from vector store
     | capacity: 512
  [Assemble stage]     -- fills prompt template with retrieved context
     | capacity: 512
  [Inference stage]    -- calls ModelWorker (OpenAI, Anthropic, llama.cpp, vLLM)
     | capacity: 1024
  [Post stage]         -- filtering, PII redaction, quality checks
     | capacity: 512
  [Stream stage]       -- emits to OutputSink (SSE, WebSocket, log)
     | capacity: 256
  [Client]

The Inference stage has the largest buffer (1024) because inference is the
slowest stage. All other buffers are smaller to conserve memory.

## Deduplication Algorithm

### In-Process Dedup (enhanced::dedup)

Uses a DashMap keyed on a hash of the prompt text. Per-key state machine:

  check_and_register(key)
    |
    +-- [New]         -> insert entry, return DeduplicationToken
    |                    caller processes the request, then calls complete(token, result)
    |                    complete() broadcasts result to all waiters and caches it
    |
    +-- [InProgress]  -> subscribe to broadcast channel, await result
    |                    when original request completes, all waiters receive same result
    |                    N identical concurrent requests collapse to 1 inference call
    |
    +-- [Cached]      -> return cached result immediately (no inference call)

Cache entries expire after the configured TTL (default 5 minutes).

If a DeduplicationToken is dropped without complete() being called, the entry
is removed from the map so waiters unblock on their next check_and_register().

### Cross-Node Dedup (distributed::redis_dedup)

Uses Redis SET key NX EX ttl -- a single atomic command -- to claim ownership:

  Node A: SET dedup:hash:<key> node-A NX EX 300 -> OK    => process request
  Node B: SET dedup:hash:<key> node-B NX EX 300 -> nil   => skip (AlreadyClaimed)

Properties:
- Atomic: NX + EX in one command, no race condition.
- TTL-bounded: keys auto-expire, preventing unbounded Redis growth.
- Non-blocking: async Redis driver, never blocks the Tokio executor.
- Graceful degradation: Redis unavailability returns an error, not a panic.

## Circuit Breaker State Machine

Prevents cascading failures when a downstream service degrades.

  +----------+
  |  CLOSED  |  <---- normal operation; all requests pass through
  +----------+
       |
  N failures within window
       |
       v
  +--------+
  |  OPEN  |  <---- all requests rejected immediately (fail-fast)
  +--------+
       |
  timeout expires
       |
       v
  +-----------+
  | HALF-OPEN |  <---- one probe request allowed through
  +-----------+
       |
   +-------+-------+
   |               |
success          failure
   |               |
   v               v
CLOSED           OPEN  (reset timer)

Configuration:
- failure_threshold: failures before opening (default 5)
- success_threshold: success rate 0-1 to close from half-open (default 0.5)
- timeout: wait before half-open transition (default 60s)
- window_size: sliding result window for rate calculation (default 10)

State transitions emit orchestrator_circuit_breaker_state_transitions_total{state}
and orchestrator_circuit_breaker_requests_rejected_total Prometheus metrics.

## Self-Improving Control Loop

A feedback-driven control plane that observes telemetry, detects anomalies,
and autonomously adjusts operating parameters.

### What It Optimizes

TuningController   -- worker concurrency, buffer sizes, timeouts
CostOptimizer      -- local-vs-cloud routing split
Autoscaler         -- target worker count
LearnedRouter      -- model selection weights (epsilon-greedy bandit)
PromptOptimizer    -- prompt template parameters

### Convergence Criteria

TuningController: stops when metric change is below 1% for 3 consecutive cycles.
Autoscaler: stops when queue depth variance is below threshold for 5 measurements.
LearnedRouter: epsilon decays from 0.3 to 0.05 over time (exploitation dominates).
CostOptimizer: converges when 7-day rolling cost-per-quality is stable.

### Safety Guardrails

All modifications proposed by MetaTaskGenerator must pass ValidationGate before
being applied. The gate runs cargo test and cargo clippy in a sandboxed subprocess.
A failing gate records the result in AgentMemory but does not apply the change.
The loop continues from the last known good snapshot in SnapshotStore.

## Backpressure Propagation

Backpressure originates at the Inference stage (slowest) and propagates upstream.
When the Inference channel (capacity 1024) is full, send_with_shed in Assemble
drops the new item. The dropped request is recorded in the DeadLetterQueue ring
buffer and increments orchestrator_requests_shed_total{stage=assemble}.

Shedding is preferred over blocking to prevent cascading stalls upstream.
High-priority requests (Priority::Critical) use blocking send().await instead.

## Distributed Mode

Multiple nodes coordinate via Redis (dedup, leader election) and NATS (work queues).

### Leader Election

  SETNX orchestrator:leader <node-id> EX 30
    -> OK    => this node is leader, renews every 10s
    -> fails => this node is follower, receives work via NATS

Leader responsibilities: task distribution, cache eviction, autoscale decisions.

### Work Distribution

  orchestrator.requests.{shard_id}    -- inference requests
  orchestrator.results.{session_id}   -- results routed to origin node
  orchestrator.control                -- autoscale / config updates from leader

Session affinity (shard_session) ensures same-session requests land on the same
node, preserving in-process cache and dedup state.

## Channel Sizing Reference

| Channel          | Capacity | Rationale                              |
|------------------|----------|----------------------------------------|
| RAG to Assemble  | 512      | Fast stage, minimal buffering needed   |
| Assemble to Inf  | 512      | Pre-inference buffer                   |
| Inference to Post| 1024     | Largest: inference is slowest stage    |
| Post to Stream   | 512      | Post-processing buffer                 |
| Stream output    | 256      | Small: fast emission to client         |

## Feature Flags

See Cargo.toml for the full feature list. Architecture-relevant flags:

web-api        -- enables axum HTTP server (REST, SSE, WebSocket)
metrics-server -- enables Prometheus scrape endpoint
caching        -- enables Redis result cache
rate-limiting  -- enables token-bucket rate limiter
self-tune      -- enables PID controllers and telemetry bus
self-modify    -- enables MetaTaskGenerator and ValidationGate (requires self-tune)
intelligence   -- enables LearnedRouter and Autoscaler (requires self-tune)
evolution      -- enables A/B experiments and snapshot rollback
self-improving -- meta-feature enabling all self-improvement subsystems
tui            -- enables ratatui terminal dashboard
mcp            -- enables MCP server (requires web-api)
distributed    -- enables Redis dedup and NATS pub/sub
