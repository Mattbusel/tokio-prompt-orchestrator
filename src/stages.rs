//! Pipeline stage implementations
//!
//! Each stage is an async task that:
//! 1. Pulls from its input channel
//! 2. Processes the message with tracing spans
//! 3. Sends to the next stage with backpressure handling
//!
//! Channel sizes:
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
use tracing::{info, instrument, warn};

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
}

/// Spawn the complete 5-stage pipeline
///
/// Returns handles to all tasks and the input sender.
/// Caller should send PromptRequests to input_tx and await handles on shutdown.
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

    // Spawn each stage
    let rag = tokio::spawn(rag_stage(input_rx, rag_tx));
    let assemble = tokio::spawn(assemble_stage(rag_rx, assemble_tx));
    let inference = tokio::spawn(inference_stage(assemble_rx, inference_tx, worker));
    let post = tokio::spawn(post_stage(inference_rx, post_tx));
    let stream = tokio::spawn(stream_stage(post_rx));

    PipelineHandles {
        rag,
        assemble,
        inference,
        post,
        stream,
        input_tx,
    }
}

/// Stage 1: RAG (Retrieval-Augmented Generation)
///
/// Simulates document retrieval and context injection.
/// In production, this would query vector DBs, semantic search, etc.
#[instrument(skip_all, name = "rag_stage")]
async fn rag_stage(mut rx: mpsc::Receiver<PromptRequest>, tx: mpsc::Sender<RagOutput>) {
    info!("RAG stage started");

    while let Some(request) = rx.recv().await {
        let start = Instant::now();
        let session = request.session.clone();

        let span = tracing::span!(
            tracing::Level::DEBUG,
            "rag_process",
            session = session.as_str()
        );
        let _enter = span.enter();

        // Simulate RAG work (DB query, embedding search, etc.)
        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;

        let output = RagOutput {
            session: session.clone(),
            context: format!(
                "CONTEXT: Retrieved documents for '{}'",
                request.input.chars().take(50).collect::<String>()
            ),
            original: request,
        };

        metrics::record_stage_latency("rag", start.elapsed());

        if let Err(e) = send_with_shed(&tx, output, "rag").await {
            warn!(error = ?e, "RAG stage send failed");
            break;
        }
    }

    info!("RAG stage shutting down");
}

/// Stage 2: Assemble
///
/// Constructs the final prompt from RAG context and user input.
/// This is where prompt templates, few-shot examples, etc. get injected.
#[instrument(skip_all, name = "assemble_stage")]
async fn assemble_stage(mut rx: mpsc::Receiver<RagOutput>, tx: mpsc::Sender<AssembleOutput>) {
    info!("Assemble stage started");

    while let Some(rag_output) = rx.recv().await {
        let start = Instant::now();
        let session = rag_output.session.clone();

        let span = tracing::span!(
            tracing::Level::DEBUG,
            "assemble_process",
            session = session.as_str()
        );
        let _enter = span.enter();

        // Construct prompt from context + user input
        let prompt = format!(
            "{}\n\nUser Query: {}\n\nAssistant:",
            rag_output.context, rag_output.original.input
        );

        let output = AssembleOutput { session, prompt };

        metrics::record_stage_latency("assemble", start.elapsed());

        if let Err(e) = send_with_shed(&tx, output, "assemble").await {
            warn!(error = ?e, "Assemble stage send failed");
            break;
        }
    }

    info!("Assemble stage shutting down");
}

/// Stage 3: Inference
///
/// Delegates to the ModelWorker trait for actual LLM inference.
/// This stage is the hot path and should be horizontally scalable.
///
/// TODO: Add multi-worker pool for parallel inference:
/// ```ignore
/// let workers: Vec<Arc<dyn ModelWorker>> = ...;
/// let worker = &workers[shard_session(&session, workers.len())];
/// ```
#[instrument(skip_all, name = "inference_stage")]
async fn inference_stage(
    mut rx: mpsc::Receiver<AssembleOutput>,
    tx: mpsc::Sender<InferenceOutput>,
    worker: Arc<dyn ModelWorker>,
) {
    info!("Inference stage started");

    while let Some(assemble_output) = rx.recv().await {
        let start = Instant::now();
        let session = assemble_output.session.clone();

        let span = tracing::span!(
            tracing::Level::DEBUG,
            "inference_process",
            session = session.as_str()
        );
        let _enter = span.enter();

        // Call model worker
        match worker.infer(&assemble_output.prompt).await {
            Ok(tokens) => {
                let output = InferenceOutput { session, tokens };

                metrics::record_stage_latency("inference", start.elapsed());

                if let Err(e) = send_with_shed(&tx, output, "inference").await {
                    warn!(error = ?e, "Inference stage send failed");
                    break;
                }
            }
            Err(e) => {
                warn!(
                    session = session.as_str(),
                    error = ?e,
                    "Inference failed"
                );
                // Drop request - could also send to DLQ
            }
        }
    }

    info!("Inference stage shutting down");
}

/// Stage 4: Post-processing
///
/// Joins tokens, applies formatting, filters, safety checks, etc.
/// Could include content moderation, PII redaction, etc.
#[instrument(skip_all, name = "post_stage")]
async fn post_stage(mut rx: mpsc::Receiver<InferenceOutput>, tx: mpsc::Sender<PostOutput>) {
    info!("Post stage started");

    while let Some(inference_output) = rx.recv().await {
        let start = Instant::now();
        let session = inference_output.session.clone();

        let span = tracing::span!(
            tracing::Level::DEBUG,
            "post_process",
            session = session.as_str()
        );
        let _enter = span.enter();

        // Join tokens into final text
        let text = inference_output.tokens.join(" ");

        let output = PostOutput { session, text };

        metrics::record_stage_latency("post", start.elapsed());

        if let Err(e) = send_with_shed(&tx, output, "post").await {
            warn!(error = ?e, "Post stage send failed");
            break;
        }
    }

    info!("Post stage shutting down");
}

/// Stage 5: Stream
///
/// Final output stage - could write to SSE, WebSocket, gRPC stream, etc.
/// For MVP, just logs the result.
///
/// TODO: Make this pluggable via trait:
/// ```ignore
/// #[async_trait]
/// pub trait OutputSink: Send + Sync {
///     async fn emit(&self, session: &SessionId, text: &str) -> Result<()>;
/// }
/// ```
#[instrument(skip_all, name = "stream_stage")]
async fn stream_stage(mut rx: mpsc::Receiver<PostOutput>) {
    info!("Stream stage started");

    while let Some(post_output) = rx.recv().await {
        let start = Instant::now();

        let span = tracing::span!(
            tracing::Level::DEBUG,
            "stream_emit",
            session = post_output.session.as_str()
        );
        let _enter = span.enter();

        // Emit final output (currently just logs)
        info!(
            session = post_output.session.as_str(),
            text_len = post_output.text.len(),
            "📤 STREAM OUTPUT: {}",
            post_output.text.chars().take(100).collect::<String>()
        );

        metrics::record_stage_latency("stream", start.elapsed());
    }

    info!("Stream stage shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EchoWorker, SessionId};
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_pipeline_end_to_end() {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init()
            .ok();

        let worker = Arc::new(EchoWorker::with_delay(1));
        let handles = spawn_pipeline(worker);

        // Send a test request
        let request = PromptRequest {
            session: SessionId::new("test-123"),
            input: "Hello world".to_string(),
            meta: HashMap::new(),
        };

        handles.input_tx.send(request).await.unwrap();
        drop(handles.input_tx); // Close input to trigger shutdown

        // Wait for pipeline to drain
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
}
