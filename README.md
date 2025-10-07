tokio-prompt-orchestrator

Multi-core, Tokio-native orchestration for LLM pipelines.

---
 Overview

Shard prompt work across CPU cores with **bounded queues**, **backpressure**, and **zero-blocking async stages**.
Scale seamlessly from **laptop → cluster** without changing a line of code.

> **TL;DR:** Treat each core like a *prompt-processing node*
> (RAG → Assembly → Inference → Post-Processing → Streaming),
> all coordinated by Tokio.
> Keep GPUs/LLM servers behind a clean worker API —
> let the orchestrator do everything else.

---

##  Why

Most “agent” frameworks fake concurrency.
This project makes it **real**.

 **Tokio at the core** — async stages, bounded channels, metrics, and backpressure
 **Per-core runtimes (optional)** — pin runtimes to cores for cache locality
 **Works with any model server** — `llama.cpp`, TGI, vLLM, OpenAI, etc.
 **Production knobs** — timeouts, retries, circuit breakers, queue depths

---

##  Features (MVP)

* **Pluggable stages** → `RagStage → AssembleStage → InferenceStage → PostStage → StreamStage`
* **Bounded MPSC channels** for deterministic backpressure
* **CPU isolation** via `spawn_blocking` / optional `rayon` pool for heavy tasks
* **Session affinity** keeps a conversation on the same “macro-core”
* **Tracing + metrics**: latency, queue depth, drops, p95/p99
* **Graceful shed** — drop oldest / reject new on surge

 *Roadmap below for gRPC mesh + distributed scale-out.*

---

##  Architecture

```
RAG (I/O)
   ↓
ASSEMBLE (CPU-lite)
   ↓
INFERENCE (blocking)
   ↓
POSTPROCESS (CPU)
   ↓
STREAM (I/O)
   ↓
CLIENT

mpsc 512 → mpsc 512 → mpsc 1024 → mpsc 512 → mpsc 256
```

*All orchestrated by a **Tokio multi-threaded runtime** with bounded queues and backpressure.*

**Optional per-core mode:**

```
core0 → RAG + ASSEMBLE
core1 → INFERENCE
core2 → POSTPROCESS
core3 → STREAM
```

Each runtime can be pinned to a CPU core, with sessions sharded by hash for cache locality.

---




##  Observability

* **Tracing spans** per stage: `rag`, `assemble`, `infer`, `post`, `stream`
* Emit:

  * Queue depth (gauge)
  * Enqueue/dequeue latency
  * Task duration histograms (p50/p95/p99)
  * Drops/rejections (counter)
* Export to **OTLP / Prometheus** via feature flag

---

##  Roadmap

* [ ] gRPC worker protocol (streaming tokens, cancel, deadlines)
* [ ] Session KV-cache affinity & reuse hints
* [ ] Micro-batch inference policy
* [ ] Distributed mesh (NATS / Kafka) for cross-node queues
* [ ] Declarative DAG config (`.toml` / `.yaml`)
* [ ] Bench suite (latency under surge, sustained tokens/sec)
* [ ] Crate split: `core`, `stages-*`, `workers-*`, `bin/`
* [ ] Adapters: `llama.cpp`, `vLLM`, `OpenAI`, `Candle`, `Burn`

---

##  FAQ

**Does this make models faster?**
No — it makes *everything around* the model faster, safer, and observable.

**GPU support?**
Treat GPU workers as blocking services behind `ModelWorker`. The orchestrator just schedules and streams.

**Is per-core required?**
No. The default multi-threaded Tokio runtime is excellent. Per-core is an optimization — benchmark before enabling.


