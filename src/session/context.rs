//! Session context implementation.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use dashmap::DashMap;
use tracing::{debug, info};

use crate::{PromptRequest, SessionId};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the [`SessionContext`] manager.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Maximum number of conversation turns kept per session.
    ///
    /// When the ring buffer reaches this limit the oldest turn is evicted
    /// before the new one is added.  Defaults to `20`.
    pub max_turns: usize,

    /// Number of recent turns to inject into each new prompt.
    ///
    /// Must be ≤ `max_turns`.  Defaults to `6`.
    pub context_window: usize,

    /// If the number of stored turns exceeds this threshold the manager
    /// returns [`SessionAction::RequestSummary`] from [`SessionContext::enrich`]
    /// so the caller can ask the model to condense the history.
    ///
    /// Set to `usize::MAX` to disable summarisation.  Defaults to `15`.
    pub summarise_after_turns: usize,

    /// Sessions idle for longer than this TTL are evicted from memory.
    ///
    /// The eviction runs lazily on each [`SessionContext::enrich`] call (no
    /// background task required).  Defaults to 30 minutes.
    pub session_ttl: Duration,

    /// Header prepended to the injected history block.
    ///
    /// Defaults to `"[Conversation so far]\n"`.
    pub history_header: String,

    /// Separator between injected history and the new user turn.
    ///
    /// Defaults to `"\n[New message]\n"`.
    pub history_separator: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_turns: 20,
            context_window: 6,
            summarise_after_turns: 15,
            session_ttl: Duration::from_secs(30 * 60),
            history_header: "[Conversation so far]\n".into(),
            history_separator: "\n[New message]\n".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Turn types
// ---------------------------------------------------------------------------

/// Role of a participant in a conversation turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    /// The human / user side.
    User,
    /// The model / assistant side.
    Assistant,
}

impl TurnRole {
    fn label(self) -> &'static str {
        match self {
            TurnRole::User => "User",
            TurnRole::Assistant => "Assistant",
        }
    }
}

/// A single exchange in a conversation.
#[derive(Debug, Clone)]
pub struct ConversationTurn {
    /// Who produced this text.
    pub role: TurnRole,
    /// The text content.
    pub content: String,
    /// Wall-clock timestamp of when this turn was recorded.
    pub timestamp: Instant,
}

// ---------------------------------------------------------------------------
// Internal session state
// ---------------------------------------------------------------------------

struct SessionState {
    turns: VecDeque<ConversationTurn>,
    last_activity: Instant,
}

impl SessionState {
    fn new() -> Self {
        Self {
            turns: VecDeque::new(),
            last_activity: Instant::now(),
        }
    }

    fn push(&mut self, turn: ConversationTurn, max_turns: usize) {
        if self.turns.len() >= max_turns {
            self.turns.pop_front();
        }
        self.turns.push_back(turn);
        self.last_activity = Instant::now();
    }

    fn recent(&self, n: usize) -> impl Iterator<Item = &ConversationTurn> {
        let start = self.turns.len().saturating_sub(n);
        self.turns.range(start..)
    }

    fn len(&self) -> usize {
        self.turns.len()
    }
}

// ---------------------------------------------------------------------------
// SessionAction
// ---------------------------------------------------------------------------

/// Action the caller should take after [`SessionContext::enrich`].
#[derive(Debug)]
pub enum SessionAction {
    /// No special action required — proceed normally.
    Continue,
    /// The session history is long; consider asking the model to summarise.
    ///
    /// Call [`SessionContext::summarise`] with the model's summary text to
    /// collapse the history into a single assistant turn before the next enrich.
    RequestSummary,
}

// ---------------------------------------------------------------------------
// SessionContext
// ---------------------------------------------------------------------------

/// Shared, thread-safe conversation context manager.
///
/// See the [module documentation][super] for a full usage example.
#[derive(Clone)]
pub struct SessionContext {
    config: SessionConfig,
    sessions: Arc<DashMap<String, SessionState>>,
    // Metrics
    enriched_total: Arc<AtomicU64>,
    evicted_total: Arc<AtomicU64>,
    active_sessions: Arc<AtomicU64>,
}

impl SessionContext {
    /// Create a new [`SessionContext`] with the given [`SessionConfig`].
    pub fn new(config: SessionConfig) -> Self {
        Self {
            config,
            sessions: Arc::new(DashMap::new()),
            enriched_total: Arc::new(AtomicU64::new(0)),
            evicted_total: Arc::new(AtomicU64::new(0)),
            active_sessions: Arc::new(AtomicU64::new(0)),
        }
    }

    // ------------------------------------------------------------------
    // Public API
    // ------------------------------------------------------------------

    /// Enrich a [`PromptRequest`] with conversation history for its session.
    ///
    /// If the session has prior turns the last `context_window` turns are
    /// formatted as a dialogue transcript and prepended to `request.input`.
    /// The unmodified request is returned when there is no prior history.
    ///
    /// Also runs a lazy TTL eviction pass on every call (O(sessions) but
    /// amortised across all callers).
    ///
    /// # Returns
    ///
    /// A tuple of `(enriched_request, action)`.  `action` is
    /// [`SessionAction::RequestSummary`] when the stored turn count exceeds
    /// [`SessionConfig::summarise_after_turns`].
    pub async fn enrich(
        &self,
        mut request: PromptRequest,
    ) -> (PromptRequest, SessionAction) {
        self.evict_expired();

        let key = request.session.as_str().to_owned();

        // Build history block from the last `context_window` turns.
        let (history_block, turn_count) = {
            if let Some(state) = self.sessions.get(&key) {
                let turns: Vec<_> = state.recent(self.config.context_window).collect();
                if turns.is_empty() {
                    (None, state.len())
                } else {
                    let mut block = self.config.history_header.clone();
                    for turn in &turns {
                        block.push_str(turn.role.label());
                        block.push_str(": ");
                        block.push_str(&turn.content);
                        block.push('\n');
                    }
                    let count = state.len();
                    (Some(block), count)
                }
            } else {
                (None, 0)
            }
        };

        if let Some(block) = history_block {
            let original = request.input.clone();
            request.input = format!(
                "{}{}{}",
                block, self.config.history_separator, original
            );
            debug!(
                session = %request.session,
                injected_turns = self.config.context_window.min(turn_count),
                "injected conversation history"
            );
        }

        // Record this user turn.
        self.sessions
            .entry(key)
            .or_insert_with(|| {
                self.active_sessions.fetch_add(1, Ordering::Relaxed);
                SessionState::new()
            })
            .push(
                ConversationTurn {
                    role: TurnRole::User,
                    content: {
                        // Store the *original* (pre-enrichment) text so history
                        // does not grow exponentially.
                        let sep = &self.config.history_separator;
                        if let Some(pos) = request.input.find(sep.as_str()) {
                            request.input[pos + sep.len()..].to_owned()
                        } else {
                            request.input.clone()
                        }
                    },
                    timestamp: Instant::now(),
                },
                self.config.max_turns,
            );

        self.enriched_total.fetch_add(1, Ordering::Relaxed);

        let action = if turn_count + 1 >= self.config.summarise_after_turns {
            SessionAction::RequestSummary
        } else {
            SessionAction::Continue
        };

        (request, action)
    }

    /// Record a model response for a session, appending it to the history.
    ///
    /// Call this after a successful inference so the next call to [`enrich`]
    /// can inject the assistant's answer as context.
    ///
    /// [`enrich`]: SessionContext::enrich
    pub async fn record_response(&self, session: &SessionId, response: &str) {
        let key = session.as_str();
        if let Some(mut state) = self.sessions.get_mut(key) {
            state.push(
                ConversationTurn {
                    role: TurnRole::Assistant,
                    content: response.to_owned(),
                    timestamp: Instant::now(),
                },
                self.config.max_turns,
            );
            debug!(session = %session, "recorded assistant response");
        }
    }

    /// Replace the full conversation history for a session with a single
    /// summary turn.
    ///
    /// Call this after the model produces a condensed summary in response to
    /// a [`SessionAction::RequestSummary`] signal.
    pub async fn summarise(&self, session: &SessionId, summary: &str) {
        let key = session.as_str().to_owned();
        let mut state = self
            .sessions
            .entry(key)
            .or_insert_with(|| {
                self.active_sessions.fetch_add(1, Ordering::Relaxed);
                SessionState::new()
            });
        state.turns.clear();
        state.push(
            ConversationTurn {
                role: TurnRole::Assistant,
                content: format!("[Summary of prior conversation]\n{summary}"),
                timestamp: Instant::now(),
            },
            self.config.max_turns,
        );
        info!(session = %session, "replaced history with summary");
    }

    /// Clear all history for a session (e.g. on explicit user logout).
    pub async fn clear_session(&self, session: &SessionId) {
        if self.sessions.remove(session.as_str()).is_some() {
            self.active_sessions.fetch_sub(1, Ordering::Relaxed);
            debug!(session = %session, "session cleared");
        }
    }

    /// Return a snapshot of aggregate statistics.
    pub fn stats(&self) -> SessionStats {
        SessionStats {
            active_sessions: self.sessions.len(),
            enriched_total: self.enriched_total.load(Ordering::Relaxed),
            evicted_total: self.evicted_total.load(Ordering::Relaxed),
        }
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    fn evict_expired(&self) {
        let ttl = self.config.session_ttl;
        let mut evicted = 0u64;
        self.sessions.retain(|_, state| {
            if state.last_activity.elapsed() > ttl {
                evicted += 1;
                false
            } else {
                true
            }
        });
        if evicted > 0 {
            self.evicted_total.fetch_add(evicted, Ordering::Relaxed);
            self.active_sessions
                .fetch_sub(evicted.min(self.active_sessions.load(Ordering::Relaxed)), Ordering::Relaxed);
            debug!(evicted, "evicted expired sessions");
        }
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Aggregate statistics produced by [`SessionContext::stats`].
#[derive(Debug, Clone)]
pub struct SessionStats {
    /// Number of sessions currently held in memory.
    pub active_sessions: usize,
    /// Total number of [`SessionContext::enrich`] calls.
    pub enriched_total: u64,
    /// Total sessions evicted due to TTL.
    pub evicted_total: u64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn make_req(session: &str, input: &str) -> PromptRequest {
        PromptRequest {
            session: SessionId::new(session),
            request_id: uuid::Uuid::new_v4().to_string(),
            input: input.to_owned(),
            meta: HashMap::new(),
            deadline: None,
        }
    }

    #[tokio::test]
    async fn no_history_first_turn() {
        let ctx = SessionContext::new(SessionConfig::default());
        let req = make_req("alice", "Hello!");
        let (enriched, action) = ctx.enrich(req).await;
        assert_eq!(enriched.input, "Hello!");
        assert!(matches!(action, SessionAction::Continue));
    }

    #[tokio::test]
    async fn history_injected_on_second_turn() {
        let ctx = SessionContext::new(SessionConfig::default());
        let (r1, _) = ctx.enrich(make_req("bob", "What is 2+2?")).await;
        ctx.record_response(&r1.session, "It is 4.").await;

        let (r2, _) = ctx.enrich(make_req("bob", "Are you sure?")).await;
        assert!(r2.input.contains("What is 2+2?"), "history not injected");
        assert!(r2.input.contains("It is 4."), "response not injected");
        assert!(r2.input.contains("Are you sure?"), "new turn missing");
    }

    #[tokio::test]
    async fn sessions_are_isolated() {
        let ctx = SessionContext::new(SessionConfig::default());
        ctx.enrich(make_req("alice", "Alice msg")).await;
        ctx.record_response(&SessionId::new("alice"), "Alice resp").await;

        let (bob_req, _) = ctx.enrich(make_req("bob", "Bob msg")).await;
        // Bob should not see Alice's history.
        assert!(!bob_req.input.contains("Alice"), "cross-session leak");
    }

    #[tokio::test]
    async fn summarise_replaces_history() {
        let cfg = SessionConfig {
            max_turns: 10,
            context_window: 10,
            ..Default::default()
        };
        let ctx = SessionContext::new(cfg);
        let s = SessionId::new("carol");
        ctx.enrich(make_req("carol", "Turn 1")).await;
        ctx.record_response(&s, "Resp 1").await;
        ctx.summarise(&s, "Carol asked about Turn 1 and got Resp 1.").await;

        let (r, _) = ctx.enrich(make_req("carol", "Turn 2")).await;
        assert!(r.input.contains("Summary"), "summary not injected");
        assert!(!r.input.contains("Turn 1"), "old raw turns not cleared");
    }

    #[tokio::test]
    async fn clear_session_removes_history() {
        let ctx = SessionContext::new(SessionConfig::default());
        let s = SessionId::new("dave");
        ctx.enrich(make_req("dave", "Hi")).await;
        ctx.record_response(&s, "Hello").await;
        ctx.clear_session(&s).await;

        let (r, _) = ctx.enrich(make_req("dave", "Fresh start")).await;
        assert_eq!(r.input, "Fresh start", "history not cleared");
    }

    #[tokio::test]
    async fn evicts_expired_sessions() {
        let cfg = SessionConfig {
            session_ttl: Duration::from_millis(10),
            ..Default::default()
        };
        let ctx = SessionContext::new(cfg);
        ctx.enrich(make_req("ephemeral", "ping")).await;
        assert_eq!(ctx.stats().active_sessions, 1);

        tokio::time::sleep(Duration::from_millis(20)).await;
        // Trigger eviction via a new enrich call.
        ctx.enrich(make_req("other", "trigger")).await;
        assert_eq!(ctx.stats().evicted_total, 1);
    }

    #[tokio::test]
    async fn summarise_action_at_threshold() {
        let cfg = SessionConfig {
            summarise_after_turns: 3,
            max_turns: 20,
            context_window: 10,
            ..Default::default()
        };
        let ctx = SessionContext::new(cfg);
        let s = SessionId::new("eve");

        ctx.enrich(make_req("eve", "t1")).await;
        ctx.record_response(&s, "r1").await;
        ctx.enrich(make_req("eve", "t2")).await;
        ctx.record_response(&s, "r2").await;
        // After 4 stored turns (2 user + 2 assistant) and turn_count hits threshold.
        let (_, action) = ctx.enrich(make_req("eve", "t3")).await;
        assert!(
            matches!(action, SessionAction::RequestSummary),
            "expected RequestSummary"
        );
    }

    #[test]
    fn stats_zero_on_new() {
        let ctx = SessionContext::new(SessionConfig::default());
        let s = ctx.stats();
        assert_eq!(s.active_sessions, 0);
        assert_eq!(s.enriched_total, 0);
        assert_eq!(s.evicted_total, 0);
    }
}
