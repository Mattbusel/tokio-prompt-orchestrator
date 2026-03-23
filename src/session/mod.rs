//! # Session Context Manager
//!
//! Tracks multi-turn conversation history per [`SessionId`] and enriches
//! incoming [`PromptRequest`]s with relevant prior context before they enter
//! the pipeline.
//!
//! ## Overview
//!
//! Many LLM applications need conversational memory: the model must "know"
//! what was said in earlier turns of the same chat session.  Without
//! session tracking every request arrives context-free and the user has to
//! repeat themselves.
//!
//! The [`SessionContext`] type provides:
//!
//! - **Per-session history** stored in a [`DashMap`] (lock-free concurrent
//!   hash map).  Each entry is a bounded ring buffer of
//!   [`ConversationTurn`]s.
//! - **Auto-injection**: [`SessionContext::enrich`] prepends the last
//!   `context_window` turns to the prompt input, formatted as a human-readable
//!   dialogue transcript.
//! - **Response recording**: [`SessionContext::record_response`] appends the
//!   model's answer after a successful inference call.
//! - **Sliding window**: only the most recent `max_turns` turns are kept;
//!   older entries are evicted automatically to bound memory use.
//! - **Session expiry**: inactive sessions are evicted after a configurable
//!   TTL, preventing unbounded map growth.
//! - **Optional summarisation stub**: when history grows beyond
//!   `summarise_after_turns`, callers receive a
//!   [`SessionAction::RequestSummary`] signal.  The caller is responsible
//!   for sending a summarisation request through the pipeline and replacing
//!   history with the returned summary via [`SessionContext::summarise`].
//!
//! ## Example
//!
//! ```no_run
//! use std::collections::HashMap;
//! use std::time::Duration;
//! use tokio_prompt_orchestrator::{SessionId, PromptRequest};
//! use tokio_prompt_orchestrator::session::{SessionContext, SessionConfig};
//!
//! #[tokio::main]
//! async fn main() {
//!     let ctx = SessionContext::new(SessionConfig::default());
//!
//!     let session = SessionId::new("alice");
//!
//!     // First turn — no prior history, prompt is unchanged.
//!     let req = PromptRequest {
//!         session: session.clone(),
//!         request_id: "r1".into(),
//!         input: "What is the capital of France?".into(),
//!         meta: HashMap::new(),
//!         deadline: None,
//!     };
//!     let enriched = ctx.enrich(req).await;
//!     assert_eq!(enriched.input, "What is the capital of France?");
//!
//!     // Record the model's response.
//!     ctx.record_response(&session, "The capital of France is Paris.").await;
//!
//!     // Second turn — prior history is prepended automatically.
//!     let req2 = PromptRequest {
//!         session: session.clone(),
//!         request_id: "r2".into(),
//!         input: "What is its population?".into(),
//!         meta: HashMap::new(),
//!         deadline: None,
//!     };
//!     let enriched2 = ctx.enrich(req2).await;
//!     assert!(enriched2.input.contains("Paris"));
//! }
//! ```

pub mod budget;
pub mod context;

pub use budget::{
    BudgetError, BudgetOutcome, SessionBudget, SessionBudgetSnapshot, SessionLimits,
};
pub use context::{
    ConversationTurn, SessionAction, SessionConfig, SessionContext, SessionStats, TurnRole,
};
