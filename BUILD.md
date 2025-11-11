# Build & Verification Guide

## Prerequisites

Ensure you have Rust 1.79+ installed:

```bash
rustc --version
# Should show: rustc 1.79.0 or higher
```

If not installed:
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
```

## Build Steps

### 1. Navigate to Project
```bash
cd tokio-prompt-orchestrator
```

### 2. Check Dependencies
```bash
cargo check
```

Expected output:
```
    Checking tokio v1.40.0
    Checking tracing v0.1.x
    Checking tokio-prompt-orchestrator v0.1.0
    Finished dev [unoptimized + debuginfo] target(s) in X.XXs
```

### 3. Run Tests
```bash
cargo test
```

Expected output:
```
running 3 tests
test tests::test_shard_session_deterministic ... ok
test tests::test_shard_session_distribution ... ok
test tests::test_echo_worker ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured
```

### 4. Build Release Binary
```bash
cargo build --release
```

Binary will be at: `target/release/orchestrator-demo`

### 5. Run Demo
```bash
cargo run --bin orchestrator-demo
```

Expected output:
```
INFO tokio_prompt_orchestrator: 🚀 Starting tokio-prompt-orchestrator demo
INFO tokio_prompt_orchestrator: ✅ Pipeline stages spawned
INFO tokio_prompt_orchestrator::stages: RAG stage started
INFO tokio_prompt_orchestrator::stages: Assemble stage started
INFO tokio_prompt_orchestrator::stages: Inference stage started
INFO tokio_prompt_orchestrator::stages: Post stage started
INFO tokio_prompt_orchestrator::stages: Stream stage started
INFO tokio_prompt_orchestrator: 📨 Sending 10 demo requests
INFO tokio_prompt_orchestrator::stages: 📤 STREAM OUTPUT: CONTEXT: Retrieved documents for 'What is the capital of France?' User Query: What is the capital of France? Assistant: What is the capital of France?
...
INFO tokio_prompt_orchestrator: ✅ All requests sent
INFO tokio_prompt_orchestrator: ⏳ Waiting for pipeline to drain...
INFO tokio_prompt_orchestrator: 🏁 Demo complete - shutting down
```

## Code Quality Checks

### Linting
```bash
cargo clippy -- -D warnings
```

Should show no warnings.

### Formatting
```bash
cargo fmt --check
```

To auto-format:
```bash
cargo fmt
```

### Documentation
```bash
cargo doc --no-deps --open
```

Opens generated API docs in browser.

## Performance Profiling

### Build with Debug Info
```bash
cargo build --release --profile=release-with-debug
```

### Run with Tokio Console (if needed)
```toml
# Add to Cargo.toml
[dependencies]
console-subscriber = "0.1"
```

```rust
// In main.rs
console_subscriber::init();
```

```bash
tokio-console
```

## Troubleshooting

### Issue: `cargo` command not found
**Solution**: Install Rust toolchain (see Prerequisites)

### Issue: Compilation errors
**Solution**: Ensure Rust 1.79+
```bash
rustup update
rustup default stable
```

### Issue: Tests fail
**Solution**: Check test output for specific failures
```bash
cargo test -- --nocapture
```

### Issue: Demo runs but no output
**Solution**: Increase log level
```bash
RUST_LOG=debug cargo run --bin orchestrator-demo
```

## Verification Checklist

- [ ] `cargo check` passes
- [ ] `cargo test` passes (3/3 tests)
- [ ] `cargo clippy` shows no warnings
- [ ] `cargo build --release` succeeds
- [ ] Demo runs and shows 10 requests processed
- [ ] All 5 stages start and complete
- [ ] No panics or errors in output

## Project Structure Verification

Ensure all files exist:

```bash
# Core files
ls -1 | grep -E '(Cargo.toml|README.md|LICENSE)'

# Documentation
ls -1 | grep -E '(ARCHITECTURE|QUICKSTART|SUMMARY)'

# Source files
ls -1 src/ | grep -E '(lib|main|metrics|stages|worker).rs'
```

Expected file count:
- 8 markdown/toml files
- 5 Rust source files
- Total: 13 files

## Size Check

```bash
find . -name "*.rs" -exec wc -l {} + | tail -1
```

Expected: ~600-800 lines of Rust code

```bash
du -sh .
```

Expected: ~100KB total (excluding target/)

## Integration Testing

### Test 1: Single Request
```rust
#[tokio::test]
async fn test_single_request() {
    let worker = Arc::new(EchoWorker::new());
    let handles = spawn_pipeline(worker);
    
    let request = PromptRequest {
        session: SessionId::new("test"),
        input: "test".to_string(),
        meta: HashMap::new(),
    };
    
    handles.input_tx.send(request).await.unwrap();
    drop(handles.input_tx);
    
    tokio::time::sleep(Duration::from_millis(200)).await;
    // Pipeline should complete without errors
}
```

### Test 2: High Load
```rust
#[tokio::test]
async fn test_high_load() {
    let worker = Arc::new(EchoWorker::with_delay(1));
    let handles = spawn_pipeline(worker);
    
    for i in 0..1000 {
        let request = PromptRequest {
            session: SessionId::new(format!("test-{}", i)),
            input: "test".to_string(),
            meta: HashMap::new(),
        };
        handles.input_tx.try_send(request).ok();
    }
    
    drop(handles.input_tx);
    tokio::time::sleep(Duration::from_secs(2)).await;
}
```

## Deployment Checklist

Before production deployment:

- [ ] All tests pass
- [ ] No clippy warnings
- [ ] Documentation complete
- [ ] Metrics backend configured (not just tracing)
- [ ] Error handling reviewed
- [ ] Load testing completed
- [ ] Resource limits set (channel sizes tuned)
- [ ] Monitoring/alerting configured
- [ ] Rollback plan prepared
- [ ] Real ModelWorker implementation (not EchoWorker)

## Next Steps

1. **Implement Real Worker**
   - Replace EchoWorker with vLLM/llama.cpp/OpenAI
   - Test with actual model inference

2. **Add Metrics Backend**
   - Enable Prometheus feature
   - Configure scrape endpoint
   - Set up Grafana dashboards

3. **Production Deploy**
   - Containerize with Docker
   - Deploy to Kubernetes
   - Configure auto-scaling

4. **Monitor & Tune**
   - Watch queue depths
   - Adjust channel sizes
   - Optimize worker pool

## Support

For issues or questions:
- Read ARCHITECTURE.md for design details
- Check QUICKSTART.md for usage examples
- Review PROJECT_SUMMARY.md for complete features
- Consult inline code comments for TODOs

---
✅ Project is ready for development and deployment!
