# tokio-prompt-orchestrator
Multi-core, Tokio-native orchestration for LLM pipelines.

tokio-prompt-orchestrator

Multi-core, Tokio-native orchestration for LLM pipelines.
Shard prompt work across CPU cores with bounded queues, backpressure, and zero-blocking async stages. Scale from laptop → cluster without changing your code.

TL;DR: Treat each core like a prompt-processing node (RAG → assembly → inference → post-processing → streaming), coordinated by Tokio. Keep GPUs/LLM servers behind a clean worker API; let the orchestrator do everything else.

Why

Most “agent” frameworks fake concurrency. This project makes it real:

Tokio at the core: async stages, bounded channels, metrics, backpressure.

Per-core runtimes (optional): pin runtimes to cores for cache locality.

Works with any model server: llama.cpp, TGI, vLLM, OpenAI, etc.

Production knobs: timeouts, retries, circuit breakers, queue depths.

Features (MVP)

Pluggable stages: RagStage → AssembleStage → InferenceStage → PostStage → StreamStage

Bounded MPSC channels for deterministic backpressure

CPU isolation via spawn_blocking / optional rayon pool for heavy tasks

Session affinity (keep a conversation on the same “macro-core”)

Tracing + metrics (latency, queue depth, drops, p95/p99)

Graceful shed (drop oldest / reject new on surge)

Roadmap below for gRPC mesh + distributed scale-out.

Architecture
           ┌──────────┐      ┌────────────┐      ┌─────────────┐      ┌─────────────┐      ┌──────────────┐
Request →  │ RAG      │ ---> │ ASSEMBLE   │ ---> │ INFERENCE   │ ---> │ POSTPROCESS │ ---> │ STREAM (SSE) │ → Client
           │ (I/O)    │      │ (CPU-lite) │      │ (blocking)  │      │ (CPU)       │      │ (I/O)        │
           └──────────┘      └────────────┘      └─────────────┘      └─────────────┘      └──────────────┘
              mpsc 512            mpsc 512            mpsc 1024            mpsc 512             mpsc 256

                   └──────────────────── all orchestrated by a Tokio multi-thread runtime ───────────────────┘

Optional “per-core” mode:
  core0: RAG+ASSEMBLE   core1: INFERENCE   core2: POST   core3: STREAM  (affinity pinned; session sharded)


Model workers live behind a trait:

#[async_trait::async_trait]
pub trait ModelWorker: Send + Sync {
    async fn infer(&self, prompt: String) -> anyhow::Result<tokio_stream::wrappers::ReceiverStream<String>>;
}


Implement for llama.cpp HTTP, OpenAI, TGI, etc.

Quickstart
Prereqs

Rust 1.80+

cargo

(Optional) llama.cpp/TGI endpoint for the inference stage
