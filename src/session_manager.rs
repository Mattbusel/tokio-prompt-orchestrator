//! Session tracking and context-window management.
//!
//! [`SessionManager`] owns a collection of [`Session`] objects behind a
//! `RwLock<HashMap>`.  Each session stores a rolling [`VecDeque`] of
//! [`Message`] records and enforces a maximum context-token budget.

use std::collections::{HashMap, VecDeque};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    RwLock,
};
use std::time::Instant;

// ── Message ───────────────────────────────────────────────────────────────────

/// A single turn in a conversation session.
#[derive(Debug, Clone)]
pub struct Message {
    /// Conversation role, e.g. `"user"` or `"assistant"`.
    pub role: String,
    /// Raw text content of the message.
    pub content: String,
    /// Token count for this message.
    pub tokens: usize,
    /// Wall-clock time at which the message was recorded.
    pub timestamp: Instant,
}

// ── Session ───────────────────────────────────────────────────────────────────

/// A conversation session with bounded context-window management.
#[derive(Debug)]
pub struct Session {
    /// Unique session identifier.
    pub id: String,
    /// Model associated with this session.
    pub model: String,
    /// Ordered message history (oldest first).
    pub messages: VecDeque<Message>,
    /// Running sum of tokens across all messages currently in `messages`.
    pub total_tokens: usize,
    /// Hard ceiling on context tokens; messages are trimmed to stay below this.
    pub max_context_tokens: usize,
    /// Wall-clock time at which the session was created.
    pub created_at: Instant,
    /// Wall-clock time of the last `add_message` call.
    pub last_active: Instant,
}

impl Session {
    /// Create a new empty session.
    fn new(id: String, model: String, max_context_tokens: usize) -> Self {
        let now = Instant::now();
        Self {
            id,
            model,
            messages: VecDeque::new(),
            total_tokens: 0,
            max_context_tokens,
            created_at: now,
            last_active: now,
        }
    }

    /// Append a message and update the running token total.
    ///
    /// Trims the oldest messages first if adding `tokens` would overflow the
    /// context window.
    pub fn add_message(&mut self, role: String, content: String, tokens: usize) {
        self.trim_to_fit(tokens);
        self.total_tokens += tokens;
        self.last_active = Instant::now();
        self.messages.push_back(Message {
            role,
            content,
            tokens,
            timestamp: self.last_active,
        });
    }

    /// Drop the oldest messages until `total_tokens + new_tokens` fits within
    /// `max_context_tokens`.
    pub fn trim_to_fit(&mut self, new_tokens: usize) {
        while self.total_tokens + new_tokens > self.max_context_tokens {
            if let Some(dropped) = self.messages.pop_front() {
                self.total_tokens = self.total_tokens.saturating_sub(dropped.tokens);
            } else {
                break;
            }
        }
    }

    /// Fraction of the context window currently occupied (`0.0`–`1.0`).
    pub fn token_utilization(&self) -> f64 {
        if self.max_context_tokens == 0 {
            return 0.0;
        }
        self.total_tokens as f64 / self.max_context_tokens as f64
    }

    /// Returns `true` if the session has not been active for longer than
    /// `timeout_secs` seconds.
    pub fn is_expired(&self, timeout_secs: u64) -> bool {
        self.last_active.elapsed().as_secs() >= timeout_secs
    }
}

// ── SessionManager ────────────────────────────────────────────────────────────

/// Thread-safe store for active [`Session`] objects.
///
/// Session IDs are generated from a monotonically increasing counter combined
/// with a timestamp so they are unique and roughly sortable by creation time.
pub struct SessionManager {
    sessions: RwLock<HashMap<String, Session>>,
    counter: AtomicU64,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    /// Create an empty session manager.
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            counter: AtomicU64::new(0),
        }
    }

    /// Generate a UUID-like session ID from the current timestamp + counter.
    fn gen_id(&self) -> String {
        let seq = self.counter.fetch_add(1, Ordering::Relaxed);
        // Use elapsed nanos from a fixed reference + counter for uniqueness.
        // This avoids a dependency on `uuid` while remaining collision-free
        // within a single process.
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("sess-{ts:x}-{seq:04x}")
    }

    /// Create a new session for `model` with the given context-token limit.
    ///
    /// Returns the new session ID.
    pub fn create_session(&self, model: &str, max_context: usize) -> String {
        let id = self.gen_id();
        let session = Session::new(id.clone(), model.to_owned(), max_context);
        self.sessions.write().unwrap().insert(id.clone(), session);
        id
    }

    /// Return a snapshot of the messages currently held in the session, or
    /// `None` if the session does not exist.
    pub fn get_context(&self, session_id: &str) -> Option<Vec<Message>> {
        self.sessions
            .read()
            .unwrap()
            .get(session_id)
            .map(|s| s.messages.iter().cloned().collect())
    }

    /// Append a user turn and an assistant turn to the session.
    ///
    /// `user_msg_tokens` and `assistant_msg_tokens` carry placeholder content
    /// strings; callers that need real content should use
    /// [`Session::add_message`] directly after obtaining a write lock.
    pub fn add_turn(
        &self,
        session_id: &str,
        user_msg_tokens: usize,
        assistant_msg_tokens: usize,
    ) {
        let mut guard = self.sessions.write().unwrap();
        if let Some(session) = guard.get_mut(session_id) {
            session.add_message("user".to_owned(), String::new(), user_msg_tokens);
            session.add_message(
                "assistant".to_owned(),
                String::new(),
                assistant_msg_tokens,
            );
        }
    }

    /// Remove sessions that have been inactive for longer than `timeout_secs`.
    ///
    /// Returns the number of sessions removed.
    pub fn expire_sessions(&self, timeout_secs: u64) -> usize {
        let mut guard = self.sessions.write().unwrap();
        let before = guard.len();
        guard.retain(|_, s| !s.is_expired(timeout_secs));
        before - guard.len()
    }

    /// Return the number of active sessions.
    pub fn active_count(&self) -> usize {
        self.sessions.read().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_retrieve_session() {
        let mgr = SessionManager::new();
        let id = mgr.create_session("gpt-4o", 4096);
        assert_eq!(mgr.active_count(), 1);
        let ctx = mgr.get_context(&id);
        assert!(ctx.is_some());
        assert!(ctx.unwrap().is_empty());
    }

    #[test]
    fn add_turn_populates_context() {
        let mgr = SessionManager::new();
        let id = mgr.create_session("claude-3", 1000);
        mgr.add_turn(&id, 50, 80);
        let ctx = mgr.get_context(&id).unwrap();
        assert_eq!(ctx.len(), 2);
        assert_eq!(ctx[0].role, "user");
        assert_eq!(ctx[1].role, "assistant");
    }

    #[test]
    fn session_trim_to_fit() {
        let mut session = Session::new("s1".into(), "m".into(), 100);
        session.add_message("user".into(), "hello".into(), 60);
        session.add_message("assistant".into(), "hi".into(), 60);
        // Second add should have evicted the first message to stay within 100
        assert!(session.total_tokens <= 100);
    }

    #[test]
    fn expire_sessions() {
        let mgr = SessionManager::new();
        let _id = mgr.create_session("gpt-4o", 1000);
        // A timeout of 0 seconds means every session is expired immediately.
        let removed = mgr.expire_sessions(0);
        assert_eq!(removed, 1);
        assert_eq!(mgr.active_count(), 0);
    }
}
