//! Example: Prometheus Metrics Collection
//!
//! Demonstrates real-time metrics collection with Prometheus.
//!
//! Run with:
//! ```bash
//! cargo run --example metrics_demo --features metrics-server
//! ```
//!
//! Then visit:
//! - http://localhost:9090/metrics - Prometheus metrics
//! - http://localhost:9090/health - Health check
//!
//! Or scrape with Prometheus:
//! ```yaml
//! scrape_configs:
//!   - job_name: 'orchestrator'
//!     static_configs:
//!       - targets: ['localhost:9090']
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_prompt_orchestrator::{
    spawn_pipeline, EchoWorker, ModelWorker, PromptRequest, SessionId,
};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("📊 Prometheus Metrics Demo");
    info!("");
    info!("Endpoints:");
    info!("  📈 Metrics: http://localhost:9090/metrics");
    info!("  ❤️  Health:  http://localhost:9090/health");
    info!("");

    // Start metrics server (non-blocking)
    #[cfg(feature = "metrics-server")]
    let metrics_handle = tokio::spawn(async {
        if let Err(e) = tokio_prompt_orchestrator::metrics_server::start_server("0.0.0.0:9090").await {
            tracing::error!("Metrics server error: {}", e);
        }
    });

    #[cfg(not(feature = "metrics-server"))]
    {
        info!("⚠️  Metrics server not enabled. Build with:");
        info!("   cargo run --example metrics_demo --features metrics-server");
        info!("");
    }

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Create worker pool with different delays (simulate different performance)
    let workers: Vec<Arc<dyn ModelWorker>> = vec![
        Arc::new(EchoWorker::with_delay(10)),
        Arc::new(EchoWorker::with_delay(20)),
        Arc::new(EchoWorker::with_delay(15)),
    ];

    info!("✅ Created worker pool with {} workers", workers.len());

    // Start multiple pipelines
    let mut handles_vec = Vec::new();
    for (i, worker) in workers.into_iter().enumerate() {
        let handles = spawn_pipeline(worker);
        handles_vec.push((i, handles));
    }

    info!("✅ Spawned {} pipelines", handles_vec.len());
    info!("");
    info!("📨 Sending requests to generate metrics...");

    // Send requests continuously
    let request_count = 50;
    let mut request_num = 0;

    for pipeline_id in 0..3 {
        let (_, handles) = &handles_vec[pipeline_id];
        
        for i in 0..request_count {
            let request = PromptRequest {
                session: SessionId::new(format!("session-{}", i % 10)),
                input: format!("Request {} to pipeline {}", i, pipeline_id),
                meta: {
                    let mut meta = HashMap::new();
                    meta.insert("pipeline_id".to_string(), pipeline_id.to_string());
                    meta.insert("request_num".to_string(), i.to_string());
                    meta
                },
            };

            if handles.input_tx.send(request).await.is_ok() {
                request_num += 1;
                
                if request_num % 10 == 0 {
                    info!("  Sent {} requests...", request_num);
                }
            }

            // Small delay between requests
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    info!("");
    info!("✅ Sent {} total requests across {} pipelines", request_num, handles_vec.len());
    info!("");
    info!("⏳ Processing requests... (metrics updating in real-time)");

    // Close all pipelines
    for (_, handles) in handles_vec {
        drop(handles.input_tx);
    }

    // Wait for processing
    tokio::time::sleep(Duration::from_secs(5)).await;

    info!("");
    info!("📊 Metrics Summary:");
    info!("════════════════════");

    // Print metrics summary
    let summary = tokio_prompt_orchestrator::metrics::get_metrics_summary();
    
    info!("");
    info!("Requests Processed:");
    for (stage, count) in &summary.requests_total {
        info!("  {} stage: {} requests", stage, count);
    }

    if !summary.requests_shed.is_empty() {
        info!("");
        info!("Requests Shed (backpressure):");
        for (stage, count) in &summary.requests_shed {
            if *count > 0 {
                info!("  {} stage: {} shed", stage, count);
            }
        }
    }

    if !summary.errors_total.is_empty() {
        info!("");
        info!("Errors:");
        for ((stage, error_type), count) in &summary.errors_total {
            info!("  {} stage ({}): {} errors", stage, error_type, count);
        }
    }

    info!("");
    info!("════════════════════");
    info!("");
    info!("🌐 Metrics Server Running");
    info!("");
    info!("View metrics at:");
    info!("  curl http://localhost:9090/metrics");
    info!("");
    info!("Sample metrics you'll see:");
    info!("  • orchestrator_requests_total{{stage=\"inference\"}} 150");
    info!("  • orchestrator_stage_duration_seconds{{stage=\"rag\"}} ...");
    info!("  • orchestrator_queue_depth{{stage=\"inference\"}} 5");
    info!("");
    info!("Press Ctrl+C to stop...");
    info!("");

    // Keep running to allow metrics scraping
    #[cfg(feature = "metrics-server")]
    {
        tokio::signal::ctrl_c().await?;
        info!("Shutting down...");
        metrics_handle.abort();
    }

    #[cfg(not(feature = "metrics-server"))]
    {
        info!("Metrics collected (but server not available without --features metrics-server)");
    }

    Ok(())
}
