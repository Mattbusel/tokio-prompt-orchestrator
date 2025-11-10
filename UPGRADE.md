# Upgrade to v2 - Real Model Integrations

## What Changed

**Version 2** adds production-ready model workers while maintaining 100% backward compatibility.

## Quick Upgrade

### Option 1: Download New Version

1. Download: [tokio-prompt-orchestrator-v2.tar.gz](computer:///mnt/user-data/outputs/tokio-prompt-orchestrator-v2.tar.gz)
2. Extract and rebuild:
   ```bash
   tar -xzf tokio-prompt-orchestrator-v2.tar.gz
   cd tokio-prompt-orchestrator
   cargo build
   ```

### Option 2: In-Place Update

If you're on Windows and already have the project:

1. Download the new version
2. Copy these new files to your existing project:
   - `src/worker.rs` (updated)
   - `examples/*.rs` (4 new examples)
   - `MODEL_WORKERS.md` (new)
   - `WHATS_NEW.md` (new)

3. Update `Cargo.toml`:
   ```toml
   [dependencies]
   # Add these three lines:
   reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
   serde = { version = "1.0", features = ["derive"] }
   serde_json = "1.0"
   ```

4. Rebuild:
   ```powershell
   cargo build
   ```

## Verify Upgrade

```bash
# Should still work (backward compatible)
cargo run --bin orchestrator-demo

# Try new OpenAI worker (requires API key)
$env:OPENAI_API_KEY="sk-proj-..."
cargo run --example openai_example
```

## What You Get

### New Workers (4)
-  OpenAiWorker - GPT models
-  AnthropicWorker - Claude models
-  LlamaCppWorker - Local inference
-  VllmWorker - GPU inference

### New Examples (4)
- `examples/openai_example.rs`
- `examples/anthropic_example.rs`
- `examples/llama_cpp_example.rs`
- `examples/vllm_example.rs`

### New Documentation
- **MODEL_WORKERS.md** - 650-line comprehensive guide
- **WHATS_NEW.md** - Feature summary
- Updated README with worker info

## Breaking Changes

**None!** Version 2 is 100% backward compatible.

All existing code continues to work:
```rust
// v1 code still works in v2
let worker = Arc::new(EchoWorker::new());
let handles = spawn_pipeline(worker);
```

## Migration Examples

### Before (v1)
```rust
use tokio_prompt_orchestrator::{EchoWorker, ModelWorker, spawn_pipeline};

let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
let handles = spawn_pipeline(worker);
```

### After (v2 - Optional)
```rust
use tokio_prompt_orchestrator::{OpenAiWorker, ModelWorker, spawn_pipeline};

// Just swap the worker - everything else stays the same!
let worker: Arc<dyn ModelWorker> = Arc::new(
    OpenAiWorker::new("gpt-3.5-turbo-instruct")
        .with_max_tokens(512)
);
let handles = spawn_pipeline(worker);
```

## New Dependencies

v2 adds 3 dependencies for HTTP/JSON:

| Crate | Purpose | Size |
|-------|---------|------|
| `reqwest` | HTTP client | ~500KB |
| `serde` | Serialization | ~100KB |
| `serde_json` | JSON support | ~50KB |

Total addition: ~650KB to binary size.

## Testing After Upgrade

```bash
# 1. Verify tests still pass
cargo test

# 2. Run original demo
cargo run --bin orchestrator-demo

# 3. Try a new example (if you have API key)
$env:OPENAI_API_KEY="sk-..."
cargo run --example openai_example

# 4. Check compilation
cargo clippy
```

All should work without errors.

## Troubleshooting

### "reqwest" not found

**Cause**: Didn't update Cargo.toml

**Fix**: Add dependencies to `Cargo.toml`:
```toml
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
```

### OpenAI example fails to compile

**Cause**: Old worker.rs file

**Fix**: Replace `src/worker.rs` with the new version

### Can't find examples

**Cause**: New example files not copied

**Fix**: Copy `examples/*.rs` from v2 package

## Rollback

If you need to roll back to v1:

1. Use the old archive: `tokio-prompt-orchestrator.tar.gz`
2. Or remove the 3 new dependencies from Cargo.toml
3. Revert `src/worker.rs` to v1 version

## Next Steps

1. Read [MODEL_WORKERS.md](MODEL_WORKERS.md) for worker documentation
2. Get an API key (OpenAI or Anthropic)
3. Run an example: `cargo run --example openai_example`
4. Integrate real models into your pipeline!

## Questions?

- Setup: See MODEL_WORKERS.md
- API keys: Check environment variables
- Errors: See WHATS_NEW.md troubleshooting
- Examples: All in `examples/` directory

**Enjoy real LLM inference!** 
