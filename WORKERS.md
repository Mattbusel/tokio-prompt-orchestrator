# Real Model Integration Guide

This guide shows how to integrate production LLM backends with the orchestrator.

## Available Workers

| Worker | Backend | Use Case |
|--------|---------|----------|
| `EchoWorker` | None (testing) | Development, testing |
| `OpenAiWorker` | OpenAI API | GPT-4, GPT-3.5 models |
| `AnthropicWorker` | Anthropic API | Claude 3/3.5 models |
| `LlamaCppWorker` | llama.cpp server | Local inference, privacy |
| `VllmWorker` | vLLM server | High-throughput, batching |

## Quick Start

### 1. OpenAI (Cloud API)

**Setup:**
```bash
export OPENAI_API_KEY="sk-..."
```

**Code:**
```rust
use tokio_prompt_orchestrator::{OpenAiWorker, spawn_pipeline};
use std::sync::Arc;

let worker = Arc::new(
    OpenAiWorker::new("gpt-3.5-turbo-instruct")
        .with_max_tokens(512)
        .with_temperature(0.7)
);

let handles = spawn_pipeline(worker);
```

**Run:**
```bash
cargo run --example openai_worker
```

**Available Models:**
- `gpt-4-turbo` - Most capable
- `gpt-4` - High quality
- `gpt-3.5-turbo-instruct` - Fast completion model
- See: https://platform.openai.com/docs/models

**Pricing (as of 2024):**
- GPT-4 Turbo: $0.01/1K tokens (input), $0.03/1K tokens (output)
- GPT-3.5 Turbo: $0.0005/1K tokens (input), $0.0015/1K tokens (output)

---

### 2. Anthropic Claude (Cloud API)

**Setup:**
```bash
export ANTHROPIC_API_KEY="sk-ant-..."
```

**Code:**
```rust
use tokio_prompt_orchestrator::{AnthropicWorker, spawn_pipeline};
use std::sync::Arc;

let worker = Arc::new(
    AnthropicWorker::new("claude-3-5-sonnet-20241022")
        .with_max_tokens(1024)
        .with_temperature(1.0)
);

let handles = spawn_pipeline(worker);
```

**Run:**
```bash
cargo run --example anthropic_worker
```

**Available Models:**
- `claude-3-5-sonnet-20241022` - Latest, most capable
- `claude-3-opus-20240229` - Highest intelligence
- `claude-3-sonnet-20240229` - Balanced
- `claude-3-haiku-20240307` - Fast and efficient
- See: https://docs.anthropic.com/claude/docs/models-overview

**Pricing (as of 2024):**
- Claude 3.5 Sonnet: $3/MTok (input), $15/MTok (output)
- Claude 3 Opus: $15/MTok (input), $75/MTok (output)
- Claude 3 Haiku: $0.25/MTok (input), $1.25/MTok (output)

---

### 3. llama.cpp (Local Inference)

**Setup:**

1. **Install llama.cpp:**
```bash
git clone https://github.com/ggerganov/llama.cpp
cd llama.cpp
make
```

2. **Download a model:**
```bash
# Example: Download Llama-2-7B-Chat GGUF
huggingface-cli download TheBloke/Llama-2-7B-Chat-GGUF llama-2-7b-chat.Q4_K_M.gguf --local-dir ./models
```

3. **Start server:**
```bash
./llama-cpp-server -m models/llama-2-7b-chat.Q4_K_M.gguf --port 8080 --n-gpu-layers 35
```

**Code:**
```rust
use tokio_prompt_orchestrator::{LlamaCppWorker, spawn_pipeline};
use std::sync::Arc;

let worker = Arc::new(
    LlamaCppWorker::new()
        .with_url("http://localhost:8080")
        .with_max_tokens(256)
        .with_temperature(0.8)
);

let handles = spawn_pipeline(worker);
```

**Run:**
```bash
cargo run --example llamacpp_worker
```

**Recommended Models:**
- **Small (7B)**: Llama-2-7B, Mistral-7B - Good for prototyping
- **Medium (13B)**: Llama-2-13B - Better quality
- **Large (70B+)**: Llama-2-70B, Mixtral-8x7B - Production quality

**Hardware Requirements:**
- 7B model: 8GB VRAM (Q4 quantization)
- 13B model: 16GB VRAM (Q4 quantization)
- 70B model: 48GB VRAM (Q4 quantization) or CPU with 64GB RAM

**Pros:**
- ✅ No API costs
- ✅ Data privacy (runs locally)
- ✅ No rate limits
- ✅ Full control

**Cons:**
- ❌ Requires hardware
- ❌ Slower than cloud APIs
- ❌ Manual model management

---

### 4. vLLM (High-Throughput Inference)

**Setup:**

1. **Install vLLM:**
```bash
pip install vllm
```

2. **Start server:**
```bash
python -m vllm.entrypoints.api_server \
  --model meta-llama/Llama-2-7b-chat-hf \
  --port 8000 \
  --dtype auto \
  --max-model-len 4096
```

For larger models:
```bash
# Mixtral 8x7B
python -m vllm.entrypoints.api_server \
  --model mistralai/Mixtral-8x7B-Instruct-v0.1 \
  --tensor-parallel-size 2 \
  --port 8000
```

**Code:**
```rust
use tokio_prompt_orchestrator::{VllmWorker, spawn_pipeline};
use std::sync::Arc;

let worker = Arc::new(
    VllmWorker::new()
        .with_url("http://localhost:8000")
        .with_max_tokens(512)
        .with_temperature(0.7)
        .with_top_p(0.95)
);

let handles = spawn_pipeline(worker);
```

**Run:**
```bash
cargo run --example vllm_worker
```

**Why vLLM?**
- ✅ **PagedAttention**: 2-4x throughput improvement
- ✅ **Continuous batching**: Efficient request handling
- ✅ **Tensor parallelism**: Multi-GPU support
- ✅ **Production-ready**: Used by Anthropic, Meta, etc.

**Performance:**
- 7B model: ~100 tokens/sec on A100
- 13B model: ~50 tokens/sec on A100
- 70B model: ~15 tokens/sec on 2xA100

**Best for:**
- High-throughput applications
- Serving multiple concurrent users
- Production deployments
- Cost optimization (on-prem)

---

## Advanced Patterns

### Worker Pool (Load Balancing)

```rust
use std::sync::Arc;
use tokio_prompt_orchestrator::{ModelWorker, OrchestratorError};

struct WorkerPool {
    workers: Vec<Arc<dyn ModelWorker>>,
    current: std::sync::atomic::AtomicUsize,
}

impl WorkerPool {
    fn new(workers: Vec<Arc<dyn ModelWorker>>) -> Self {
        Self {
            workers,
            current: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl ModelWorker for WorkerPool {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        // Round-robin selection
        let index = self.current.fetch_add(1, std::sync::atomic::Ordering::Relaxed) 
            % self.workers.len();
        self.workers[index].infer(prompt).await
    }
}

// Usage
let pool = Arc::new(WorkerPool::new(vec![
    Arc::new(VllmWorker::new().with_url("http://host1:8000")),
    Arc::new(VllmWorker::new().with_url("http://host2:8000")),
    Arc::new(VllmWorker::new().with_url("http://host3:8000")),
]));
```

### Fallback Chain (Reliability)

```rust
struct FallbackWorker {
    primary: Arc<dyn ModelWorker>,
    fallback: Arc<dyn ModelWorker>,
}

#[async_trait::async_trait]
impl ModelWorker for FallbackWorker {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        match self.primary.infer(prompt).await {
            Ok(tokens) => Ok(tokens),
            Err(e) => {
                tracing::warn!("Primary worker failed: {}, trying fallback", e);
                self.fallback.infer(prompt).await
            }
        }
    }
}

// Usage: Try local vLLM first, fallback to OpenAI
let worker = Arc::new(FallbackWorker {
    primary: Arc::new(VllmWorker::new()),
    fallback: Arc::new(OpenAiWorker::new("gpt-3.5-turbo-instruct")),
});
```

### Request Routing (A/B Testing)

```rust
struct RoutingWorker {
    worker_a: Arc<dyn ModelWorker>,
    worker_b: Arc<dyn ModelWorker>,
    a_percentage: f32, // 0.0 - 1.0
}

#[async_trait::async_trait]
impl ModelWorker for RoutingWorker {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        
        if rng.gen::<f32>() < self.a_percentage {
            self.worker_a.infer(prompt).await
        } else {
            self.worker_b.infer(prompt).await
        }
    }
}

// Usage: 80% GPT-4, 20% Claude
let worker = Arc::new(RoutingWorker {
    worker_a: Arc::new(OpenAiWorker::new("gpt-4")),
    worker_b: Arc::new(AnthropicWorker::new("claude-3-5-sonnet-20241022")),
    a_percentage: 0.8,
});
```

---

## Troubleshooting

### OpenAI Errors

**Error: "401 Unauthorized"**
```bash
# Check API key
echo $OPENAI_API_KEY

# Verify key is valid at https://platform.openai.com/api-keys
```

**Error: "429 Rate Limit"**
- You've exceeded your quota
- Upgrade plan or implement backoff
- Consider rate limiting in your app

### Anthropic Errors

**Error: "Authentication failed"**
```bash
# Check API key format (starts with sk-ant-)
echo $ANTHROPIC_API_KEY
```

**Error: "503 Service Unavailable"**
- Temporary API outage
- Implement retry logic with exponential backoff

### llama.cpp Errors

**Error: "Connection refused"**
```bash
# Check if server is running
curl http://localhost:8080/health

# Start server if not running
./llama-cpp-server -m models/your-model.gguf --port 8080
```

**Error: "Out of memory"**
- Model too large for GPU
- Use smaller quantization (Q4 instead of Q8)
- Offload fewer layers: `--n-gpu-layers 20`

### vLLM Errors

**Error: "CUDA out of memory"**
```bash
# Reduce max model length
python -m vllm.entrypoints.api_server \
  --model meta-llama/Llama-2-7b-chat-hf \
  --max-model-len 2048  # instead of 4096
```

**Error: "No module named 'vllm'"**
```bash
pip install vllm
# Or for CUDA 11.8:
pip install vllm==0.2.7
```

---

## Performance Tuning

### OpenAI/Anthropic (Cloud)

```rust
// Reduce latency
worker.with_max_tokens(256)  // Lower = faster
      .with_temperature(0.0)  // Deterministic = faster

// Increase timeout for complex queries
worker.with_timeout(Duration::from_secs(120))
```

### llama.cpp (Local)

```bash
# More GPU layers = faster
./llama-cpp-server -m model.gguf --n-gpu-layers 35

# Increase context (more memory, slower)
./llama-cpp-server -m model.gguf --ctx-size 4096

# Enable flash attention (faster, requires compatible GPU)
./llama-cpp-server -m model.gguf --flash-attn
```

### vLLM (Local)

```bash
# Tensor parallelism for large models
python -m vllm.entrypoints.api_server \
  --model meta-llama/Llama-2-70b-chat-hf \
  --tensor-parallel-size 4  # Use 4 GPUs

# Increase batch size
python -m vllm.entrypoints.api_server \
  --model meta-llama/Llama-2-7b-chat-hf \
  --max-num-batched-tokens 8192
```

---

## Production Checklist

- [ ] Set appropriate timeouts for your use case
- [ ] Implement retry logic for API failures
- [ ] Add circuit breakers for failing workers
- [ ] Monitor token usage and costs (cloud APIs)
- [ ] Set up alerts for high latency
- [ ] Implement request logging for debugging
- [ ] Add health checks for local workers
- [ ] Configure auto-restart for crashed servers
- [ ] Set resource limits (memory, CPU, GPU)
- [ ] Implement graceful degradation (fallbacks)

---

## Next Steps

1. **Try the examples:**
   ```bash
   cargo run --example openai_worker
   cargo run --example anthropic_worker
   cargo run --example llamacpp_worker
   cargo run --example vllm_worker
   cargo run --example multi_worker
   ```

2. **Read the API docs:**
   ```bash
   cargo doc --open
   ```

3. **Customize for your use case:**
   - Adjust channel sizes in `stages.rs`
   - Implement custom RAG stage
   - Add domain-specific post-processing

4. **Add monitoring:**
   - See `ARCHITECTURE.md` for metrics setup
   - Integrate Prometheus/Grafana
   - Set up alerting
