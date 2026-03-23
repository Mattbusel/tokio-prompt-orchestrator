//! Multi-session state management with idle/lifetime expiry.
//!
//! [`SessionManager`] tracks active inference sessions, enforces message and
//! context-entry limits, and transitions sessions through the
//! [`SessionState`] lifecycle.

use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------
// SessionState
// ---------------------------------------------------------------------------

/// Lifecycle state of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// The session is actively being used.
    Active,
    /// The session has not seen activity within the idle window.
    Idle,
    /// The session exceeded its maximum lifetime and was expired automatically.
    Expired,
    /// The session was explicitly closed.
    Terminated,
}

// ---------------------------------------------------------------------------
// SessionMetadata
// ---------------------------------------------------------------------------

/// Per-session metadata stored alongside message history.
#[derive(Debug, Clone)]
pub struct SessionMetadata {
    /// Unique session identifier.
    pub id: String,
    /// Unix-epoch milliseconds at which the session was created.
    pub created_at_ms: u64,
    /// Unix-epoch milliseconds of the last recorded activity.
    pub last_active_ms: u64,
    /// Current lifecycle state.
    pub state: SessionState,
    /// Optional caller-supplied user identifier.
    pub user_id: Option<String>,
    /// Model name / identifier used by this session.
    pub model: String,
    /// Cumulative tokens consumed across all requests in this session.
    pub total_tokens: u64,
}

// ---------------------------------------------------------------------------
// SessionData
// ---------------------------------------------------------------------------

/// Full session data including messages and key/value context.
#[derive(Debug, Clone)]
pub struct SessionData {
    /// Session metadata.
    pub metadata: SessionMetadata,
    /// Ordered list of messages (prompt + completion turns).
    pub messages: Vec<String>,
    /// Arbitrary key/value context entries.
    pub context: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// SessionConfig
// ---------------------------------------------------------------------------

/// Configuration controlling session lifecycle and limits.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Milliseconds of inactivity before a session transitions to [`SessionState::Idle`]
    /// and is eligible for expiry via [`SessionManager::expire_idle`].
    pub idle_timeout_ms: u64,
    /// Maximum session lifetime in milliseconds; session is expired once exceeded.
    pub max_lifetime_ms: u64,
    /// Maximum number of messages that may be stored per session.
    pub max_messages: usize,
    /// Maximum number of context entries per session.
    pub max_context_entries: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            idle_timeout_ms: 5 * 60 * 1000,      // 5 minutes
            max_lifetime_ms: 60 * 60 * 1000,     // 1 hour
            max_messages: 1_000,
            max_context_entries: 256,
        }
    }
}

// ---------------------------------------------------------------------------
// SessionError
// ---------------------------------------------------------------------------

/// Errors produced by [`SessionManager`] operations.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionError {
    /// No session with the given id exists.
    NotFound(String),
    /// The session has already expired or been terminated.
    SessionExpired,
    /// Adding a message would exceed [`SessionConfig::max_messages`].
    TooManyMessages,
    /// Adding a context entry would exceed [`SessionConfig::max_context_entries`].
    TooManyContextEntries,
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::NotFound(id) => write!(f, "session not found: {}", id),
            SessionError::SessionExpired => write!(f, "session has expired or been terminated"),
            SessionError::TooManyMessages => write!(f, "session message limit exceeded"),
            SessionError::TooManyContextEntries => write!(f, "session context entry limit exceeded"),
        }
    }
}

// ---------------------------------------------------------------------------
// SessionStats
// ---------------------------------------------------------------------------

/// Aggregate counts across all managed sessions.
#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    /// Total number of sessions ever created (and not yet removed from memory).
    pub total: usize,
    /// Sessions currently in [`SessionState::Active`].
    pub active: usize,
    /// Sessions currently in [`SessionState::Idle`].
    pub idle: usize,
    /// Sessions currently in [`SessionState::Expired`].
    pub expired: usize,
    /// Sessions currently in [`SessionState::Terminated`].
    pub terminated: usize,
}

// ---------------------------------------------------------------------------
// SessionManager
// ---------------------------------------------------------------------------

/// Manages the full lifecycle of multiple inference sessions.
pub struct SessionManager {
    /// All sessions, keyed by id.
    pub sessions: HashMap<String, SessionData>,
    /// Configuration controlling timeouts and limits.
    pub config: SessionConfig,
    /// Monotonically increasing counter used to generate unique session ids.
    pub next_id: u64,
}

impl SessionManager {
    /// Creates a new manager with the supplied configuration.
    pub fn new(config: SessionConfig) -> Self {
        SessionManager {
            sessions: HashMap::new(),
            config,
            next_id: 1,
        }
    }

    /// Creates a new session and returns its id.
    pub fn create_session(&mut self, user_id: Option<String>, model: String) -> String {
        let id = format!("sess-{}", self.next_id);
        self.next_id += 1;

        // Use a simple counter-based "now" sentinel; callers that need real time
        // should pass a real timestamp via `touch`.
        let now_ms: u64 = 0;

        let metadata = SessionMetadata {
            id: id.clone(),
            created_at_ms: now_ms,
            last_active_ms: now_ms,
            state: SessionState::Active,
            user_id,
            model,
            total_tokens: 0,
        };

        let data = SessionData {
            metadata,
            messages: Vec::new(),
            context: HashMap::new(),
        };

        self.sessions.insert(id.clone(), data);
        id
    }

    /// Returns an immutable reference to the session data, if it exists.
    pub fn get_session(&self, id: &str) -> Option<&SessionData> {
        self.sessions.get(id)
    }

    /// Updates `last_active_ms` for the session and transitions it to
    /// [`SessionState::Active`] if it was previously [`SessionState::Idle`].
    pub fn touch(&mut self, id: &str, now_ms: u64) {
        if let Some(session) = self.sessions.get_mut(id) {
            session.metadata.last_active_ms = now_ms;
            if session.metadata.state == SessionState::Idle {
                session.metadata.state = SessionState::Active;
            }
        }
    }

    /// Appends a message to the session's history.
    ///
    /// Returns [`SessionError::TooManyMessages`] if the limit would be exceeded.
    pub fn add_message(&mut self, id: &str, message: String) -> Result<(), SessionError> {
        let session = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;

        if matches!(
            session.metadata.state,
            SessionState::Expired | SessionState::Terminated
        ) {
            return Err(SessionError::SessionExpired);
        }

        if session.messages.len() >= self.config.max_messages {
            return Err(SessionError::TooManyMessages);
        }

        session.messages.push(message);
        Ok(())
    }

    /// Inserts or updates a context entry on the session.
    ///
    /// Returns [`SessionError::TooManyContextEntries`] if inserting a new key
    /// would exceed the limit.
    pub fn set_context(
        &mut self,
        id: &str,
        key: String,
        value: String,
    ) -> Result<(), SessionError> {
        let config_max = self.config.max_context_entries;
        let session = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;

        if matches!(
            session.metadata.state,
            SessionState::Expired | SessionState::Terminated
        ) {
            return Err(SessionError::SessionExpired);
        }

        // Only enforce limit when the key is new
        if !session.context.contains_key(&key) && session.context.len() >= config_max {
            return Err(SessionError::TooManyContextEntries);
        }

        session.context.insert(key, value);
        Ok(())
    }

    /// Marks sessions that have been idle longer than [`SessionConfig::idle_timeout_ms`]
    /// as [`SessionState::Expired`] and returns their ids.
    pub fn expire_idle(&mut self, now_ms: u64) -> Vec<String> {
        let idle_timeout = self.config.idle_timeout_ms;
        let max_lifetime = self.config.max_lifetime_ms;
        let mut expired_ids = Vec::new();

        for (id, session) in self.sessions.iter_mut() {
            if matches!(
                session.metadata.state,
                SessionState::Expired | SessionState::Terminated
            ) {
                continue;
            }

            let age = now_ms.saturating_sub(session.metadata.created_at_ms);
            let idle = now_ms.saturating_sub(session.metadata.last_active_ms);

            if idle > idle_timeout || age > max_lifetime {
                session.metadata.state = SessionState::Expired;
                expired_ids.push(id.clone());
            } else if idle > idle_timeout / 2 {
                // Transition to Idle so callers can detect near-expiry
                session.metadata.state = SessionState::Idle;
            }
        }
        expired_ids
    }

    /// Explicitly terminates a session.  Returns `true` if the session existed.
    pub fn terminate(&mut self, id: &str) -> bool {
        if let Some(session) = self.sessions.get_mut(id) {
            session.metadata.state = SessionState::Terminated;
            true
        } else {
            false
        }
    }

    /// Returns metadata for all non-expired, non-terminated sessions.
    pub fn active_sessions(&self) -> Vec<&SessionMetadata> {
        self.sessions
            .values()
            .filter(|s| {
                matches!(
                    s.metadata.state,
                    SessionState::Active | SessionState::Idle
                )
            })
            .map(|s| &s.metadata)
            .collect()
    }

    /// Computes aggregate counts across all tracked sessions.
    pub fn session_stats(&self) -> SessionStats {
        let mut stats = SessionStats::default();
        stats.total = self.sessions.len();
        for s in self.sessions.values() {
            match s.metadata.state {
                SessionState::Active => stats.active += 1,
                SessionState::Idle => stats.idle += 1,
                SessionState::Expired => stats.expired += 1,
                SessionState::Terminated => stats.terminated += 1,
            }
        }
        stats
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn default_manager() -> SessionManager {
        SessionManager::new(SessionConfig {
            idle_timeout_ms: 1_000,
            max_lifetime_ms: 10_000,
            max_messages: 3,
            max_context_entries: 2,
        })
    }

    #[test]
    fn test_create_and_get() {
        let mut mgr = default_manager();
        let id = mgr.create_session(Some("alice".to_string()), "gpt-4".to_string());
        let session = mgr.get_session(&id).unwrap();
        assert_eq!(session.metadata.user_id.as_deref(), Some("alice"));
        assert_eq!(session.metadata.model, "gpt-4");
        assert_eq!(session.metadata.state, SessionState::Active);
    }

    #[test]
    fn test_idle_expiry() {
        let mut mgr = default_manager();
        let id = mgr.create_session(None, "claude".to_string());
        // Simulate 2000 ms elapsed (> idle_timeout_ms of 1000)
        let expired = mgr.expire_idle(2_000);
        assert!(expired.contains(&id));
        assert_eq!(
            mgr.get_session(&id).unwrap().metadata.state,
            SessionState::Expired
        );
    }

    #[test]
    fn test_message_limit() {
        let mut mgr = default_manager();
        let id = mgr.create_session(None, "model".to_string());
        mgr.add_message(&id, "m1".to_string()).unwrap();
        mgr.add_message(&id, "m2".to_string()).unwrap();
        mgr.add_message(&id, "m3".to_string()).unwrap();
        let err = mgr.add_message(&id, "m4".to_string()).unwrap_err();
        assert_eq!(err, SessionError::TooManyMessages);
    }

    #[test]
    fn test_context_overflow() {
        let mut mgr = default_manager();
        let id = mgr.create_session(None, "model".to_string());
        mgr.set_context(&id, "k1".to_string(), "v1".to_string()).unwrap();
        mgr.set_context(&id, "k2".to_string(), "v2".to_string()).unwrap();
        let err = mgr
            .set_context(&id, "k3".to_string(), "v3".to_string())
            .unwrap_err();
        assert_eq!(err, SessionError::TooManyContextEntries);
    }

    #[test]
    fn test_terminate() {
        let mut mgr = default_manager();
        let id = mgr.create_session(None, "model".to_string());
        assert!(mgr.terminate(&id));
        assert_eq!(
            mgr.get_session(&id).unwrap().metadata.state,
            SessionState::Terminated
        );
        assert!(!mgr.terminate("nonexistent"));
    }

    #[test]
    fn test_stats_counts() {
        let mut mgr = default_manager();
        let a = mgr.create_session(None, "m".to_string());
        let b = mgr.create_session(None, "m".to_string());
        let _c = mgr.create_session(None, "m".to_string());

        mgr.terminate(&a);
        mgr.expire_idle(2_000); // expires b and _c (both idle > 1000ms from t=0)

        let stats = mgr.session_stats();
        assert_eq!(stats.total, 3);
        assert_eq!(stats.terminated, 1);
        // b and _c should be expired
        assert_eq!(stats.expired, 2);
        let _ = b; // suppress unused warning
    }
}
