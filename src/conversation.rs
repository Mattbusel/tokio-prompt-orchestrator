//! # Conversation History Manager
//!
//! Tracks multi-turn conversation history per [`SessionId`], automatically injects
//! prior turns into outgoing prompts, and compresses old history when approaching
//! a configurable token budget.
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use tokio_prompt_orchestrator::conversation::{ConversationManager, ConversationConfig};
//! use tokio_prompt_orchestrator::SessionId;
//!
//! #[tokio::main]
//! async fn main() {
//!     let mgr = ConversationManager::new(ConversationConfig::default());
//!     let sid = SessionId::new("user-42");
//!
//!     // Record a user turn
//!     mgr.push_user(&sid, "What is backpressure?").await;
//!
//!     // Build a prompt that includes conversation history
//!     let prompt = mgr.build_prompt(&sid, "Explain it with an example.").await;
//!     println!("{prompt}");
//!
//!     // Record the assistant's reply
//!     mgr.push_assistant(&sid, "Backpressure is…").await;
//! }
//! ```

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::sync::RwLock;

use crate::SessionId;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Role of a conversation turn.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// Tool / function result injected mid-conversation.
    Tool,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        f.write_str(s)
    }
}

/// A single turn in a conversation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Turn {
    pub id: String,
    pub role: Role,
    pub content: String,
    pub timestamp: SystemTime,
    /// Rough token estimate (4 chars ≈ 1 token).
    pub token_estimate: usize,
    /// Optional key–value tags for filtering or retrieval.
    pub tags: Vec<String>,
    /// True if this turn was synthesised during compression (not verbatim).
    pub is_summary: bool,
}

impl Turn {
    fn new(role: Role, content: impl Into<String>) -> Self {
        let content = content.into();
        let token_estimate = estimate_tokens(&content);
        Turn {
            id: new_id(),
            role,
            content,
            timestamp: SystemTime::now(),
            token_estimate,
            tags: Vec::new(),
            is_summary: false,
        }
    }
}

/// A single conversation thread.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Conversation {
    pub id: String,
    pub session_id: String,
    pub turns: Vec<Turn>,
    pub total_tokens: usize,
    pub created_at: SystemTime,
    pub last_active: SystemTime,
    /// Number of times history was compressed.
    pub compressions: u32,
    /// Metadata bag for callers to attach arbitrary data.
    pub meta: HashMap<String, String>,
}

impl Conversation {
    fn new(session_id: &str) -> Self {
        let now = SystemTime::now();
        Conversation {
            id: new_id(),
            session_id: session_id.to_owned(),
            turns: Vec::new(),
            total_tokens: 0,
            created_at: now,
            last_active: now,
            compressions: 0,
            meta: HashMap::new(),
        }
    }

    fn push(&mut self, turn: Turn) {
        self.total_tokens += turn.token_estimate;
        self.last_active = SystemTime::now();
        self.turns.push(turn);
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for [`ConversationManager`].
#[derive(Debug, Clone)]
pub struct ConversationConfig {
    /// Maximum tokens allowed in a conversation before compression is triggered.
    /// Default: 6 000.
    pub max_tokens: usize,

    /// Always keep the N most recent turns verbatim (never compressed).
    /// Default: 4.
    pub recency_keep: usize,

    /// System prompt injected at the start of every built prompt.
    /// If `None`, no system prompt is added.
    pub system_prompt: Option<String>,

    /// Conversations inactive for longer than this duration are evicted.
    /// Default: 2 hours.
    pub ttl: Duration,

    /// Format used when assembling a multi-turn prompt.
    /// Default: `ChatMl`.
    pub format: PromptFormat,
}

impl Default for ConversationConfig {
    fn default() -> Self {
        ConversationConfig {
            max_tokens: 6_000,
            recency_keep: 4,
            system_prompt: None,
            ttl: Duration::from_secs(2 * 3600),
            format: PromptFormat::ChatMl,
        }
    }
}

/// Wire format used when serialising conversation history into a prompt string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptFormat {
    /// OpenAI / Anthropic style:
    /// ```text
    /// <|system|>…<|user|>…<|assistant|>…
    /// ```
    ChatMl,

    /// Plain markdown with role headers:
    /// ```text
    /// ## User\n…\n## Assistant\n…
    /// ```
    Markdown,

    /// Each turn on one line as `ROLE: content`.
    Inline,
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

/// Shared, async conversation history manager.
///
/// Thread-safe: clone the `Arc` to share across tasks. All operations are
/// O(1) amortised apart from [`build_prompt`](Self::build_prompt) which
/// is O(turns).
#[derive(Clone)]
pub struct ConversationManager {
    store: Arc<RwLock<HashMap<String, Conversation>>>,
    config: Arc<ConversationConfig>,
}

impl ConversationManager {
    /// Create a new manager with the given configuration.
    pub fn new(config: ConversationConfig) -> Self {
        ConversationManager {
            store: Arc::new(RwLock::new(HashMap::new())),
            config: Arc::new(config),
        }
    }

    // -----------------------------------------------------------------------
    // Mutation helpers
    // -----------------------------------------------------------------------

    /// Append a user turn to the conversation for `session`.
    pub async fn push_user(&self, session: &SessionId, content: impl Into<String>) {
        self.push(session, Role::User, content).await;
    }

    /// Append an assistant turn to the conversation for `session`.
    pub async fn push_assistant(&self, session: &SessionId, content: impl Into<String>) {
        self.push(session, Role::Assistant, content).await;
    }

    /// Append a tool-result turn.
    pub async fn push_tool(&self, session: &SessionId, content: impl Into<String>) {
        self.push(session, Role::Tool, content).await;
    }

    /// Append a system turn (overrides the global system prompt for this turn).
    pub async fn push_system(&self, session: &SessionId, content: impl Into<String>) {
        self.push(session, Role::System, content).await;
    }

    async fn push(&self, session: &SessionId, role: Role, content: impl Into<String>) {
        let key = session.as_str().to_owned();
        let turn = Turn::new(role, content);
        let max_tokens = self.config.max_tokens;
        let recency_keep = self.config.recency_keep;

        let mut guard = self.store.write().await;
        let conv = guard.entry(key).or_insert_with(|| Conversation::new(session.as_str()));
        conv.push(turn);

        if conv.total_tokens > max_tokens {
            compress_conversation(conv, recency_keep);
        }
    }

    // -----------------------------------------------------------------------
    // Prompt assembly
    // -----------------------------------------------------------------------

    /// Build a full prompt string that includes prior conversation history
    /// followed by `new_input` as the next user message.
    ///
    /// The returned string is ready to pass directly to a [`crate::ModelWorker`].
    pub async fn build_prompt(&self, session: &SessionId, new_input: &str) -> String {
        let key = session.as_str();
        let guard = self.store.read().await;

        let history: Vec<(Role, String)> = if let Some(conv) = guard.get(key) {
            conv.turns
                .iter()
                .map(|t| (t.role.clone(), t.content.clone()))
                .collect()
        } else {
            Vec::new()
        };

        drop(guard);
        self.format_prompt(&history, new_input)
    }

    fn format_prompt(&self, history: &[(Role, String)], new_input: &str) -> String {
        match self.config.format {
            PromptFormat::ChatMl => {
                let mut out = String::with_capacity(1024);
                if let Some(sys) = &self.config.system_prompt {
                    out.push_str("<|system|>\n");
                    out.push_str(sys);
                    out.push_str("\n<|end|>\n");
                }
                for (role, content) in history {
                    match role {
                        Role::System => {
                            out.push_str("<|system|>\n");
                            out.push_str(content);
                            out.push_str("\n<|end|>\n");
                        }
                        Role::User => {
                            out.push_str("<|user|>\n");
                            out.push_str(content);
                            out.push_str("\n<|end|>\n");
                        }
                        Role::Assistant => {
                            out.push_str("<|assistant|>\n");
                            out.push_str(content);
                            out.push_str("\n<|end|>\n");
                        }
                        Role::Tool => {
                            out.push_str("<|tool|>\n");
                            out.push_str(content);
                            out.push_str("\n<|end|>\n");
                        }
                    }
                }
                out.push_str("<|user|>\n");
                out.push_str(new_input);
                out.push_str("\n<|end|>\n<|assistant|>\n");
                out
            }

            PromptFormat::Markdown => {
                let mut out = String::with_capacity(1024);
                if let Some(sys) = &self.config.system_prompt {
                    out.push_str("## System\n\n");
                    out.push_str(sys);
                    out.push_str("\n\n---\n\n");
                }
                for (role, content) in history {
                    let header = match role {
                        Role::System => "## System",
                        Role::User => "## User",
                        Role::Assistant => "## Assistant",
                        Role::Tool => "## Tool",
                    };
                    out.push_str(header);
                    out.push_str("\n\n");
                    out.push_str(content);
                    out.push_str("\n\n");
                }
                out.push_str("## User\n\n");
                out.push_str(new_input);
                out.push_str("\n\n## Assistant\n\n");
                out
            }

            PromptFormat::Inline => {
                let mut out = String::with_capacity(1024);
                if let Some(sys) = &self.config.system_prompt {
                    out.push_str("system: ");
                    out.push_str(sys);
                    out.push('\n');
                }
                for (role, content) in history {
                    out.push_str(&role.to_string());
                    out.push_str(": ");
                    out.push_str(content);
                    out.push('\n');
                }
                out.push_str("user: ");
                out.push_str(new_input);
                out.push('\n');
                out
            }
        }
    }

    // -----------------------------------------------------------------------
    // Introspection
    // -----------------------------------------------------------------------

    /// Return a snapshot of the conversation for `session`, or `None` if no
    /// history exists.
    pub async fn get(&self, session: &SessionId) -> Option<Conversation> {
        let guard = self.store.read().await;
        guard.get(session.as_str()).cloned()
    }

    /// Return the number of turns currently stored for `session`.
    pub async fn turn_count(&self, session: &SessionId) -> usize {
        let guard = self.store.read().await;
        guard
            .get(session.as_str())
            .map(|c| c.turns.len())
            .unwrap_or(0)
    }

    /// Return the estimated token count for `session`.
    pub async fn token_count(&self, session: &SessionId) -> usize {
        let guard = self.store.read().await;
        guard
            .get(session.as_str())
            .map(|c| c.total_tokens)
            .unwrap_or(0)
    }

    /// Attach arbitrary metadata to a session's conversation.
    pub async fn set_meta(&self, session: &SessionId, key: impl Into<String>, value: impl Into<String>) {
        let mut guard = self.store.write().await;
        let conv = guard
            .entry(session.as_str().to_owned())
            .or_insert_with(|| Conversation::new(session.as_str()));
        conv.meta.insert(key.into(), value.into());
    }

    /// Clear the conversation history for `session`.
    pub async fn clear(&self, session: &SessionId) {
        let mut guard = self.store.write().await;
        guard.remove(session.as_str());
    }

    /// Evict all conversations that have been inactive longer than the
    /// configured TTL. Call this periodically (e.g. every hour) to reclaim
    /// memory.
    pub async fn evict_stale(&self) {
        let ttl = self.config.ttl;
        let mut guard = self.store.write().await;
        guard.retain(|_, conv| {
            conv.last_active
                .elapsed()
                .map(|d| d < ttl)
                .unwrap_or(true)
        });
    }

    /// Export a conversation as a JSON string for persistence or debugging.
    pub async fn export_json(&self, session: &SessionId) -> Option<String> {
        let guard = self.store.read().await;
        guard
            .get(session.as_str())
            .and_then(|c| serde_json::to_string_pretty(c).ok())
    }

    /// Return all active session IDs.
    pub async fn active_sessions(&self) -> Vec<String> {
        let guard = self.store.read().await;
        guard.keys().cloned().collect()
    }

    /// Total number of active conversations.
    pub async fn len(&self) -> usize {
        self.store.read().await.len()
    }

    /// True if there are no active conversations.
    pub async fn is_empty(&self) -> bool {
        self.store.read().await.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Compression
// ---------------------------------------------------------------------------

/// In-place compression: replaces old turns with a single summary turn,
/// preserving the most recent `recency_keep` turns verbatim.
fn compress_conversation(conv: &mut Conversation, recency_keep: usize) {
    let len = conv.turns.len();
    if len <= recency_keep {
        return;
    }

    let split = len.saturating_sub(recency_keep);
    let old_turns: Vec<Turn> = conv.turns.drain(..split).collect();

    // Build a compact summary of the compressed turns
    let mut summary_parts = Vec::new();
    let mut user_count = 0usize;
    let mut assistant_count = 0usize;

    for t in &old_turns {
        match t.role {
            Role::User => {
                user_count += 1;
                // Include significant user messages (> 20 tokens)
                if t.token_estimate > 20 && summary_parts.len() < 5 {
                    let excerpt = truncate_str(&t.content, 120);
                    summary_parts.push(format!("User said: {excerpt}"));
                }
            }
            Role::Assistant => {
                assistant_count += 1;
                if t.token_estimate > 30 && summary_parts.len() < 8 {
                    let excerpt = truncate_str(&t.content, 150);
                    summary_parts.push(format!("Assistant replied: {excerpt}"));
                }
            }
            Role::Tool => {
                if summary_parts.len() < 8 {
                    let excerpt = truncate_str(&t.content, 80);
                    summary_parts.push(format!("Tool returned: {excerpt}"));
                }
            }
            Role::System => {}
        }
    }

    let header = format!(
        "[Conversation summary — {user_count} user turns, {assistant_count} assistant turns compressed]"
    );

    let summary_text = if summary_parts.is_empty() {
        header
    } else {
        format!("{header}\n{}", summary_parts.join("\n"))
    };

    let summary_turn = Turn {
        id: new_id(),
        role: Role::System,
        content: summary_text,
        timestamp: SystemTime::now(),
        token_estimate: estimate_tokens(&{
            let mut s = "[Conversation summary]".to_owned();
            s
        }),
        tags: vec!["summary".to_owned()],
        is_summary: true,
    };

    // Recalculate total tokens
    conv.total_tokens = summary_turn.token_estimate
        + conv.turns.iter().map(|t| t.token_estimate).sum::<usize>();

    conv.turns.insert(0, summary_turn);
    conv.compressions += 1;
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

/// Estimate token count for a string (4 chars ≈ 1 token is a well-known heuristic).
pub fn estimate_tokens(s: &str) -> usize {
    (s.len() / 4).max(1)
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_owned()
    } else {
        let mut end = max_chars;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("{:08x}", t ^ (t.wrapping_mul(0x9e37_79b9)))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> SessionId {
        SessionId::new(s)
    }

    #[tokio::test]
    async fn test_push_and_count() {
        let mgr = ConversationManager::new(ConversationConfig::default());
        let s = sid("s1");
        mgr.push_user(&s, "Hello").await;
        mgr.push_assistant(&s, "Hi there").await;
        assert_eq!(mgr.turn_count(&s).await, 2);
    }

    #[tokio::test]
    async fn test_build_prompt_chatml() {
        let mgr = ConversationManager::new(ConversationConfig {
            format: PromptFormat::ChatMl,
            system_prompt: Some("You are helpful.".into()),
            ..Default::default()
        });
        let s = sid("s2");
        mgr.push_user(&s, "Q1").await;
        mgr.push_assistant(&s, "A1").await;
        let prompt = mgr.build_prompt(&s, "Q2").await;
        assert!(prompt.contains("<|system|>"));
        assert!(prompt.contains("Q1"));
        assert!(prompt.contains("A1"));
        assert!(prompt.contains("Q2"));
        assert!(prompt.ends_with("<|assistant|>\n"));
    }

    #[tokio::test]
    async fn test_build_prompt_markdown() {
        let mgr = ConversationManager::new(ConversationConfig {
            format: PromptFormat::Markdown,
            ..Default::default()
        });
        let s = sid("s3");
        mgr.push_user(&s, "What?").await;
        let prompt = mgr.build_prompt(&s, "Why?").await;
        assert!(prompt.contains("## User"));
        assert!(prompt.contains("What?"));
        assert!(prompt.contains("## Assistant"));
    }

    #[tokio::test]
    async fn test_compression_triggered() {
        let mgr = ConversationManager::new(ConversationConfig {
            max_tokens: 50,
            recency_keep: 2,
            ..Default::default()
        });
        let s = sid("s4");
        // Push enough content to exceed the 50-token budget
        for i in 0..10 {
            mgr.push_user(&s, format!("This is user message number {i} which has some content in it."))
                .await;
            mgr.push_assistant(&s, format!("This is the assistant reply to message {i}."))
                .await;
        }
        let conv = mgr.get(&s).await.unwrap();
        assert!(conv.compressions > 0, "should have triggered compression");
    }

    #[tokio::test]
    async fn test_clear() {
        let mgr = ConversationManager::new(ConversationConfig::default());
        let s = sid("s5");
        mgr.push_user(&s, "hello").await;
        mgr.clear(&s).await;
        assert_eq!(mgr.turn_count(&s).await, 0);
    }

    #[tokio::test]
    async fn test_export_json() {
        let mgr = ConversationManager::new(ConversationConfig::default());
        let s = sid("s6");
        mgr.push_user(&s, "test").await;
        let json = mgr.export_json(&s).await.unwrap();
        assert!(json.contains("\"content\""));
        assert!(json.contains("test"));
    }

    #[tokio::test]
    async fn test_evict_stale_leaves_active() {
        let mgr = ConversationManager::new(ConversationConfig {
            ttl: Duration::from_secs(3600),
            ..Default::default()
        });
        let s = sid("s7");
        mgr.push_user(&s, "hello").await;
        mgr.evict_stale().await;
        assert_eq!(mgr.len().await, 1);
    }

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens("hello"), 1);
        assert_eq!(estimate_tokens("hello world"), 2);
        assert!(estimate_tokens("a".repeat(100).as_str()) == 25);
    }
}
