//! # Example: the smallest pipeline
//!
//! The same code as "Use the library" in the README: send one prompt through
//! the five stages with the offline `EchoWorker` (no API key), print the
//! answer, then print anything the dead-letter queue caught.
//!
//! ```bash
//! cargo run --example quickstart
//! ```
//!
//! Features needed: none.

use std::{collections::HashMap, sync::Arc};
use tokio_prompt_orchestrator::{
    spawn_pipeline, EchoWorker, ModelWorker, PromptRequest, SessionId,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Swap EchoWorker for AnthropicWorker, OpenAiWorker, LlamaCppWorker or VllmWorker.
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
    let handles = spawn_pipeline(worker);
    let mut output = handles
        .take_output_rx()
        .await
        .ok_or("output already taken")?;

    handles
        .input_tx
        .send(PromptRequest {
            session: SessionId::new("demo"),
            request_id: "req-1".into(),
            input: "Hello, pipeline!".into(),
            meta: HashMap::new(),
            deadline: None,
        })
        .await?;

    let answer = output.recv().await.ok_or("pipeline closed")?;
    println!("{}", answer.text);

    for dropped in handles.dlq.drain() {
        println!("dropped {}: {}", dropped.request_id, dropped.reason);
    }
    Ok(())
}
