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

RAG (I/O) → ASSEMBLE (CPU-lite) → INFERENCE (blocking) → POSTPROCESS (CPU) → STREAM (I/O) → CLIENT
        mpsc 512         mpsc 512          mpsc 1024           mpsc 512           mpsc 256





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

Per-Core Mode (optional)

Launch N runtimes with core_affinity to pin each to a CPU core.

Route sessions by hash to keep cache locality and reuse per-session state.

Benchmark both modes; affinity helps locality but reduces elasticity.

Observability

tracing spans per stage (rag, assemble, infer, post, stream)

Emit:

queue depth (gauge)

enqueue/dequeue latency

task duration (histograms p50/p95/p99)

drops/rejections (counter)

Export to OTLP / Prometheus (feature flag).

Roadmap

 gRPC worker protocol (streaming tokens, cancel, deadlines)

 Session KV-cache affinity & reuse hints for CPU inference paths

 Batching policy (micro-batch inference workers)

 Distributed mesh (NATS/Kafka) for cross-node queues

 DAG config (.toml/.yaml) to declare pipelines declaratively

 Bench suite (latency under surge; sustained tokens/sec)

 Crate split: orchestrator-core, stages-*, workers-*, bin/

 Adapters: llama.cpp HTTP, TGI/vLLM, OpenAI/Anthropic, Candle/Burn


 FAQ

Does this make models faster?
No. It makes everything around the model faster, safer, and observable.

GPU support?
Treat GPU workers as blocking services behind the ModelWorker trait. The orchestrator just schedules and streams.

Is per-core required?
No. The default multi-threaded Tokio runtime is great. Per-core is an optimization—benchmark before enabling.
