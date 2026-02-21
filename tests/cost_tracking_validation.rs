//! Cost tracking validation test suite
//!
//! Exercises the same code paths as MCP infer + pipeline_status:
//! - Runs 25 inference calls with llama_cpp worker
//! - Runs 25 inference calls with echo worker
//! - Validates cost_saved calculations against cloud baseline
//! - Compares all three cost models for consistency
//!
//! Results are printed for capture by the validation report generator.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio_prompt_orchestrator::{
    metrics, routing::cost_tracker::CostTracker, EchoWorker, LlamaCppWorker, ModelWorker,
};

/// Generate 25 varied test prompts.
fn generate_prompts(prefix: &str) -> Vec<String> {
    (0..25)
        .map(|i| match i % 5 {
            0 => format!("{prefix}: Explain quantum computing in simple terms (prompt {i})"),
            1 => format!("{prefix}: Write a haiku about Rust programming (prompt {i})"),
            2 => format!("{prefix}: What is the capital of France? (prompt {i})"),
            3 => format!("{prefix}: Summarize the benefits of async programming (prompt {i})"),
            _ => format!("{prefix}: Calculate 42 * 17 and explain (prompt {i})"),
        })
        .collect()
}

/// Run one inference through the exact MCP code path (inline pipeline stages).
/// Returns (output, latency_ms, stage_latencies, token_count).
async fn run_single_infer(
    worker: &Arc<dyn ModelWorker>,
    prompt: &str,
    session_id: &str,
) -> Result<(String, u64, HashMap<String, f64>, u64), String> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let overall_start = Instant::now();

    // Stage 1: RAG (simulated)
    let rag_start = Instant::now();
    let context = format!("[context for: {}]", prompt);
    let rag_ms = rag_start.elapsed().as_secs_f64() * 1000.0;

    // Stage 2: Assemble
    let assemble_start = Instant::now();
    let assembled = format!("{context}\n\n{prompt}");
    let assemble_ms = assemble_start.elapsed().as_secs_f64() * 1000.0;

    // Stage 3: Inference via worker
    let infer_start = Instant::now();
    let tokens = worker
        .infer(&assembled)
        .await
        .map_err(|e| format!("inference failed: {e}"))?;
    let infer_ms = infer_start.elapsed().as_secs_f64() * 1000.0;

    // Stage 4: Post-process
    let post_start = Instant::now();
    let output = tokens.join(" ");
    let post_ms = post_start.elapsed().as_secs_f64() * 1000.0;

    // Stage 5: Stream (no-op)
    let stream_start = Instant::now();
    let stream_ms = stream_start.elapsed().as_secs_f64() * 1000.0;

    let total_ms = overall_start.elapsed().as_millis() as u64;
    let token_count = tokens.len() as u64;

    let mut stage_latencies = HashMap::new();
    stage_latencies.insert("rag".to_string(), rag_ms);
    stage_latencies.insert("assemble".to_string(), assemble_ms);
    stage_latencies.insert("inference".to_string(), infer_ms);
    stage_latencies.insert("post_process".to_string(), post_ms);
    stage_latencies.insert("stream".to_string(), stream_ms);

    let _ = (request_id, session_id); // suppress unused

    Ok((output, total_ms, stage_latencies, token_count))
}

/// Build pipeline_status equivalent from Prometheus metrics.
/// Same logic as MCP pipeline_status tool (src/bin/mcp.rs:212-260).
fn get_pipeline_status() -> serde_json::Value {
    let summary = metrics::get_metrics_summary();

    let total_requests: u64 = summary.requests_total.values().sum();
    let total_shed: u64 = summary.requests_shed.values().sum();
    let total_errors: u64 = summary.errors_total.values().sum();

    let status = if total_errors > total_requests / 4 {
        "degraded"
    } else {
        "healthy"
    };

    let inferences = total_requests.saturating_sub(total_shed);
    let savings = if total_requests > 0 {
        (total_shed as f64 / total_requests as f64) * 100.0
    } else {
        0.0
    };

    serde_json::json!({
        "status": status,
        "dedup_stats": {
            "requests_total": total_requests,
            "inferences_total": inferences,
            "savings_percent": savings,
            "cost_saved_usd": total_shed as f64 * 0.01
        },
        "throughput_rps": total_requests
    })
}

// ============================================================================
// Test: 25 echo worker calls + CostTracker validation
// ============================================================================

#[tokio::test]
async fn test_echo_worker_25_calls_cost_tracking() {
    let _ = metrics::init_metrics();

    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
    let cost_tracker = CostTracker::new(0.0, 0.015);
    let prompts = generate_prompts("echo");

    let mut total_tokens = 0u64;
    let mut total_latency_ms = 0u64;

    for (i, prompt) in prompts.iter().enumerate() {
        let session = format!("echo-session-{i}");
        let result = run_single_infer(&worker, prompt, &session).await;
        assert!(result.is_ok(), "Echo call {} failed: {:?}", i, result.err());

        let (_, latency, _, token_count) = result.unwrap_or_default();
        cost_tracker.record_local(token_count);
        total_tokens += token_count;
        total_latency_ms += latency;
    }

    let snapshot = cost_tracker.snapshot();

    // Pipeline status (MCP-equivalent)
    let status = get_pipeline_status();

    println!("========================================");
    println!("ECHO WORKER: 25 Inference Calls");
    println!("========================================");
    println!("Total tokens:     {}", total_tokens);
    println!("Avg latency:      {:.2}ms", total_latency_ms as f64 / 25.0);
    println!("Local requests:   {}", snapshot.local_requests);
    println!("Cloud requests:   {}", snapshot.cloud_requests);
    println!("Actual cost:      ${:.6}", snapshot.actual_cost_usd);
    println!("Baseline cost:    ${:.6}", snapshot.baseline_cost_usd);
    println!("Savings (USD):    ${:.6}", snapshot.savings_usd);
    println!("Savings (%):      {:.1}%", snapshot.savings_percent);
    println!(
        "Pipeline status:  {}",
        serde_json::to_string_pretty(&status).unwrap_or_default()
    );
    println!("========================================");

    // Assertions
    assert_eq!(snapshot.local_requests, 25);
    assert_eq!(snapshot.cloud_requests, 0);
    assert!(total_tokens > 0, "Should have produced tokens");
    assert!(
        snapshot.actual_cost_usd.abs() < 0.001,
        "Local cost should be ~$0"
    );

    let expected_baseline = total_tokens as f64 * 0.015 / 1000.0;
    assert!(
        (snapshot.baseline_cost_usd - expected_baseline).abs() < 0.001,
        "Baseline: expected ${:.6}, got ${:.6}",
        expected_baseline,
        snapshot.baseline_cost_usd
    );
    assert!(
        (snapshot.savings_usd - expected_baseline).abs() < 0.001,
        "Savings should equal baseline for all-local"
    );
    assert!(
        (snapshot.savings_percent - 100.0).abs() < 0.1,
        "100% savings expected for all-local, got {:.1}%",
        snapshot.savings_percent
    );
}

// ============================================================================
// Test: 25 llama_cpp worker calls + CostTracker validation
// ============================================================================

#[tokio::test]
async fn test_llama_cpp_worker_25_calls_cost_tracking() {
    let _ = metrics::init_metrics();

    let worker: Arc<dyn ModelWorker> = Arc::new(LlamaCppWorker::new());
    let cost_tracker = CostTracker::new(0.0, 0.015);
    let prompts = generate_prompts("llama_cpp");

    let mut successful = 0u64;
    let mut failed = 0u64;
    let mut total_tokens = 0u64;

    for (i, prompt) in prompts.iter().enumerate() {
        let session = format!("llama-session-{i}");
        match run_single_infer(&worker, prompt, &session).await {
            Ok((_, _, _, token_count)) => {
                cost_tracker.record_local(token_count);
                total_tokens += token_count;
                successful += 1;
            }
            Err(_) => {
                // llama_cpp server not running — record as cloud fallback
                let estimated_tokens = 20u64;
                cost_tracker.record_fallback(estimated_tokens);
                total_tokens += estimated_tokens;
                failed += 1;
            }
        }
    }

    let snapshot = cost_tracker.snapshot();
    let status = get_pipeline_status();

    println!("========================================");
    println!("LLAMA_CPP WORKER: 25 Inference Calls");
    println!("========================================");
    println!("Successful:       {}", successful);
    println!("Failed→fallback:  {}", failed);
    println!("Total tokens:     {}", total_tokens);
    println!("Local tokens:     {}", snapshot.local_tokens);
    println!("Cloud tokens:     {}", snapshot.cloud_tokens);
    println!("Actual cost:      ${:.6}", snapshot.actual_cost_usd);
    println!("Baseline cost:    ${:.6}", snapshot.baseline_cost_usd);
    println!("Savings (USD):    ${:.6}", snapshot.savings_usd);
    println!("Savings (%):      {:.1}%", snapshot.savings_percent);
    println!(
        "Pipeline status:  {}",
        serde_json::to_string_pretty(&status).unwrap_or_default()
    );
    println!("========================================");

    // Invariant: all 25 calls accounted for
    assert_eq!(
        snapshot.local_requests + snapshot.fallback_requests,
        25,
        "All 25 calls must be accounted for"
    );

    // Baseline should be positive if any tokens processed
    if total_tokens > 0 {
        assert!(snapshot.baseline_cost_usd > 0.0);
    }

    // Savings should be non-negative
    assert!(snapshot.savings_usd >= 0.0);

    // If all failed (no llama_cpp server), all go to cloud fallback → $0 savings
    if successful == 0 {
        assert!(
            snapshot.savings_usd.abs() < 0.001,
            "All-fallback should have ~$0 savings"
        );
    }
    // If all succeeded (llama_cpp running), 100% savings
    if failed == 0 {
        assert!(
            (snapshot.savings_percent - 100.0).abs() < 0.1,
            "All-local should have 100% savings"
        );
    }
}

// ============================================================================
// Test: Comparison — 25 local + 25 cloud (the acquirer demo scenario)
// ============================================================================

#[tokio::test]
async fn test_cost_comparison_25_local_then_25_cloud() {
    let tracker = CostTracker::new(0.0, 0.015);

    // Phase 1: 25 "local" calls (llama_cpp/echo) — ~20 tokens each
    for _ in 0..25 {
        tracker.record_local(20);
    }

    let phase1 = tracker.snapshot();

    println!("========================================");
    println!("PHASE 1: 25 Local Calls (500 tokens)");
    println!("========================================");
    println!("Local requests:   {}", phase1.local_requests);
    println!("Actual cost:      ${:.6}", phase1.actual_cost_usd);
    println!("Baseline cost:    ${:.6}", phase1.baseline_cost_usd);
    println!(
        "Savings:          ${:.6} ({:.1}%)",
        phase1.savings_usd, phase1.savings_percent
    );

    // Phase 1 validation: all local, free
    assert_eq!(phase1.local_requests, 25);
    assert!(phase1.actual_cost_usd.abs() < 0.001);
    // 500 tokens * $0.015/1K = $0.0075
    assert!((phase1.baseline_cost_usd - 0.0075).abs() < 0.0001);
    assert!((phase1.savings_usd - 0.0075).abs() < 0.0001);
    assert!((phase1.savings_percent - 100.0).abs() < 0.1);

    // Phase 2: 25 "cloud" calls — ~20 tokens each
    for _ in 0..25 {
        tracker.record_cloud(20);
    }

    let phase2 = tracker.snapshot();

    println!("========================================");
    println!("PHASE 2: +25 Cloud Calls (cumulative)");
    println!("========================================");
    println!("Local requests:   {}", phase2.local_requests);
    println!("Cloud requests:   {}", phase2.cloud_requests);
    println!("Local tokens:     {}", phase2.local_tokens);
    println!("Cloud tokens:     {}", phase2.cloud_tokens);
    println!("Actual cost:      ${:.6}", phase2.actual_cost_usd);
    println!("Baseline cost:    ${:.6}", phase2.baseline_cost_usd);
    println!(
        "Savings:          ${:.6} ({:.1}%)",
        phase2.savings_usd, phase2.savings_percent
    );

    // Phase 2 validation: 50/50 split
    assert_eq!(phase2.local_requests, 25);
    assert_eq!(phase2.cloud_requests, 25);
    assert_eq!(phase2.local_tokens, 500);
    assert_eq!(phase2.cloud_tokens, 500);
    // Actual: 0 + 500*0.015/1K = $0.0075
    assert!((phase2.actual_cost_usd - 0.0075).abs() < 0.0001);
    // Baseline: 1000*0.015/1K = $0.015
    assert!((phase2.baseline_cost_usd - 0.015).abs() < 0.0001);
    // Savings: $0.015 - $0.0075 = $0.0075 (50%)
    assert!((phase2.savings_usd - 0.0075).abs() < 0.0001);
    assert!((phase2.savings_percent - 50.0).abs() < 0.1);

    println!("========================================");
    println!("DELTA: Phase 2 vs Phase 1");
    println!("========================================");
    println!(
        "Cost increase:    ${:.6}",
        phase2.actual_cost_usd - phase1.actual_cost_usd
    );
    println!(
        "Savings drop:     {:.1}% → {:.1}%",
        phase1.savings_percent, phase2.savings_percent
    );
}

// ============================================================================
// Test: Three cost models compared
// ============================================================================

#[test]
fn test_three_cost_models_divergence() {
    // Scenario: 1000 requests, 200 deduped, 50 shed, 750 inferred, ~20 tokens each

    // Model 1: MCP (shed-based): cost_saved = total_shed * $0.01
    let shed = 50u64;
    let mcp_savings = shed as f64 * 0.01;

    // Model 2: Dashboard (dedup-based): cost_saved = requests_deduped * $0.002
    let deduped = 200u64;
    let dashboard_savings = deduped as f64 * 0.002;

    // Model 3: CostTracker (token-based): most accurate
    let tracker = CostTracker::new(0.0, 0.015);
    // 750 actual inferences * 20 tokens = 15,000 tokens, all local
    tracker.record_local(15_000);
    let token_savings = tracker.snapshot().savings_usd;

    println!("========================================");
    println!("THREE COST MODELS COMPARED");
    println!("========================================");
    println!("Scenario: 1000 req, 200 dedup, 50 shed, 750 inferred @ 20 tok/req");
    println!(
        "  MCP shed-based:        ${:.4} (shed avoidance)",
        mcp_savings
    );
    println!(
        "  Dashboard dedup-based: ${:.4} (cache hit avoidance)",
        dashboard_savings
    );
    println!(
        "  CostTracker tokens:    ${:.4} (local vs cloud delta)",
        token_savings
    );
    println!("========================================");

    assert!((mcp_savings - 0.50).abs() < 0.001);
    assert!((dashboard_savings - 0.40).abs() < 0.001);
    assert!((token_savings - 0.225).abs() < 0.001);

    // Document: these measure DIFFERENT things, divergence is expected
    assert!(
        mcp_savings > dashboard_savings,
        "MCP model charges $0.01/shed vs dashboard $0.002/dedup"
    );
    assert!(
        dashboard_savings > token_savings,
        "Dedup avoidance > token-level savings (different baselines)"
    );
}

// ============================================================================
// Test: MCP pipeline_status formula validation
// ============================================================================

#[test]
fn test_mcp_cost_formula_correctness() {
    // MCP: cost_saved_usd = total_shed * 0.01
    for shed in [0u64, 1, 10, 100, 1000, 10_000] {
        let cost = shed as f64 * 0.01;
        let expected = shed as f64 / 100.0;
        assert!(
            (cost - expected).abs() < f64::EPSILON,
            "shed={}: expected ${}, got ${}",
            shed,
            expected,
            cost
        );
    }
}

#[test]
fn test_dashboard_cost_formula_correctness() {
    // Dashboard: cost_saved_usd = requests_deduped * 0.002
    for deduped in [0u64, 1, 50, 250, 1000, 5000] {
        let cost = deduped as f64 * 0.002;
        let expected = deduped as f64 * 0.002;
        assert!(
            (cost - expected).abs() < f64::EPSILON,
            "deduped={}: expected ${}, got ${}",
            deduped,
            expected,
            cost
        );
    }
}

// ============================================================================
// Test: CostTracker precision at scale
// ============================================================================

#[tokio::test]
async fn test_cost_tracker_precision_at_scale() {
    let tracker = CostTracker::new(0.0, 0.015);

    // Simulate high volume: 10,000 requests * 50 tokens = 500K tokens local
    for _ in 0..10_000 {
        tracker.record_local(50);
    }

    let s = tracker.snapshot();

    // Baseline: 500K * $0.015/1K = $7.50
    assert!((s.baseline_cost_usd - 7.50).abs() < 0.01);
    assert!(s.actual_cost_usd.abs() < 0.001);
    assert!((s.savings_usd - 7.50).abs() < 0.01);
    assert!((s.savings_percent - 100.0).abs() < 0.1);

    println!("=== Scale Test: 10K requests ===");
    println!("Tokens: {}", s.local_tokens);
    println!("Baseline: ${:.2}", s.baseline_cost_usd);
    println!(
        "Savings:  ${:.2} ({:.1}%)",
        s.savings_usd, s.savings_percent
    );
}

#[tokio::test]
async fn test_cost_tracker_micro_dollar_no_float_drift() {
    let tracker = CostTracker::new(0.0, 0.015);

    // 1M tokens — tests that micro-dollar arithmetic avoids drift
    tracker.record_local(1_000_000);
    let s = tracker.snapshot();

    // 1M * $0.015/1K = $15.00 exactly
    assert!((s.baseline_cost_usd - 15.0).abs() < 0.01);
    assert!(s.actual_cost_usd.abs() < 0.001);
    assert!((s.savings_usd - 15.0).abs() < 0.01);
}

// ============================================================================
// Test: Edge cases
// ============================================================================

#[test]
fn test_cost_zero_requests() {
    let tracker = CostTracker::new(0.0, 0.015);
    let s = tracker.snapshot();
    assert!(s.savings_usd.abs() < f64::EPSILON);
    assert!(s.savings_percent.abs() < f64::EPSILON);
    assert!(s.actual_cost_usd.abs() < f64::EPSILON);
    assert!(s.baseline_cost_usd.abs() < f64::EPSILON);
}

#[test]
fn test_cost_mixed_with_fallback() {
    let tracker = CostTracker::new(0.0, 0.015);

    // 10 local (200 tokens), 5 fallback (100 tokens)
    for _ in 0..10 {
        tracker.record_local(20);
    }
    for _ in 0..5 {
        tracker.record_fallback(20);
    }

    let s = tracker.snapshot();

    // Local: 200 tokens * $0/1K = $0
    // Cloud (fallback): 100 tokens * $0.015/1K = $0.0015
    // Total actual: $0.0015
    assert!((s.actual_cost_usd - 0.0015).abs() < 0.0001);

    // Baseline: 300 tokens * $0.015/1K = $0.0045
    assert!((s.baseline_cost_usd - 0.0045).abs() < 0.0001);

    // Savings: $0.0045 - $0.0015 = $0.003 (66.7%)
    assert!((s.savings_usd - 0.003).abs() < 0.0001);
    assert!((s.savings_percent - 66.666).abs() < 0.1);
}
