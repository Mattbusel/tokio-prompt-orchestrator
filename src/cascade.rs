//! Cascading multi-turn inference engine.
//!
//! Enables multi-turn reasoning loops where a model's output can trigger
//! additional pipeline passes — tool calls, function execution, chain-of-thought
//! continuation — until a termination condition is met.
//!
//! ## Architecture
//!
//! ```text
//! PromptRequest
//!       │
//!       ▼
//! ┌─────────────────────────────────────────┐
//! │          CascadeEngine                  │
//! │                                         │
//! │  Turn 1: infer → parse tools            │
//! │  Turn 2: execute tools → infer again    │
//! │  Turn N: model returns DONE / no tools  │
//! └─────────────────────────────────────────┘
//!       │
//!       ▼
//! CascadeResult (all turns + final answer)
//! ```
//!
//! ## Termination Conditions
//!
//! The loop exits when ANY of the following is true:
//! - The model output contains no tool calls (`ToolCallParser` finds nothing)
//! - The output text contains the DONE sentinel string
//! - `max_turns` is reached (default: 10)
//! - A pipeline error occurs (propagated to caller)
//!
//! ## Tool Call Format
//!
//! The engine recognises tool calls in this JSON block format:
//!
//! ```text
//! <tool_call>
//! {"name": "search", "arguments": {"query": "rust tokio"}}
//! </tool_call>
//! ```
//!
//! Custom parsers can be registered via [`CascadeEngine::with_tool_parser`].

use crate::{OrchestratorError, PromptRequest, SessionId};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// Default maximum number of cascade turns before halting.
pub const DEFAULT_MAX_TURNS: usize = 10;

/// Sentinel string that signals the model is done cascading.
pub const DONE_SENTINEL: &str = "[DONE]";

/// A single parsed tool call extracted from a model response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// The name of the tool to invoke.
    pub name: String,
    /// Arguments as a JSON-compatible map.
    pub arguments: HashMap<String, serde_json::Value>,
    /// Position in the response text where this call was found.
    pub position: usize,
}

/// The result of a single cascade turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CascadeTurn {
    /// Turn index (0-based).
    pub turn: usize,
    /// The prompt sent in this turn.
    pub prompt: String,
    /// The model's raw response.
    pub response: String,
    /// Tool calls parsed from the response, if any.
    pub tool_calls: Vec<ToolCall>,
    /// Tool execution results injected into the next turn's context.
    pub tool_results: Vec<ToolResult>,
    /// Elapsed time for this turn.
    pub elapsed_ms: u64,
}

/// The result of a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Name of the tool that was called.
    pub tool_name: String,
    /// JSON-serializable result.
    pub result: serde_json::Value,
    /// Whether execution succeeded.
    pub success: bool,
    /// Error message if `success` is false.
    pub error: Option<String>,
}

/// Final output of a complete cascade run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CascadeResult {
    /// Session this cascade belongs to.
    pub session_id: SessionId,
    /// All turns executed, in order.
    pub turns: Vec<CascadeTurn>,
    /// The final model response text (last turn's response).
    pub final_answer: String,
    /// Why the cascade stopped.
    pub termination_reason: TerminationReason,
    /// Total wall-clock time across all turns.
    pub total_elapsed_ms: u64,
    /// Number of tool calls executed.
    pub total_tool_calls: usize,
}

/// Why a cascade loop exited.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TerminationReason {
    /// Model produced no tool calls — natural completion.
    NoToolCalls,
    /// Model emitted the DONE sentinel explicitly.
    DoneSentinel,
    /// Maximum turn limit was reached.
    MaxTurnsReached,
    /// A pipeline error aborted the cascade.
    Error(String),
}

/// Trait for parsing tool calls out of a raw model response.
///
/// The default implementation looks for `<tool_call>…</tool_call>` JSON blocks.
/// Register custom parsers via [`CascadeEngine::with_tool_parser`].
#[async_trait]
pub trait ToolCallParser: Send + Sync {
    /// Parse zero or more tool calls from a model response string.
    async fn parse(&self, response: &str) -> Vec<ToolCall>;
}

/// Default parser: finds `<tool_call>…</tool_call>` JSON blocks.
pub struct XmlStyleToolParser;

#[async_trait]
impl ToolCallParser for XmlStyleToolParser {
    async fn parse(&self, response: &str) -> Vec<ToolCall> {
        let mut calls = Vec::new();
        let mut search_from = 0usize;

        while let Some(start_rel) = response[search_from..].find("<tool_call>") {
            let abs_start = search_from + start_rel;
            let content_start = abs_start + "<tool_call>".len();

            if let Some(end_rel) = response[content_start..].find("</tool_call>") {
                let abs_end = content_start + end_rel;
                let json_str = response[content_start..abs_end].trim();

                match serde_json::from_str::<HashMap<String, serde_json::Value>>(json_str) {
                    Ok(mut map) => {
                        let name = map
                            .remove("name")
                            .and_then(|v| v.as_str().map(String::from))
                            .unwrap_or_else(|| "unknown".to_string());
                        let arguments = map
                            .remove("arguments")
                            .and_then(|v| v.as_object().cloned())
                            .map(|o| o.into_iter().collect())
                            .unwrap_or_default();
                        calls.push(ToolCall {
                            name,
                            arguments,
                            position: abs_start,
                        });
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to parse tool_call JSON block");
                    }
                }
                search_from = abs_end + "</tool_call>".len();
            } else {
                break;
            }
        }

        calls
    }
}

/// Trait for executing tool calls and returning results.
///
/// Implement this to wire real tools (web search, code execution, DB queries)
/// into the cascade loop. The default [`NoopToolExecutor`] returns a stub result.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Execute a single tool call and return its result.
    async fn execute(&self, call: &ToolCall) -> ToolResult;
}

/// Stub executor that returns a placeholder result without doing real work.
///
/// Useful for testing the cascade loop logic without external dependencies.
pub struct NoopToolExecutor;

#[async_trait]
impl ToolExecutor for NoopToolExecutor {
    async fn execute(&self, call: &ToolCall) -> ToolResult {
        debug!(tool = %call.name, "noop executor called");
        ToolResult {
            tool_name: call.name.clone(),
            result: serde_json::json!({
                "status": "ok",
                "note": "noop executor — register a real ToolExecutor via CascadeEngine::with_executor"
            }),
            success: true,
            error: None,
        }
    }
}

/// Builds the next-turn prompt by injecting tool results into the conversation.
fn build_next_prompt(
    original_prompt: &str,
    turns: &[CascadeTurn],
    pending_results: &[ToolResult],
) -> String {
    let mut parts = Vec::new();

    // Original user prompt
    parts.push(format!("User: {original_prompt}"));

    // Prior turns
    for turn in turns {
        parts.push(format!("Assistant: {}", turn.response));
        for result in &turn.tool_results {
            let result_str = serde_json::to_string(&result.result).unwrap_or_default();
            parts.push(format!(
                "Tool[{}]: {}",
                result.tool_name, result_str
            ));
        }
    }

    // Pending results from last turn's tool calls
    for result in pending_results {
        let result_str = serde_json::to_string(&result.result).unwrap_or_default();
        parts.push(format!(
            "Tool[{}]: {}",
            result.tool_name, result_str
        ));
    }

    parts.push("Assistant:".to_string());
    parts.join("\n\n")
}

/// Configuration for a cascade run.
#[derive(Debug, Clone)]
pub struct CascadeConfig {
    /// Maximum number of turns before forcibly stopping (default: 10).
    pub max_turns: usize,
    /// Maximum wall-clock duration for the entire cascade.
    pub timeout: Duration,
    /// System prompt injected at the start of every turn.
    pub system_prompt: Option<String>,
}

impl Default for CascadeConfig {
    fn default() -> Self {
        Self {
            max_turns: DEFAULT_MAX_TURNS,
            timeout: Duration::from_secs(300),
            system_prompt: None,
        }
    }
}

/// Infer function type: takes a prompt string, returns the model response.
///
/// In production this is wired to [`spawn_pipeline`](crate::spawn_pipeline) or
/// any [`ModelWorker`](crate::ModelWorker). In tests it can be a simple closure.
pub type InferFn = Arc<
    dyn Fn(String) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<String, OrchestratorError>> + Send>,
        > + Send
        + Sync,
>;

/// Multi-turn cascading inference engine.
///
/// Drives a model through a tool-call loop until a termination condition is met.
///
/// # Example
///
/// ```no_run
/// use std::sync::Arc;
/// use tokio_prompt_orchestrator::cascade::{CascadeEngine, CascadeConfig, NoopToolExecutor};
///
/// # async fn example() {
/// let engine = CascadeEngine::new(
///     Arc::new(|prompt: String| Box::pin(async move {
///         Ok(format!("Echo: {prompt}"))
///     })),
///     Arc::new(NoopToolExecutor),
/// );
///
/// let result = engine
///     .run("Summarise the Rust docs", &Default::default())
///     .await
///     .unwrap();
///
/// println!("Final answer: {}", result.final_answer);
/// println!("Turns taken: {}", result.turns.len());
/// # }
/// ```
pub struct CascadeEngine {
    infer_fn: InferFn,
    executor: Arc<dyn ToolExecutor>,
    parser: Arc<dyn ToolCallParser>,
}

impl CascadeEngine {
    /// Create a new engine with the given infer function and tool executor.
    pub fn new(infer_fn: InferFn, executor: Arc<dyn ToolExecutor>) -> Self {
        Self {
            infer_fn,
            executor,
            parser: Arc::new(XmlStyleToolParser),
        }
    }

    /// Override the default `<tool_call>` XML parser with a custom one.
    pub fn with_tool_parser(mut self, parser: Arc<dyn ToolCallParser>) -> Self {
        self.parser = parser;
        self
    }

    /// Run the cascade for the given prompt, returning all turns and the final answer.
    ///
    /// # Errors
    ///
    /// Returns an error only if the first inference call fails. Subsequent
    /// failures are recorded in [`CascadeResult::termination_reason`].
    pub async fn run(
        &self,
        prompt: &str,
        config: &CascadeConfig,
    ) -> Result<CascadeResult, OrchestratorError> {
        let session = SessionId::new(uuid::Uuid::new_v4().to_string());
        let start = Instant::now();
        let deadline = start + config.timeout;

        let mut turns: Vec<CascadeTurn> = Vec::new();
        let mut current_prompt = if let Some(ref sys) = config.system_prompt {
            format!("{sys}\n\n{prompt}")
        } else {
            prompt.to_string()
        };

        info!(
            session_id = %session.as_str(),
            max_turns = config.max_turns,
            "cascade started"
        );

        for turn_idx in 0..config.max_turns {
            // Hard timeout guard
            if Instant::now() >= deadline {
                warn!(turn = turn_idx, "cascade hit wall-clock timeout");
                let total_ms = start.elapsed().as_millis() as u64;
                let total_calls: usize = turns.iter().map(|t| t.tool_calls.len()).sum();
                return Ok(CascadeResult {
                    session_id: session,
                    final_answer: turns.last().map(|t| t.response.clone()).unwrap_or_default(),
                    termination_reason: TerminationReason::MaxTurnsReached,
                    total_elapsed_ms: total_ms,
                    total_tool_calls: total_calls,
                    turns,
                });
            }

            let turn_start = Instant::now();

            // Infer
            let response = (self.infer_fn)(current_prompt.clone()).await.map_err(|e| {
                if turn_idx == 0 {
                    // First turn failure is a hard error
                    e
                } else {
                    // Subsequent turn failures are soft — we stop the cascade
                    OrchestratorError::Other(format!("cascade turn {turn_idx} failed: {e}"))
                }
            })?;

            let elapsed_ms = turn_start.elapsed().as_millis() as u64;

            // Check DONE sentinel
            if response.contains(DONE_SENTINEL) {
                let resp_clean = response.replace(DONE_SENTINEL, "").trim().to_string();
                let total_ms = start.elapsed().as_millis() as u64;
                let total_calls: usize = turns.iter().map(|t| t.tool_calls.len()).sum();

                turns.push(CascadeTurn {
                    turn: turn_idx,
                    prompt: current_prompt,
                    response: resp_clean.clone(),
                    tool_calls: vec![],
                    tool_results: vec![],
                    elapsed_ms,
                });

                info!(
                    session_id = %session.as_str(),
                    turns = turn_idx + 1,
                    reason = "done_sentinel",
                    "cascade complete"
                );

                return Ok(CascadeResult {
                    session_id: session,
                    final_answer: resp_clean,
                    termination_reason: TerminationReason::DoneSentinel,
                    total_elapsed_ms: total_ms,
                    total_tool_calls: total_calls,
                    turns,
                });
            }

            // Parse tool calls
            let tool_calls = self.parser.parse(&response).await;

            if tool_calls.is_empty() {
                // Natural completion — no tools requested
                let total_ms = start.elapsed().as_millis() as u64;
                let total_calls: usize = turns.iter().map(|t| t.tool_calls.len()).sum();

                turns.push(CascadeTurn {
                    turn: turn_idx,
                    prompt: current_prompt,
                    response: response.clone(),
                    tool_calls: vec![],
                    tool_results: vec![],
                    elapsed_ms,
                });

                info!(
                    session_id = %session.as_str(),
                    turns = turn_idx + 1,
                    reason = "no_tool_calls",
                    "cascade complete"
                );

                return Ok(CascadeResult {
                    session_id: session,
                    final_answer: response,
                    termination_reason: TerminationReason::NoToolCalls,
                    total_elapsed_ms: total_ms,
                    total_tool_calls: total_calls,
                    turns,
                });
            }

            // Execute all tool calls concurrently
            let executor = Arc::clone(&self.executor);
            let calls_for_exec = tool_calls.clone();
            let mut exec_futures = Vec::with_capacity(calls_for_exec.len());
            for call in &calls_for_exec {
                let exec = Arc::clone(&executor);
                let c = call.clone();
                exec_futures.push(async move { exec.execute(&c).await });
            }

            let tool_results = futures::future::join_all(exec_futures).await;

            debug!(
                turn = turn_idx,
                tool_count = tool_results.len(),
                "tool calls executed"
            );

            // Build next prompt
            let next_prompt = build_next_prompt(
                prompt,
                &turns,
                &tool_results,
            );

            turns.push(CascadeTurn {
                turn: turn_idx,
                prompt: current_prompt,
                response,
                tool_calls,
                tool_results,
                elapsed_ms,
            });

            current_prompt = next_prompt;
        }

        // Max turns reached
        let total_ms = start.elapsed().as_millis() as u64;
        let total_calls: usize = turns.iter().map(|t| t.tool_calls.len()).sum();
        let final_answer = turns.last().map(|t| t.response.clone()).unwrap_or_default();

        warn!(
            session_id = %session.as_str(),
            max_turns = config.max_turns,
            "cascade reached max turn limit"
        );

        Ok(CascadeResult {
            session_id: session,
            final_answer,
            termination_reason: TerminationReason::MaxTurnsReached,
            total_elapsed_ms: total_ms,
            total_tool_calls: total_calls,
            turns,
        })
    }
}

/// A thread-safe store of active cascade sessions for introspection.
///
/// Useful for monitoring dashboards and the web API to surface
/// how many cascades are running and their turn counts.
pub struct CascadeMonitor {
    active: Arc<Mutex<HashMap<String, usize>>>,
}

impl Default for CascadeMonitor {
    fn default() -> Self {
        Self {
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl CascadeMonitor {
    /// Record that a cascade has started.
    pub async fn on_start(&self, session_id: &str) {
        self.active.lock().await.insert(session_id.to_string(), 0);
    }

    /// Record that a cascade has completed another turn.
    pub async fn on_turn(&self, session_id: &str, turn: usize) {
        if let Some(entry) = self.active.lock().await.get_mut(session_id) {
            *entry = turn;
        }
    }

    /// Record that a cascade has finished.
    pub async fn on_complete(&self, session_id: &str) {
        self.active.lock().await.remove(session_id);
    }

    /// Return the number of active cascade sessions.
    pub async fn active_count(&self) -> usize {
        self.active.lock().await.len()
    }

    /// Return a snapshot of active sessions and their current turn number.
    pub async fn snapshot(&self) -> HashMap<String, usize> {
        self.active.lock().await.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_echo_infer() -> InferFn {
        Arc::new(|prompt: String| {
            Box::pin(async move { Ok(format!("Echo: {}", &prompt[..prompt.len().min(50)])) })
        })
    }

    fn make_one_tool_infer() -> InferFn {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        Arc::new(move |_prompt: String| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    // First call: request a tool
                    Ok(r#"<tool_call>{"name":"search","arguments":{"q":"rust"}}</tool_call>"#
                        .to_string())
                } else {
                    // Second call: done
                    Ok("Final answer based on search results.".to_string())
                }
            })
        })
    }

    #[tokio::test]
    async fn test_no_tool_calls_terminates_immediately() {
        let engine = CascadeEngine::new(make_echo_infer(), Arc::new(NoopToolExecutor));
        let result = engine.run("Hello", &Default::default()).await.unwrap();
        assert_eq!(result.termination_reason, TerminationReason::NoToolCalls);
        assert_eq!(result.turns.len(), 1);
        assert_eq!(result.total_tool_calls, 0);
    }

    #[tokio::test]
    async fn test_done_sentinel_terminates() {
        let infer: InferFn = Arc::new(|_p: String| {
            Box::pin(async { Ok(format!("I am done. {DONE_SENTINEL}")) })
        });
        let engine = CascadeEngine::new(infer, Arc::new(NoopToolExecutor));
        let result = engine.run("Test", &Default::default()).await.unwrap();
        assert_eq!(result.termination_reason, TerminationReason::DoneSentinel);
    }

    #[tokio::test]
    async fn test_tool_call_then_answer() {
        let engine = CascadeEngine::new(make_one_tool_infer(), Arc::new(NoopToolExecutor));
        let result = engine.run("Search for Rust", &Default::default()).await.unwrap();
        assert_eq!(result.termination_reason, TerminationReason::NoToolCalls);
        assert_eq!(result.turns.len(), 2);
        assert_eq!(result.total_tool_calls, 1);
        assert_eq!(result.turns[0].tool_calls[0].name, "search");
    }

    #[tokio::test]
    async fn test_max_turns_respected() {
        let infer: InferFn = Arc::new(|_p: String| {
            Box::pin(async {
                Ok(r#"<tool_call>{"name":"loop","arguments":{}}</tool_call>"#.to_string())
            })
        });
        let engine = CascadeEngine::new(infer, Arc::new(NoopToolExecutor));
        let config = CascadeConfig {
            max_turns: 3,
            ..Default::default()
        };
        let result = engine.run("Loop forever", &config).await.unwrap();
        assert_eq!(result.termination_reason, TerminationReason::MaxTurnsReached);
        assert_eq!(result.turns.len(), 3);
    }

    #[tokio::test]
    async fn test_xml_parser_extracts_tool_calls() {
        let parser = XmlStyleToolParser;
        let text = r#"
            I'll search for this.
            <tool_call>{"name": "search", "arguments": {"query": "tokio async"}}</tool_call>
            And also look this up:
            <tool_call>{"name": "lookup", "arguments": {"id": 42}}</tool_call>
        "#;
        let calls = parser.parse(text).await;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "search");
        assert_eq!(calls[1].name, "lookup");
    }

    #[tokio::test]
    async fn test_cascade_monitor() {
        let monitor = CascadeMonitor::default();
        monitor.on_start("sess-1").await;
        monitor.on_start("sess-2").await;
        assert_eq!(monitor.active_count().await, 2);
        monitor.on_turn("sess-1", 3).await;
        let snap = monitor.snapshot().await;
        assert_eq!(snap["sess-1"], 3);
        monitor.on_complete("sess-1").await;
        assert_eq!(monitor.active_count().await, 1);
    }
}
