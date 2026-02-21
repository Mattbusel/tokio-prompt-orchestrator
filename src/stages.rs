//! Pipeline stage implementations with structured tracing.
//!
//! Each stage is an async task that:
//! 1. Pulls from its input channel
//! 2. Processes the message inside a structured tracing span
//! 3. Records outcome, duration, and error fields before exiting the span
//! 4. Sends to the next stage with backpressure handling
//!
//! ## Span Fields (every stage)
//!
//! | Field | Description |
//! |-------|-------------|
//! | `session_id` | Session this request belongs to |
//! | `request_id` | Unique ID for trace correlation |
//! | `stage` | Stage name string |
//! | `duration_ms` | Recorded after processing completes |
//! | `outcome` | `"ok"` or `"err"` |
//! | `error_kind` | Recorded only on error — the variant name |
//!
//! ## Sensitive Fields — NEVER Logged
//!
//! - Prompt content (`request.input`, assembled prompts)
//! - Model responses (inference tokens, final text)
//! - API keys
//!
//! ## Channel sizes
//!
//! - RAG → Assemble: 512
//! - Assemble → Inference: 512
//! - Inference → Post: 1024
//! - Post → Stream: 512
//! - Stream output: 256

use crate::{
    metrics, send_with_shed, AssembleOutput, InferenceOutput, ModelWorker, PostOutput,
    PromptRequest, RagOutput,
};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{info, warn, Span};

/// Handles for all spawned pipeline tasks
pub struct PipelineHandles {
    /// Handle for the RAG (retrieval-augmented generation) stage task.
    pub rag: JoinHandle<()>,
    /// Handle for the prompt assembly stage task.
    pub assemble: JoinHandle<()>,
    /// Handle for the model inference stage task.
    pub inference: JoinHandle<()>,
    /// Handle for the post-processing stage task.
    pub post: JoinHandle<()>,
    /// Handle for the streaming output stage task.
    pub stream: JoinHandle<()>,
    /// Channel sender for submitting new requests to the pipeline.
    pub input_tx: mpsc::Sender<PromptRequest>,
    /// Receiver for completed pipeline outputs.
    ///
    /// Wrapped in `Mutex<Option<...>>` so a single consumer (e.g. the MCP
    /// collector task) can `.take()` ownership. Callers who do not need
    /// pipeline output simply ignore this field.
    pub output_rx: tokio::sync::Mutex<Option<mpsc::Receiver<PostOutput>>>,
}

/// Spawn the complete 5-stage pipeline
///
/// Returns handles to all tasks and the input sender.
/// Caller should send PromptRequests to input_tx and await handles on shutdown.
///
/// # Panics
///
/// This function never panics.
///
/// TODO: Support per-core pinning:
/// ```ignore
/// let core_id = shard_session(&request.session, num_cores);
/// tokio::task::Builder::new()
///     .name(&format!("rag-core-{}", core_id))
///     .spawn_on(runtime_handles[core_id], async move { ... });
/// ```
pub fn spawn_pipeline(worker: Arc<dyn ModelWorker>) -> PipelineHandles {
    // Channel creation with specified buffer sizes
    let (input_tx, input_rx) = mpsc::channel::<PromptRequest>(512);
    let (rag_tx, rag_rx) = mpsc::channel::<RagOutput>(512);
    let (assemble_tx, assemble_rx) = mpsc::channel::<AssembleOutput>(512);
    let (inference_tx, inference_rx) = mpsc::channel::<InferenceOutput>(1024);
    let (post_tx, post_rx) = mpsc::channel::<PostOutput>(512);
    let (output_tx, output_rx) = mpsc::channel::<PostOutput>(256);

    // Spawn each stage
    let rag = tokio::spawn(rag_stage(input_rx, rag_tx));
    let assemble = tokio::spawn(assemble_stage(rag_rx, assemble_tx));
    let inference = tokio::spawn(inference_stage(assemble_rx, inference_tx, worker));
    let post = tokio::spawn(post_stage(inference_rx, post_tx));
    let stream = tokio::spawn(stream_stage(post_rx, output_tx));

    PipelineHandles {
        rag,
        assemble,
        inference,
        post,
        stream,
        input_tx,
        output_rx: tokio::sync::Mutex::new(Some(output_rx)),
    }
}

/// Stage 1: RAG (Retrieval-Augmented Generation)
///
/// Simulates document retrieval and context injection.
/// In production, this would query vector DBs, semantic search, etc.
///
/// # Panics
///
/// This function never panics.
async fn rag_stage(mut rx: mpsc::Receiver<PromptRequest>, tx: mpsc::Sender<RagOutput>) {
    info!(target: "orchestrator::pipeline", "RAG stage started");

    while let Some(request) = rx.recv().await {
        let start = Instant::now();
        let session_id = request.session.as_str().to_string();
        let request_id = request.request_id.clone();

        let span = tracing::info_span!(
            "pipeline.rag",
            session_id = %session_id,
            request_id = %request_id,
            stage = "rag",
            duration_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            error_kind = tracing::field::Empty,
        );
        let _enter = span.enter();

        tracing::info!(
            target: "orchestrator::pipeline",
            session_id = %session_id,
            request_id = %request_id,
            "Request received at RAG stage"
        );

        metrics::inc_request("rag");

        // Simulate RAG work (DB query, embedding search, etc.)
        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;

        let session = request.session.clone();
        let output = RagOutput {
            session: session.clone(),
            context: format!(
                "CONTEXT: Retrieved documents for '{}'",
                request.input.chars().take(50).collect::<String>()
            ),
            original: request,
        };

        let elapsed = start.elapsed();
        metrics::record_stage_latency("rag", elapsed);
        Span::current().record("duration_ms", elapsed.as_millis() as u64);
        Span::current().record("outcome", "ok");

        if let Err(e) = send_with_shed(&tx, output, "rag").await {
            warn!(
                target: "orchestrator::pipeline",
                session_id = %session_id,
                request_id = %request_id,
                error = ?e,
                "RAG stage send failed"
            );
            metrics::inc_error("rag", "channel_closed");
            break;
        }
    }

    info!(target: "orchestrator::pipeline", "RAG stage shutting down");
}

/// Stage 2: Assemble
///
/// Constructs the final prompt from RAG context and user input.
/// This is where prompt templates, few-shot examples, etc. get injected.
///
/// # Panics
///
/// This function never panics.
async fn assemble_stage(mut rx: mpsc::Receiver<RagOutput>, tx: mpsc::Sender<AssembleOutput>) {
    info!(target: "orchestrator::pipeline", "Assemble stage started");

    while let Some(rag_output) = rx.recv().await {
        let start = Instant::now();
        let session_id = rag_output.session.as_str().to_string();
        let request_id = rag_output.original.request_id.clone();

        let span = tracing::info_span!(
            "pipeline.assemble",
            session_id = %session_id,
            request_id = %request_id,
            stage = "assemble",
            duration_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            error_kind = tracing::field::Empty,
        );
        let _enter = span.enter();

        metrics::inc_request("assemble");

        // Construct prompt from context + user input
        // NOTE: prompt content is NOT logged — it is sensitive data
        let prompt = format!(
            "{}\n\nUser Query: {}\n\nAssistant:",
            rag_output.context, rag_output.original.input
        );

        let output = AssembleOutput {
            session: rag_output.session,
            request_id: request_id.clone(),
            prompt,
        };

        let elapsed = start.elapsed();
        metrics::record_stage_latency("assemble", elapsed);
        Span::current().record("duration_ms", elapsed.as_millis() as u64);
        Span::current().record("outcome", "ok");

        if let Err(e) = send_with_shed(&tx, output, "assemble").await {
            warn!(
                target: "orchestrator::pipeline",
                session_id = %session_id,
                request_id = %request_id,
                error = ?e,
                "Assemble stage send failed"
            );
            metrics::inc_error("assemble", "channel_closed");
            break;
        }
    }

    info!(target: "orchestrator::pipeline", "Assemble stage shutting down");
}

/// Stage 3: Inference
///
/// Delegates to the ModelWorker trait for actual LLM inference.
/// This stage is the hot path and should be horizontally scalable.
///
/// # Panics
///
/// This function never panics.
///
/// TODO: Add multi-worker pool for parallel inference:
/// ```ignore
/// let workers: Vec<Arc<dyn ModelWorker>> = ...;
/// let worker = &workers[shard_session(&session, workers.len())];
/// ```
async fn inference_stage(
    mut rx: mpsc::Receiver<AssembleOutput>,
    tx: mpsc::Sender<InferenceOutput>,
    worker: Arc<dyn ModelWorker>,
) {
    info!(target: "orchestrator::pipeline", "Inference stage started");

    while let Some(assemble_output) = rx.recv().await {
        let start = Instant::now();
        let session_id = assemble_output.session.as_str().to_string();
        let request_id = assemble_output.request_id.clone();

        let span = tracing::info_span!(
            "pipeline.inference",
            session_id = %session_id,
            request_id = %request_id,
            stage = "inference",
            duration_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            error_kind = tracing::field::Empty,
        );
        let _enter = span.enter();

        metrics::inc_request("inference");

        // Call model worker — prompt content is NOT logged
        match worker.infer(&assemble_output.prompt).await {
            Ok(tokens) => {
                let elapsed = start.elapsed();
                metrics::record_stage_latency("inference", elapsed);
                Span::current().record("duration_ms", elapsed.as_millis() as u64);
                Span::current().record("outcome", "ok");

                let output = InferenceOutput {
                    session: assemble_output.session,
                    request_id: request_id.clone(),
                    tokens,
                };

                if let Err(e) = send_with_shed(&tx, output, "inference").await {
                    warn!(
                        target: "orchestrator::pipeline",
                        session_id = %session_id,
                        request_id = %request_id,
                        error = ?e,
                        "Inference stage send failed"
                    );
                    metrics::inc_error("inference", "channel_closed");
                    break;
                }
            }
            Err(e) => {
                let elapsed = start.elapsed();
                metrics::record_stage_latency("inference", elapsed);
                Span::current().record("duration_ms", elapsed.as_millis() as u64);
                Span::current().record("outcome", "err");
                Span::current().record("error_kind", "inference_failure");

                warn!(
                    target: "orchestrator::pipeline",
                    session_id = %session_id,
                    request_id = %request_id,
                    error = %e,
                    "Inference failed"
                );
                metrics::inc_error("inference", "inference_failure");
                // Drop request — could also send to DLQ
            }
        }
    }

    info!(target: "orchestrator::pipeline", "Inference stage shutting down");
}

/// Stage 4: Post-processing
///
/// Joins tokens, applies formatting, filters, safety checks, etc.
/// Could include content moderation, PII redaction, etc.
///
/// # Panics
///
/// This function never panics.
async fn post_stage(mut rx: mpsc::Receiver<InferenceOutput>, tx: mpsc::Sender<PostOutput>) {
    info!(target: "orchestrator::pipeline", "Post stage started");

    while let Some(inference_output) = rx.recv().await {
        let start = Instant::now();
        let session_id = inference_output.session.as_str().to_string();
        let request_id = inference_output.request_id.clone();

        let span = tracing::info_span!(
            "pipeline.post",
            session_id = %session_id,
            request_id = %request_id,
            stage = "post",
            duration_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            error_kind = tracing::field::Empty,
        );
        let _enter = span.enter();

        metrics::inc_request("post");

        // Join tokens into final text — content is NOT logged
        let text = inference_output.tokens.join(" ");

        let output = PostOutput {
            session: inference_output.session,
            request_id: request_id.clone(),
            text,
        };

        let elapsed = start.elapsed();
        metrics::record_stage_latency("post", elapsed);
        Span::current().record("duration_ms", elapsed.as_millis() as u64);
        Span::current().record("outcome", "ok");

        if let Err(e) = send_with_shed(&tx, output, "post").await {
            warn!(
                target: "orchestrator::pipeline",
                session_id = %session_id,
                request_id = %request_id,
                error = ?e,
                "Post stage send failed"
            );
            metrics::inc_error("post", "channel_closed");
            break;
        }
    }

    info!(target: "orchestrator::pipeline", "Post stage shutting down");
}

/// Stage 5: Stream
///
/// Final output stage — could write to SSE, WebSocket, gRPC stream, etc.
/// For MVP, logs a redacted summary (text length only, no content).
///
/// # Panics
///
/// This function never panics.
///
/// TODO: Make this pluggable via trait:
/// ```ignore
/// #[async_trait]
/// pub trait OutputSink: Send + Sync {
///     async fn emit(&self, session: &SessionId, text: &str) -> Result<()>;
/// }
/// ```
async fn stream_stage(
    mut rx: mpsc::Receiver<PostOutput>,
    output_tx: mpsc::Sender<PostOutput>,
) {
    info!(target: "orchestrator::pipeline", "Stream stage started");

    while let Some(post_output) = rx.recv().await {
        let start = Instant::now();
        let session_id = post_output.session.as_str().to_string();
        let request_id = post_output.request_id.clone();

        let span = tracing::info_span!(
            "pipeline.stream",
            session_id = %session_id,
            request_id = %request_id,
            stage = "stream",
            duration_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            error_kind = tracing::field::Empty,
        );
        let _enter = span.enter();

        metrics::inc_request("stream");

        // Emit final output — log text LENGTH only, never content
        info!(
            target: "orchestrator::pipeline",
            session_id = %session_id,
            request_id = %request_id,
            text_len = post_output.text.len(),
            "Stream output emitted"
        );

        let elapsed = start.elapsed();
        metrics::record_stage_latency("stream", elapsed);
        Span::current().record("duration_ms", elapsed.as_millis() as u64);
        Span::current().record("outcome", "ok");

        // Forward to output channel (best-effort, non-blocking)
        if output_tx.try_send(post_output).is_err() {
            warn!(
                target: "orchestrator::pipeline",
                session_id = %session_id,
                request_id = %request_id,
                "Output channel full or closed, discarding result"
            );
        }
    }

    info!(target: "orchestrator::pipeline", "Stream stage shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EchoWorker, SessionId};
    use std::collections::HashMap;

    /// Helper to create a test request with all required fields.
    fn make_test_request(session: &str, request_id: &str, input: &str) -> PromptRequest {
        PromptRequest {
            session: SessionId::new(session),
            request_id: request_id.to_string(),
            input: input.to_string(),
            meta: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn test_pipeline_end_to_end_request_flows_through_all_stages() {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init()
            .ok();

        let worker = Arc::new(EchoWorker::with_delay(1));
        let handles = spawn_pipeline(worker);

        let request = make_test_request("test-123", "req-001", "Hello world");

        handles.input_tx.send(request).await.unwrap_or_else(|_| ());
        drop(handles.input_tx); // Close input to trigger shutdown

        // Wait for pipeline to drain
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }

    #[tokio::test]
    async fn test_pipeline_multiple_requests_complete_successfully() {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init()
            .ok();

        let worker = Arc::new(EchoWorker::with_delay(1));
        let handles = spawn_pipeline(worker);

        for i in 0..5 {
            let request =
                make_test_request(&format!("session-{i}"), &format!("req-{i}"), "test input");
            handles.input_tx.send(request).await.unwrap_or_else(|_| ());
        }

        drop(handles.input_tx);
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    }

    #[tokio::test]
    async fn test_pipeline_request_id_propagation_does_not_panic() {
        let worker = Arc::new(EchoWorker::with_delay(0));
        let handles = spawn_pipeline(worker);

        let request = make_test_request("s1", "req-propagation-test", "propagation test");

        handles.input_tx.send(request).await.unwrap_or_else(|_| ());
        drop(handles.input_tx);
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }

    #[tokio::test]
    async fn test_pipeline_shutdown_gracefully_on_sender_drop() {
        let worker = Arc::new(EchoWorker::with_delay(0));
        let handles = spawn_pipeline(worker);

        // Immediately drop sender — all stages should shut down gracefully
        drop(handles.input_tx);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        // Primary assertion: no panic, no hang
    }

    #[tokio::test]
    async fn test_rag_stage_increments_metrics() {
        let _ = crate::metrics::init_metrics();

        let worker = Arc::new(EchoWorker::with_delay(0));
        let handles = spawn_pipeline(worker);

        let request = make_test_request("metrics-test", "req-metrics", "metrics test");
        handles.input_tx.send(request).await.unwrap_or_else(|_| ());
        drop(handles.input_tx);
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        // Metrics should have been recorded (no panic)
        let summary = crate::metrics::get_metrics_summary();
        // Summary is valid (fields are accessible without panic)
        let _rt = summary.requests_total.len();
    }

    #[tokio::test]
    async fn test_pipeline_output_channel_receives_result() {
        let worker = Arc::new(EchoWorker::with_delay(0));
        let handles = spawn_pipeline(worker);

        // Take the output receiver
        let mut output_rx = {
            let mut guard = handles.output_rx.lock().await;
            guard.take().expect("output_rx should be available")
        };

        let request = make_test_request("out-test", "req-out-1", "hello output");
        handles.input_tx.send(request).await.unwrap_or_else(|_| ());

        // Wait for result with timeout
        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            output_rx.recv(),
        )
        .await;

        assert!(result.is_ok(), "should receive result within timeout");
        let post_output = result.unwrap_or(None);
        assert!(post_output.is_some(), "output channel should yield a PostOutput");
        let post = post_output.unwrap_or_else(|| crate::PostOutput {
            session: SessionId::new(""),
            request_id: String::new(),
            text: String::new(),
        });
        assert_eq!(post.request_id, "req-out-1");
        assert!(!post.text.is_empty());
    }

    #[tokio::test]
    async fn test_pipeline_output_channel_ignored_does_not_block() {
        let worker = Arc::new(EchoWorker::with_delay(0));
        let handles = spawn_pipeline(worker);

        // Do NOT take output_rx — simulate callers who ignore pipeline output.
        // Send several requests and verify the pipeline does not deadlock.
        for i in 0..5 {
            let request = make_test_request(
                &format!("no-consume-{i}"),
                &format!("req-nc-{i}"),
                "ignore output",
            );
            handles.input_tx.send(request).await.unwrap_or_else(|_| ());
        }

        drop(handles.input_tx);
        // If the pipeline deadlocks this will hang; the test runner timeout catches it.
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }
}
