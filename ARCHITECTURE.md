# Architecture Documentation

## Overview

tokio-prompt-orchestrator implements a staged event-driven architecture (SEDA).
Each stage is an independent async Tokio task communicating via bounded MPSC channels.

## Pipeline Stages

Five stages: RAG (512) -> Assemble (512) -> Inference (1024) -> Post (512) -> Stream (256).
Channel sizes are bounded. Inference has the largest buffer (1024) because inference is slowest.

## Deduplication Algorithm

In-process dedup uses a DashMap keyed on a hash of the prompt text. State machine:
- New: insert entry, process, call complete() which broadcasts result to waiters
- InProgress: subscribe to broadcast channel, await result (N requests -> 1 inference call)
- Cached: return cached result immediately

Cache TTL default: 5 minutes. Dropped tokens remove entries to prevent waiter hangs.

Cross-node dedup uses Redis SET key NX EX ttl (single atomic command). Claimed keys
prevent other nodes from processing the same request. TTL ensures expiry on node failure.

## Circuit Breaker State Machine

CLOSED (normal) -> N failures -> OPEN (fail-fast) -> timeout -> HALF-OPEN (one probe).
Success from half-open closes; failure reopens. Configurable: failure_threshold (5),
success_threshold (0.5), timeout (60s), window_size (10).

## Self-Improving Control Loop

Feedback-driven control plane that observes telemetry and adjusts operating parameters.

What it optimizes:
- TuningController: worker concurrency, buffer sizes, timeouts
- CostOptimizer: local/cloud routing split
- Autoscaler: target worker count
- LearnedRouter: model weights (epsilon-greedy bandit, epsilon 0.3 -> 0.05)
- PromptOptimizer: prompt template parameters

Safety: MetaTaskGenerator proposals must pass ValidationGate (cargo test + clippy).
Failing proposals are recorded in AgentMemory; loop continues from last SnapshotStore checkpoint.

## Backpressure Propagation

Backpressure originates at Inference (slowest). send_with_shed drops incoming items
when a channel is full. Dropped requests go to the DeadLetterQueue ring buffer.
High-priority requests (Priority::Critical) use blocking send() and bypass shedding.

## Distributed Mode

Leader election: SETNX orchestrator:leader <node-id> EX 30. Leader renews TTL every 10s.
Followers receive work via NATS subjects. Session affinity routes same-session requests
to the same node via shard_session(session_id, num_nodes).

## Prometheus Metrics

Key metrics: orchestrator_requests_total, orchestrator_requests_shed_total,
orchestrator_errors_total, orchestrator_stage_duration_seconds, orchestrator_queue_depth,
inference_time_to_first_token_seconds, circuit_breaker_state_transitions_total.

Sensitive fields (prompts, responses, API keys) are never logged.
