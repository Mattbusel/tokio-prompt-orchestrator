//! # Context Manager
//!
//! Multi-turn LLM conversation context manager with token budget enforcement
//! and configurable truncation strategies.
//!
//! ## Overview
//!
//! [`ConversationContext`] maintains an ordered deque of [`Message`] values and
//! automatically enforces a token budget when new messages are added.  Three
//! truncation strategies are supported: dropping the oldest messages, keeping
//! only the first N turns plus recent history, and replacing oldest messages
//! with a summary placeholder.
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::context_mgr::{
//!     ConversationContext, ContextConfig, Role, TruncationStrategy,
//! };
//!
//! let config = ContextConfig {
//!     max_tokens: 1000,
//!     system_prompt: Some("You are a helpful assistant.".into()),
//!     reserve_for_response: 100,
//!     truncation_strategy: TruncationStrategy::DropOldest,
//! };
//! let mut ctx = ConversationContext::new(config);
//! ctx.add_message(Role::User, "Hello!");
//! ctx.add_message(Role::Assistant, "Hi there!");
//! assert!(!ctx.messages_for_api().is_empty());
//! ```

use std::{
    collections::VecDeque,
    time::{SystemTime, UNIX_EPOCH},
};

// ── TokenCounter ──────────────────────────────────────────────────────────────

/// Simple word-count token estimator.
///
/// Approximates token count as `words * 4 / 3` (~1.33 tokens per word), which
/// is a reasonable heuristic for most LLM tokenisers.
pub struct TokenCounter;

impl TokenCounter {
    /// Estimate the number of tokens in `text`.
    pub fn count(text: &str) -> usize {
        let words = text.split_whitespace().count();
        words * 4 / 3
    }
}

// ── Role ──────────────────────────────────────────────────────────────────────

/// The role of a message participant in a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// A system-level instruction (not counted toward turn limits).
    System,
    /// A human turn.
    User,
    /// A model turn.
    Assistant,
    /// A tool/function response.
    Tool,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::System => write!(f, "system"),
            Role::User => write!(f, "user"),
            Role::Assistant => write!(f, "assistant"),
            Role::Tool => write!(f, "tool"),
        }
    }
}

// ── Message ───────────────────────────────────────────────────────────────────

/// A single message in a conversation.
#[derive(Debug, Clone)]
pub struct Message {
    /// Who sent the message.
    pub role: Role,
    /// The message text.
    pub content: String,
    /// Estimated token count (computed at insertion time).
    pub token_count: usize,
    /// Unix timestamp (seconds) of when this message was added.
    pub timestamp: u64,
}

impl Message {
    fn new(role: Role, content: &str) -> Self {
        let token_count = TokenCounter::count(content);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            role,
            content: content.to_string(),
            token_count,
            timestamp,
        }
    }
}

// ── TruncationStrategy ────────────────────────────────────────────────────────

/// How to reduce the context when the token budget is exceeded.
#[derive(Debug, Clone)]
pub enum TruncationStrategy {
    /// Remove the oldest non-system messages one by one until within budget.
    DropOldest,
    /// Replace the oldest non-system messages with a single summary placeholder.
    SummarizeOldest,
    /// Keep the system prompt, the first `n` user/assistant turns, and fill
    /// the remainder with the most recent messages.
    KeepFirst {
        /// Number of early turns to preserve.
        n: usize,
    },
}

// ── ContextConfig ─────────────────────────────────────────────────────────────

/// Configuration for a [`ConversationContext`].
#[derive(Debug, Clone)]
pub struct ContextConfig {
    /// Hard token budget for the entire context window.
    pub max_tokens: usize,
    /// Optional system prompt prepended to every context window.
    pub system_prompt: Option<String>,
    /// Token budget reserved for the model's response.  Effective budget is
    /// `max_tokens - reserve_for_response`.
    pub reserve_for_response: usize,
    /// Strategy to apply when the token budget is exceeded.
    pub truncation_strategy: TruncationStrategy,
}

impl ContextConfig {
    /// Effective token budget after reserving space for the model response.
    pub fn effective_budget(&self) -> usize {
        self.max_tokens.saturating_sub(self.reserve_for_response)
    }
}

// ── ContextSummary ────────────────────────────────────────────────────────────

/// A lightweight summary of the current context state.
#[derive(Debug, Clone)]
pub struct ContextSummary {
    /// Number of messages currently in the context (including system).
    pub message_count: usize,
    /// Total estimated token usage.
    pub total_tokens: usize,
    /// Number of user turns.
    pub user_turns: usize,
    /// Number of assistant turns.
    pub assistant_turns: usize,
    /// Age in seconds of the oldest message (0 if no messages).
    pub oldest_message_age_secs: u64,
}

// ── ConversationContext ────────────────────────────────────────────────────────

/// Multi-turn conversation context with automatic token budget enforcement.
pub struct ConversationContext {
    /// Configuration (strategy, budgets, system prompt).
    pub config: ContextConfig,
    /// Ordered message history (front = oldest).
    pub messages: VecDeque<Message>,
    /// Running total of estimated tokens across all messages.
    pub total_tokens: usize,
}

impl ConversationContext {
    /// Create a new context.  If the config has a `system_prompt`, it is
    /// added as the first message immediately.
    pub fn new(config: ContextConfig) -> Self {
        let mut ctx = Self {
            config,
            messages: VecDeque::new(),
            total_tokens: 0,
        };
        // Pre-load the system prompt.
        if let Some(sp) = ctx.config.system_prompt.clone() {
            let msg = Message::new(Role::System, &sp);
            ctx.total_tokens += msg.token_count;
            ctx.messages.push_back(msg);
        }
        ctx
    }

    /// Append a new message and enforce the token budget.
    pub fn add_message(&mut self, role: Role, content: &str) {
        let msg = Message::new(role, content);
        self.total_tokens += msg.token_count;
        self.messages.push_back(msg);
        self.enforce_budget();
    }

    /// Apply the configured [`TruncationStrategy`] if the effective budget is
    /// exceeded.
    pub fn enforce_budget(&mut self) {
        let budget = self.config.effective_budget();
        if self.total_tokens <= budget {
            return;
        }
        match self.config.truncation_strategy.clone() {
            TruncationStrategy::DropOldest => {
                self.drop_oldest(budget);
            }
            TruncationStrategy::SummarizeOldest => {
                self.summarize_oldest(budget);
            }
            TruncationStrategy::KeepFirst { n } => {
                self.keep_first(n, budget);
            }
        }
    }

    // -- internal truncation helpers ------------------------------------------

    /// Drop the oldest non-system messages until within `budget`.
    fn drop_oldest(&mut self, budget: usize) {
        while self.total_tokens > budget {
            // Find the first non-system message index.
            let idx = self
                .messages
                .iter()
                .position(|m| m.role != Role::System);
            match idx {
                Some(i) => {
                    let removed = self.messages.remove(i).expect("index exists");
                    self.total_tokens = self.total_tokens.saturating_sub(removed.token_count);
                }
                None => break, // only system message left, nothing more to drop
            }
        }
    }

    /// Replace the oldest non-system messages with a single summary placeholder
    /// until within `budget`.
    fn summarize_oldest(&mut self, budget: usize) {
        let mut summarized = 0usize;
        while self.total_tokens > budget {
            let idx = self
                .messages
                .iter()
                .position(|m| m.role != Role::System);
            match idx {
                Some(i) => {
                    let removed = self.messages.remove(i).expect("index exists");
                    self.total_tokens =
                        self.total_tokens.saturating_sub(removed.token_count);
                    summarized += 1;
                }
                None => break,
            }
        }
        if summarized > 0 {
            // Insert the placeholder right after the last system message.
            let insert_pos = self
                .messages
                .iter()
                .rposition(|m| m.role == Role::System)
                .map(|p| p + 1)
                .unwrap_or(0);
            let placeholder =
                format!("[{} messages summarized]", summarized);
            let msg = Message::new(Role::System, &placeholder);
            self.total_tokens += msg.token_count;
            self.messages.insert(insert_pos, msg);
        }
    }

    /// Keep the system prompt + first `n` non-system messages + most recent
    /// messages, dropping middle messages, until within `budget`.
    fn keep_first(&mut self, n: usize, budget: usize) {
        if self.total_tokens <= budget {
            return;
        }
        // Collect system messages and non-system messages separately.
        let (system_msgs, non_system): (Vec<_>, Vec<_>) = self
            .messages
            .drain(..)
            .partition(|m| m.role == Role::System);

        // Determine tokens used by system messages.
        let system_tokens: usize = system_msgs.iter().map(|m| m.token_count).sum();
        let remaining_budget = budget.saturating_sub(system_tokens);

        // Split non-system into first-n and the rest.
        let first_n: Vec<_> = non_system.iter().take(n).cloned().collect();
        let rest: Vec<_> = non_system.into_iter().skip(n).collect();

        let first_n_tokens: usize = first_n.iter().map(|m| m.token_count).sum();
        let tail_budget = remaining_budget.saturating_sub(first_n_tokens);

        // Fill tail from the most recent messages.
        let mut tail: Vec<Message> = Vec::new();
        let mut tail_tokens = 0usize;
        for msg in rest.into_iter().rev() {
            if tail_tokens + msg.token_count > tail_budget {
                break;
            }
            tail_tokens += msg.token_count;
            tail.push(msg);
        }
        tail.reverse();

        // Reassemble.
        self.messages.clear();
        for m in system_msgs {
            self.messages.push_back(m);
        }
        for m in first_n {
            self.messages.push_back(m);
        }
        for m in tail {
            self.messages.push_back(m);
        }
        self.total_tokens = self.messages.iter().map(|m| m.token_count).sum();
    }

    // -- public query API --------------------------------------------------------

    /// Return the messages that should be sent to the LLM API.
    pub fn messages_for_api(&self) -> Vec<&Message> {
        self.messages.iter().collect()
    }

    /// Token utilisation ratio in `[0.0, 1.0]`.
    pub fn token_utilization(&self) -> f64 {
        if self.config.max_tokens == 0 {
            return 1.0;
        }
        self.total_tokens as f64 / self.config.max_tokens as f64
    }

    /// Return a lightweight summary snapshot.
    pub fn summary(&self) -> ContextSummary {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let user_turns = self.messages.iter().filter(|m| m.role == Role::User).count();
        let assistant_turns = self
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();
        let oldest_age = self
            .messages
            .front()
            .map(|m| now.saturating_sub(m.timestamp))
            .unwrap_or(0);

        ContextSummary {
            message_count: self.messages.len(),
            total_tokens: self.total_tokens,
            user_turns,
            assistant_turns,
            oldest_message_age_secs: oldest_age,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn basic_config(max_tokens: usize, strategy: TruncationStrategy) -> ContextConfig {
        ContextConfig {
            max_tokens,
            system_prompt: None,
            reserve_for_response: 0,
            truncation_strategy: strategy,
        }
    }

    // Helper: 3 words → TokenCounter::count = 3 * 4 / 3 = 4 tokens.
    const THREE_WORD_MSG: &str = "hello world foo";

    #[test]
    fn test_token_counter() {
        assert_eq!(TokenCounter::count("hello world"), 2); // 2 * 4/3 = 2
        assert_eq!(TokenCounter::count(""), 0);
        // 6 words → 6 * 4 / 3 = 8
        assert_eq!(TokenCounter::count("one two three four five six"), 8);
    }

    #[test]
    fn test_add_message_accumulates_tokens() {
        let config = basic_config(10_000, TruncationStrategy::DropOldest);
        let mut ctx = ConversationContext::new(config);
        ctx.add_message(Role::User, THREE_WORD_MSG);
        // 3 words → 4 tokens
        assert_eq!(ctx.total_tokens, 4);
        ctx.add_message(Role::Assistant, THREE_WORD_MSG);
        assert_eq!(ctx.total_tokens, 8);
    }

    #[test]
    fn test_drop_oldest_enforces_budget() {
        // Budget: 8 tokens.  Each 3-word message = 4 tokens.
        // After 3 messages the total would be 12 > 8 so oldest must be dropped.
        let config = basic_config(8, TruncationStrategy::DropOldest);
        let mut ctx = ConversationContext::new(config);
        ctx.add_message(Role::User, THREE_WORD_MSG); // 4 tokens
        ctx.add_message(Role::Assistant, THREE_WORD_MSG); // 4 tokens → total 8
        ctx.add_message(Role::User, THREE_WORD_MSG); // 4 tokens → would be 12, trigger drop
        // One message should have been dropped to get back to ≤ 8.
        assert!(ctx.total_tokens <= 8, "tokens={}", ctx.total_tokens);
        assert_eq!(ctx.messages.len(), 2);
    }

    #[test]
    fn test_summarize_oldest_inserts_placeholder() {
        let config = basic_config(8, TruncationStrategy::SummarizeOldest);
        let mut ctx = ConversationContext::new(config);
        ctx.add_message(Role::User, THREE_WORD_MSG);     // 4 tokens
        ctx.add_message(Role::Assistant, THREE_WORD_MSG); // 4 tokens → 8
        ctx.add_message(Role::User, THREE_WORD_MSG);     // 4 → 12, trigger summarize

        // Total should be within budget.
        assert!(ctx.total_tokens <= 8, "tokens={}", ctx.total_tokens);
        // There should be a placeholder message.
        let has_placeholder = ctx
            .messages
            .iter()
            .any(|m| m.content.contains("messages summarized"));
        assert!(has_placeholder, "expected a summary placeholder");
    }

    #[test]
    fn test_keep_first_strategy() {
        // max_tokens=12, n=1 (keep 1 first turn + recent).
        // Each message = 4 tokens.
        let config = basic_config(12, TruncationStrategy::KeepFirst { n: 1 });
        let mut ctx = ConversationContext::new(config);
        ctx.add_message(Role::User, THREE_WORD_MSG);     // msg A
        ctx.add_message(Role::Assistant, THREE_WORD_MSG); // msg B
        ctx.add_message(Role::User, THREE_WORD_MSG);     // msg C
        ctx.add_message(Role::Assistant, THREE_WORD_MSG); // msg D → 16 tokens > 12
        assert!(ctx.total_tokens <= 12, "tokens={}", ctx.total_tokens);
    }

    #[test]
    fn test_messages_for_api() {
        let config = basic_config(10_000, TruncationStrategy::DropOldest);
        let mut ctx = ConversationContext::new(config);
        ctx.add_message(Role::User, "hello");
        ctx.add_message(Role::Assistant, "world");
        let msgs = ctx.messages_for_api();
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn test_token_utilization() {
        let config = ContextConfig {
            max_tokens: 100,
            system_prompt: None,
            reserve_for_response: 0,
            truncation_strategy: TruncationStrategy::DropOldest,
        };
        let mut ctx = ConversationContext::new(config);
        ctx.add_message(Role::User, "hello world"); // 2 words → 2 tokens
        let util = ctx.token_utilization();
        assert!(util > 0.0 && util <= 1.0, "util={}", util);
    }

    #[test]
    fn test_system_prompt_preserved() {
        let config = ContextConfig {
            max_tokens: 16,
            system_prompt: Some("You are helpful.".to_string()),
            reserve_for_response: 0,
            truncation_strategy: TruncationStrategy::DropOldest,
        };
        let mut ctx = ConversationContext::new(config);
        // Fill with user/assistant pairs until truncation kicks in.
        for _ in 0..5 {
            ctx.add_message(Role::User, "hello world foo");
            ctx.add_message(Role::Assistant, "hello world foo");
        }
        // System message must never be dropped.
        let has_system = ctx.messages.iter().any(|m| m.role == Role::System);
        assert!(has_system, "system prompt was dropped during truncation");
    }

    #[test]
    fn test_summary() {
        let config = basic_config(10_000, TruncationStrategy::DropOldest);
        let mut ctx = ConversationContext::new(config);
        ctx.add_message(Role::User, "question");
        ctx.add_message(Role::Assistant, "answer");
        let s = ctx.summary();
        assert_eq!(s.user_turns, 1);
        assert_eq!(s.assistant_turns, 1);
        assert_eq!(s.message_count, 2);
    }

    #[test]
    fn test_reserve_for_response_reduces_budget() {
        let config = ContextConfig {
            max_tokens: 20,
            system_prompt: None,
            reserve_for_response: 12,
            truncation_strategy: TruncationStrategy::DropOldest,
        };
        // effective budget = 20 - 12 = 8 tokens.
        assert_eq!(config.effective_budget(), 8);
        let mut ctx = ConversationContext::new(config);
        // Each 3-word message ≈ 4 tokens.
        ctx.add_message(Role::User, THREE_WORD_MSG); // 4
        ctx.add_message(Role::User, THREE_WORD_MSG); // 4 → 8 (at budget)
        ctx.add_message(Role::User, THREE_WORD_MSG); // 4 → 12, must drop
        assert!(ctx.total_tokens <= 8, "tokens={}", ctx.total_tokens);
    }
}
