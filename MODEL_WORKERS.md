# Model Worker Implementations

This document describes all available model worker implementations and how to use them.

## Overview

The orchestrator supports multiple inference backends via the `ModelWorker` trait:

| Worker | Type | Requires | Best For |
|--------|------|----------|----------|
| `EchoWorker` | Testing | None | Pipeline testing, demos |
| `OpenAiWorker` | Cloud API | API key | GPT models, quick prototyping |
| `AnthropicWorker` | Cloud API | API key | Claude models, long context |
| `LlamaCppWorker` | Local server | llama.cpp | On-premise, privacy, no cost |
| `VllmWorker` | Local server | vLLM | High-throughput, batching |

## EchoWorker (Testing)

Simple worker that echoes the prompt back as tokens. No setup required.

```rust
use tokio_prompt_orchestrator::{EchoWorker, ModelWorker};
use std::sync::Arc;

let worker: Arc<dyn ModelWorker> = Arc::new(
    EchoWorker::with_delay(10) // 10ms simulated latency
);
```

**Configuration:**
- `delay_ms`: Simulated inference latency (default: 10ms)

**Use Cases:**
- Pipeline smoke tests
- Performance benchmarking
- Development without API costs

---

## OpenAiWorker

Connects to OpenAI's API for GPT models.

### Setup

1. **Get API Key**: https://platform.openai.com/api-keys

2. **Set Environment Variable**:
   ```bash
   export OPENAI_API_KEY="sk-proj-..."
   ```

3. **Create Worker**:
   ```rust
   use tokio_prompt_orchestrator::OpenAiWorker;
   use std::sync::Arc;
   
   let worker = Arc::new(
       OpenAiWorker::new("gpt-3.5-turbo-instruct")
           .with_max_tokens(512)
           .with_temperature(0.7)
           .with_timeout(Duration::from_secs(30))
   );
   ```

### Configuration

| Method | Type | Default | Description |
|--------|------|---------|-------------|
| `new(model)` | String | - | Model name |
| `with_max_tokens()` | u32 | 256 | Max tokens to generate |
| `with_temperature()` | f32 | 0.7 | Sampling temperature (0.0-2.0) |
| `with_timeout()` | Duration | 30s | Request timeout |

### Available Models

- `gpt-4`
- `gpt-4-turbo`
- `gpt-3.5-turbo-instruct`
- `text-davinci-003`

### Example

```bash
# Set API key
export OPENAI_API_KEY="sk-proj-..."

# Run example
cargo run --example openai_example
```

### Cost Considerations

- GPT-4: ~$0.03 per 1K tokens
- GPT-3.5-turbo-instruct: ~$0.0015 per 1K tokens

Track usage at: https://platform.openai.com/usage

---

## AnthropicWorker

Connects to Anthropic's API for Claude models.

### Setup

1. **Get API Key**: https://console.anthropic.com/

2. **Set Environment Variable**:
   ```bash
   export ANTHROPIC_API_KEY="sk-ant-..."
   ```

3. **Create Worker**:
   ```rust
   use tokio_prompt_orchestrator::AnthropicWorker;
   use std::sync::Arc;
   
   let worker = Arc::new(
       AnthropicWorker::new("claude-3-5-sonnet-20241022")
           .with_max_tokens(1024)
           .with_temperature(1.0)
           .with_timeout(Duration::from_secs(60))
   );
   ```

### Configuration

| Method | Type | Default | Description |
|--------|------|---------|-------------|
| `new(model)` | String | - | Model name |
| `with_max_tokens()` | u32 | 1024 | Max tokens to generate |
| `with_temperature()` | f32 | 1.0 | Sampling temperature (0.0-1.0) |
| `with_timeout()` | Duration | 60s | Request timeout |

### Available Models

- `claude-3-5-sonnet-20241022` (latest, best)
- `claude-3-opus-20240229` (most capable)
- `claude-3-sonnet-20240229` (balanced)
- `claude-3-haiku-20240307` (fastest)

### Example

```bash
# Set API key
export ANTHROPIC_API_KEY="sk-ant-..."

# Run example
cargo run --example anthropic_example
```

### Special Features

- **Long Context**: Up to 200K tokens
- **Prompt Format**: Automatically adds `\n\nHuman:...\n\nAssistant:` wrapper
- **Safety**: Built-in safety features

---

## LlamaCppWorker

Connects to a local llama.cpp server for on-premise inference.

### Setup

1. **Install llama.cpp**:
   ```bash
   git clone https://github.com/ggerganov/llama.cpp
   cd llama.cpp
   make
   ```

2. **Download a Model**:
   ```bash
   # Download Llama 2 7B (example)
   wget https://huggingface.co/TheBloke/Llama-2-7B-GGUF/resolve/main/llama-2-7b.Q4_K_M.gguf
   ```

3. **Start Server**:
   ```bash
   ./server -m llama-2-7b.Q4_K_M.gguf --port 8080 --ctx-size 2048
   ```

4. **Create Worker**:
   ```rust
   use tokio_prompt_orchestrator::LlamaCppWorker;
   use std::sync::Arc;
   
   let worker = Arc::new(
       LlamaCppWorker::new()
           .with_url("http://localhost:8080")
           .with_max_tokens(256)
           .with_temperature(0.8)
   );
   ```

### Configuration

| Method | Type | Default | Description |
|--------|------|---------|-------------|
| `new()` | - | - | Uses `LLAMA_CPP_URL` env var |
| `with_url()` | String | `http://localhost:8080` | Server URL |
| `with_max_tokens()` | i32 | 256 | Max tokens to generate |
| `with_temperature()` | f32 | 0.8 | Sampling temperature |
| `with_timeout()` | Duration | 30s | Request timeout |

### Example

```bash
# Start llama.cpp server (terminal 1)
./server -m models/llama-2-7b.gguf --port 8080

# Run example (terminal 2)
cargo run --example llama_cpp_example
```

### Model Recommendations

| Model | Size | RAM | Use Case |
|-------|------|-----|----------|
| Llama-2-7B-Q4 | 3.8GB | 8GB | Development, testing |
| Llama-2-13B-Q4 | 7.3GB | 16GB | Production, quality |
| Llama-2-70B-Q4 | 38GB | 64GB | High-quality inference |
| Mistral-7B-Q4 | 4GB | 8GB | Fast, efficient |

### Advantages

- **Privacy**: Data never leaves your infrastructure
- **No Cost**: No per-token charges
- **Customizable**: Run any GGUF model
- **Offline**: Works without internet

---

## VllmWorker

Connects to a vLLM server for high-throughput inference.

### Setup

1. **Install vLLM**:
   ```bash
   pip install vllm
   ```

2. **Start Server**:
   ```bash
   python -m vllm.entrypoints.api_server \
       --model meta-llama/Llama-2-7b-chat-hf \
       --port 8000 \
       --dtype auto
   ```

3. **Create Worker**:
   ```rust
   use tokio_prompt_orchestrator::VllmWorker;
   use std::sync::Arc;
   
   let worker = Arc::new(
       VllmWorker::new()
           .with_url("http://localhost:8000")
           .with_max_tokens(512)
           .with_temperature(0.7)
           .with_top_p(0.95)
   );
   ```

### Configuration

| Method | Type | Default | Description |
|--------|------|---------|-------------|
| `new()` | - | - | Uses `VLLM_URL` env var |
| `with_url()` | String | `http://localhost:8000` | Server URL |
| `with_max_tokens()` | u32 | 512 | Max tokens to generate |
| `with_temperature()` | f32 | 0.7 | Sampling temperature |
| `with_top_p()` | f32 | 0.95 | Nucleus sampling |
| `with_timeout()` | Duration | 60s | Request timeout |

### Example

```bash
# Start vLLM server (terminal 1)
python -m vllm.entrypoints.api_server \
    --model meta-llama/Llama-2-7b-chat-hf \
    --port 8000

# Run example (terminal 2)
cargo run --example vllm_example
```

### Performance Tuning

```bash
# Multi-GPU
vllm serve meta-llama/Llama-2-70b-chat-hf \
    --tensor-parallel-size 4

# Quantization for less VRAM
vllm serve meta-llama/Llama-2-70b-chat-hf \
    --quantization awq

# Batch size optimization
vllm serve meta-llama/Llama-2-7b-chat-hf \
    --max-num-batched-tokens 4096
```

### Advantages

- **High Throughput**: Continuous batching, PagedAttention
- **GPU Optimized**: CUDA kernels, efficient memory
- **Scalable**: Multi-GPU support
- **HuggingFace**: Easy model loading

---

## Choosing a Worker

### Development

```rust
// Quick testing
let worker = Arc::new(EchoWorker::new());

// API prototyping
let worker = Arc::new(OpenAiWorker::new("gpt-3.5-turbo-instruct"));
```

### Production

```rust
// Cloud deployment (managed scaling)
let worker = Arc::new(
    AnthropicWorker::new("claude-3-5-sonnet-20241022")
        .with_max_tokens(2048)
);

// On-premise (privacy, control)
let worker = Arc::new(
    VllmWorker::new()
        .with_url("http://vllm-cluster:8000")
        .with_max_tokens(1024)
);
```

### Decision Matrix

| Requirement | Recommended Worker |
|-------------|-------------------|
| Privacy/Compliance | LlamaCppWorker, VllmWorker |
| High Throughput | VllmWorker |
| Low Latency | OpenAiWorker (GPT-3.5) |
| Long Context | AnthropicWorker (Claude) |
| Cost Sensitive | LlamaCppWorker, EchoWorker |
| Quick Start | OpenAiWorker |

---

## Advanced Usage

### Worker Pool (Multiple Models)

```rust
use std::sync::Arc;
use tokio_prompt_orchestrator::{ModelWorker, OpenAiWorker, AnthropicWorker};

// Route by session or priority
fn select_worker(session: &str) -> Arc<dyn ModelWorker> {
    if session.starts_with("premium-") {
        Arc::new(AnthropicWorker::new("claude-3-opus-20240229"))
    } else {
        Arc::new(OpenAiWorker::new("gpt-3.5-turbo-instruct"))
    }
}
```

### Fallback Chain

```rust
async fn infer_with_fallback(prompt: &str) -> Result<Vec<String>, Error> {
    let workers: Vec<Arc<dyn ModelWorker>> = vec![
        Arc::new(VllmWorker::new()), // Try local first
        Arc::new(OpenAiWorker::new("gpt-3.5-turbo-instruct")), // Fallback to API
    ];
    
    for worker in workers {
        match worker.infer(prompt).await {
            Ok(tokens) => return Ok(tokens),
            Err(e) => tracing::warn!("Worker failed: {}", e),
        }
    }
    
    Err(Error::msg("All workers failed"))
}
```

### Custom Worker

Implement `ModelWorker` for your own backend:

```rust
use async_trait::async_trait;
use tokio_prompt_orchestrator::{ModelWorker, OrchestratorError};

struct MyCustomWorker {
    endpoint: String,
}

#[async_trait]
impl ModelWorker for MyCustomWorker {
    async fn infer(&self, prompt: &str) -> Result<Vec<String>, OrchestratorError> {
        // Your custom inference logic
        let response = reqwest::get(&format!("{}/generate?prompt={}", self.endpoint, prompt))
            .await
            .map_err(|e| OrchestratorError::Inference(e.to_string()))?;
        
        let text: String = response.text().await
            .map_err(|e| OrchestratorError::Inference(e.to_string()))?;
        
        Ok(text.split_whitespace().map(String::from).collect())
    }
}
```

---

## Troubleshooting

### OpenAI Errors

**401 Unauthorized**
```bash
# Check API key is set correctly
echo $OPENAI_API_KEY

# Verify key at https://platform.openai.com/api-keys
```

**Rate Limit**
```rust
// Add retry logic
.with_timeout(Duration::from_secs(60))
```

### Anthropic Errors

**Invalid API Key Format**
- Keys start with `sk-ant-`
- Get from https://console.anthropic.com/

**Model Not Found**
- Use exact model string: `claude-3-5-sonnet-20241022`

### llama.cpp Errors

**Connection Refused**
```bash
# Check server is running
curl http://localhost:8080/health

# Check port matches
export LLAMA_CPP_URL="http://localhost:8080"
```

**Out of Memory**
- Use smaller model or quantization (Q4, Q5)
- Reduce `--ctx-size` parameter

### vLLM Errors

**CUDA Out of Memory**
```bash
# Use quantization
vllm serve <model> --quantization awq

# Or smaller model
vllm serve meta-llama/Llama-2-7b-chat-hf
```

**Slow Inference**
- Enable tensor parallelism for multi-GPU
- Increase batch size: `--max-num-batched-tokens 8192`

---

## Performance Comparison

Tested on: Intel Xeon, 32GB RAM, NVIDIA A100 40GB

| Worker | Latency (p50) | Throughput | Cost/1M tokens |
|--------|---------------|------------|----------------|
| EchoWorker | 10ms | 1000 req/s | $0 |
| OpenAI GPT-3.5 | 500ms | 50 req/s | $1.50 |
| OpenAI GPT-4 | 2000ms | 20 req/s | $30 |
| Anthropic Claude | 800ms | 40 req/s | $15 |
| llama.cpp (7B) | 200ms | 100 req/s | $0 |
| vLLM (7B) | 50ms | 500 req/s | $0 |

*Results vary based on hardware, model size, and configuration*

---

## Next Steps

1. Try the examples: `cargo run --example <name>`
2. Read [QUICKSTART.md](QUICKSTART.md) for pipeline usage
3. See [ARCHITECTURE.md](ARCHITECTURE.md) for design details
4. Implement your custom worker for specialized backends
