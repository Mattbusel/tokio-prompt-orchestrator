//! Per-session token spend budgeting with hard/soft limits and daily reset.
//!
//! ## Overview
//!
//! [`SessionBudget`] tracks cumulative token usage per [`SessionId`] and
//! enforces configurable spending limits:
//!
//! - **Hard limit** — requests are rejected with [`BudgetError::HardLimitExceeded`]
//!   before inference when spend would exceed this cap.
//! - **Soft limit** — requests are allowed but a [`BudgetWarning`] signal is
//!   returned so callers can notify users or throttle gracefully.
//! - **Daily reset** — accumulated spend is zeroed at UTC midnight (or on
//!   explicit call to [`SessionBudget::reset_session`]).
//!
//! ## REST API surface
//!
//! The budget store is intended to back two REST endpoints wired in `web_api.rs`:
//!
//! - `GET  /v1/sessions/{id}/budget`       → [`SessionBudgetSnapshot`]
//! - `POST /v1/sessions/{id}/budget/reset` → resets spend to zero
//!
//! ## Units
//!
//! All limits and accumulated spend are in **tokens** (not USD), because token
//! counts are provider-agnostic and directly observable.  Callers that want
//! cost-based limits should convert via a token-to-cost rate before recording.
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::session::budget::{
//!     SessionBudget, SessionLimits, BudgetOutcome,
//! };
//!
//! let budget = SessionBudget::new();
//! budget.set_limits("alice", SessionLimits {
//!     soft_limit_tokens: 8_000,
//!     hard_limit_tokens: 10_000,
//! });
//!
//! // First request: well within budget
//! let outcome = budget.record_tokens("alice", 500).unwrap();
//! assert!(matches!(outcome, BudgetOutcome::Ok));
//!
//! // Another request pushing past soft limit
//! let outcome = budget.record_tokens("alice", 8_000).unwrap();
//! assert!(matches!(outcome, BudgetOutcome::SoftLimitWarning { .. }));
//!
//! // Reset daily spend
//! budget.reset_session("alice");
//! ```

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use tracing::{debug, warn};

// ============================================================================
// Error types
// ============================================================================

/// Errors returned by [`SessionBudget`] operations.
#[derive(Debug, thiserror::Error)]
pub enum BudgetError {
    /// The request would push accumulated spend past the session hard limit.
    #[error("hard limit exceeded: spent {spent} of {limit} tokens")]
    HardLimitExceeded {
        /// Total tokens that would have been spent after this request.
        spent: u64,
        /// Configured hard limit.
        limit: u64,
    },

    /// The session has already been closed/removed.
    #[error("session '{0}' not found")]
    SessionNotFound(String),
}

// ============================================================================
// Configuration
// ============================================================================

/// Per-session budget limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLimits {
    /// Tokens at which a soft-limit warning is issued (non-blocking).
    pub soft_limit_tokens: u64,
    /// Tokens at which new requests are rejected (hard block).
    pub hard_limit_tokens: u64,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            soft_limit_tokens: 80_000,
            hard_limit_tokens: 100_000,
        }
    }
}

// ============================================================================
// BudgetOutcome
// ============================================================================

/// Outcome of a [`SessionBudget::record_tokens`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetOutcome {
    /// Spend is within all configured limits.
    Ok,
    /// Spend has crossed the soft limit; the request is still allowed.
    SoftLimitWarning {
        /// Accumulated tokens spent this window.
        spent: u64,
        /// Configured soft limit.
        soft_limit: u64,
    },
}

// ============================================================================
// SessionBudgetSnapshot — serialisable for REST responses
// ============================================================================

/// A point-in-time snapshot of a session's budget state.
///
/// Returned by `GET /v1/sessions/{id}/budget`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionBudgetSnapshot {
    /// Session identifier.
    pub session_id: String,
    /// Tokens accumulated this window.
    pub tokens_used: u64,
    /// Soft limit threshold.
    pub soft_limit_tokens: u64,
    /// Hard limit threshold (requests blocked above this).
    pub hard_limit_tokens: u64,
    /// `true` when `tokens_used >= soft_limit_tokens`.
    pub soft_limit_reached: bool,
    /// `true` when `tokens_used >= hard_limit_tokens`.
    pub hard_limit_reached: bool,
    /// UTC timestamp of the last token recording.
    pub last_updated: Option<DateTime<Utc>>,
    /// UTC timestamp of the last manual or automatic reset.
    pub last_reset: Option<DateTime<Utc>>,
    /// Number of requests recorded this window.
    pub request_count: u64,
}

// ============================================================================
// Internal session entry
// ============================================================================

#[derive(Debug)]
struct SessionEntry {
    limits: SessionLimits,
    tokens_used: u64,
    request_count: u64,
    last_updated: Option<DateTime<Utc>>,
    last_reset: Option<DateTime<Utc>>,
}

impl SessionEntry {
    fn new(limits: SessionLimits) -> Self {
        Self {
            limits,
            tokens_used: 0,
            request_count: 0,
            last_updated: None,
            last_reset: None,
        }
    }

    fn reset(&mut self) {
        self.tokens_used = 0;
        self.request_count = 0;
        self.last_reset = Some(Utc::now());
    }

    fn snapshot(&self, session_id: &str) -> SessionBudgetSnapshot {
        SessionBudgetSnapshot {
            session_id: session_id.to_owned(),
            tokens_used: self.tokens_used,
            soft_limit_tokens: self.limits.soft_limit_tokens,
            hard_limit_tokens: self.limits.hard_limit_tokens,
            soft_limit_reached: self.tokens_used >= self.limits.soft_limit_tokens,
            hard_limit_reached: self.tokens_used >= self.limits.hard_limit_tokens,
            last_updated: self.last_updated,
            last_reset: self.last_reset,
            request_count: self.request_count,
        }
    }
}

// ============================================================================
// SessionBudget
// ============================================================================

/// Thread-safe per-session token budget tracker.
///
/// All methods accept the session ID as a `&str` to avoid requiring a
/// [`SessionId`](crate::SessionId) wrapper at the session-management layer.
///
/// # Thread Safety
///
/// Internally uses a `Mutex<HashMap>`.  For very high-throughput workloads
/// consider sharding by session prefix.
#[derive(Debug, Default)]
pub struct SessionBudget {
    sessions: Mutex<HashMap<String, SessionEntry>>,
}

impl SessionBudget {
    /// Create a new, empty budget store.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or update limits for a session.
    ///
    /// If the session already exists its accumulated spend is preserved;
    /// only the limits are updated.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn set_limits(&self, session_id: impl Into<String>, limits: SessionLimits) {
        let id = session_id.into();
        let mut map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(id.clone()).or_insert_with(|| SessionEntry::new(limits.clone()));
        entry.limits = limits;
        debug!(session_id = %id, "budget limits updated");
    }

    /// Remove a session from the budget store entirely.
    ///
    /// Useful when a session is logically closed and its budget data no longer
    /// needs to be retained.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn remove_session(&self, session_id: &str) {
        let mut map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(session_id);
    }

    /// Record `tokens` spent by `session_id` and check against limits.
    ///
    /// ## Hard limit enforcement
    ///
    /// If adding `tokens` would cause accumulated spend to reach or exceed
    /// `hard_limit_tokens`, the spend is **not** recorded and
    /// `Err(BudgetError::HardLimitExceeded)` is returned.  The caller
    /// should reject the underlying inference request with a clear user-facing
    /// error before dispatching to a worker.
    ///
    /// ## Soft limit warning
    ///
    /// If the new accumulated spend crosses `soft_limit_tokens` the function
    /// returns `Ok(BudgetOutcome::SoftLimitWarning { .. })`.  The request
    /// proceeds normally; callers may choose to surface a warning to the user.
    ///
    /// ## Unregistered sessions
    ///
    /// Sessions with no registered limits are silently accepted — all spend
    /// calls return `Ok(BudgetOutcome::Ok)` and no tracking is performed.
    ///
    /// # Errors
    ///
    /// Returns [`BudgetError::HardLimitExceeded`] when the hard limit is hit.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn record_tokens(
        &self,
        session_id: &str,
        tokens: u64,
    ) -> Result<BudgetOutcome, BudgetError> {
        let mut map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());

        let entry = match map.get_mut(session_id) {
            Some(e) => e,
            None => {
                // No limits registered — silently accept
                return Ok(BudgetOutcome::Ok);
            }
        };

        let new_total = entry.tokens_used.saturating_add(tokens);

        // Hard limit check — reject before updating
        if new_total >= entry.limits.hard_limit_tokens {
            warn!(
                session_id,
                spent = new_total,
                limit = entry.limits.hard_limit_tokens,
                "session hard token budget exceeded"
            );
            return Err(BudgetError::HardLimitExceeded {
                spent: new_total,
                limit: entry.limits.hard_limit_tokens,
            });
        }

        // Commit the spend
        entry.tokens_used = new_total;
        entry.request_count += 1;
        entry.last_updated = Some(Utc::now());

        // Soft limit check
        if entry.tokens_used >= entry.limits.soft_limit_tokens {
            warn!(
                session_id,
                tokens_used = entry.tokens_used,
                soft_limit = entry.limits.soft_limit_tokens,
                "session soft token budget reached"
            );
            Ok(BudgetOutcome::SoftLimitWarning {
                spent: entry.tokens_used,
                soft_limit: entry.limits.soft_limit_tokens,
            })
        } else {
            Ok(BudgetOutcome::Ok)
        }
    }

    /// Reset accumulated spend to zero without changing the session limits.
    ///
    /// This is the operation behind `POST /v1/sessions/{id}/budget/reset`.
    ///
    /// Does nothing if the session is not registered (no-op, not an error).
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn reset_session(&self, session_id: &str) {
        let mut map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = map.get_mut(session_id) {
            entry.reset();
            debug!(session_id, "session budget reset");
        }
    }

    /// Perform a daily reset: zero the spend of every session whose
    /// `last_reset` was more than 24 hours ago (or was never set).
    ///
    /// Intended to be called from a background task, e.g.:
    ///
    /// ```rust,no_run
    /// use tokio::time::{interval, Duration};
    /// use tokio_prompt_orchestrator::session::budget::SessionBudget;
    /// use std::sync::Arc;
    ///
    /// async fn daily_reset_task(budget: Arc<SessionBudget>) {
    ///     let mut ticker = interval(Duration::from_secs(60 * 60)); // check hourly
    ///     loop {
    ///         ticker.tick().await;
    ///         let count = budget.daily_reset();
    ///         if count > 0 {
    ///             tracing::info!(count, "daily budget reset applied to sessions");
    ///         }
    ///     }
    /// }
    /// ```
    ///
    /// # Returns
    ///
    /// Number of sessions whose budget was reset.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn daily_reset(&self) -> usize {
        let now = Utc::now();
        let threshold = chrono::Duration::hours(24);
        let mut map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let mut count = 0usize;

        for (id, entry) in map.iter_mut() {
            let needs_reset = match entry.last_reset {
                None => true,
                Some(last) => now.signed_duration_since(last) >= threshold,
            };
            if needs_reset {
                entry.reset();
                count += 1;
                debug!(session_id = %id, "daily budget reset applied");
            }
        }

        count
    }

    /// Return a snapshot of the given session's budget.
    ///
    /// This is the backing data for `GET /v1/sessions/{id}/budget`.
    ///
    /// Returns `None` if the session has not been registered.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn snapshot(&self, session_id: &str) -> Option<SessionBudgetSnapshot> {
        let map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        map.get(session_id).map(|e| e.snapshot(session_id))
    }

    /// Return snapshots for all tracked sessions.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn all_snapshots(&self) -> Vec<SessionBudgetSnapshot> {
        let map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        map.iter().map(|(id, e)| e.snapshot(id)).collect()
    }

    /// Return the number of sessions currently tracked.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn session_count(&self) -> usize {
        let map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        map.len()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn default_limits() -> SessionLimits {
        SessionLimits {
            soft_limit_tokens: 100,
            hard_limit_tokens: 200,
        }
    }

    #[test]
    fn ok_within_budget() {
        let b = SessionBudget::new();
        b.set_limits("s1", default_limits());
        let outcome = b.record_tokens("s1", 50).unwrap();
        assert_eq!(outcome, BudgetOutcome::Ok);
    }

    #[test]
    fn soft_limit_warning_issued() {
        let b = SessionBudget::new();
        b.set_limits("s1", default_limits());
        b.record_tokens("s1", 80).unwrap();
        let outcome = b.record_tokens("s1", 30).unwrap(); // crosses 100
        assert!(matches!(outcome, BudgetOutcome::SoftLimitWarning { .. }));
    }

    #[test]
    fn hard_limit_blocks_request() {
        let b = SessionBudget::new();
        b.set_limits("s1", default_limits());
        b.record_tokens("s1", 150).unwrap();
        let err = b.record_tokens("s1", 60).unwrap_err();
        assert!(matches!(err, BudgetError::HardLimitExceeded { .. }));
    }

    #[test]
    fn unregistered_session_accepted() {
        let b = SessionBudget::new();
        let outcome = b.record_tokens("unknown", 9999).unwrap();
        assert_eq!(outcome, BudgetOutcome::Ok);
    }

    #[test]
    fn reset_clears_spend() {
        let b = SessionBudget::new();
        b.set_limits("s1", default_limits());
        b.record_tokens("s1", 150).unwrap();
        b.reset_session("s1");
        let snap = b.snapshot("s1").unwrap();
        assert_eq!(snap.tokens_used, 0);
        assert!(!snap.soft_limit_reached);
    }

    #[test]
    fn snapshot_reflects_state() {
        let b = SessionBudget::new();
        b.set_limits("s1", default_limits());
        b.record_tokens("s1", 120).unwrap();
        let snap = b.snapshot("s1").unwrap();
        assert_eq!(snap.tokens_used, 120);
        assert!(snap.soft_limit_reached);
        assert!(!snap.hard_limit_reached);
        assert_eq!(snap.request_count, 1);
    }

    #[test]
    fn update_limits_preserves_spend() {
        let b = SessionBudget::new();
        b.set_limits("s1", default_limits());
        b.record_tokens("s1", 50).unwrap();
        // Raise hard limit
        b.set_limits("s1", SessionLimits {
            soft_limit_tokens: 200,
            hard_limit_tokens: 500,
        });
        let snap = b.snapshot("s1").unwrap();
        assert_eq!(snap.tokens_used, 50); // spend preserved
        assert_eq!(snap.hard_limit_tokens, 500);
    }

    #[test]
    fn session_count_tracks_correctly() {
        let b = SessionBudget::new();
        assert_eq!(b.session_count(), 0);
        b.set_limits("a", default_limits());
        b.set_limits("b", default_limits());
        assert_eq!(b.session_count(), 2);
        b.remove_session("a");
        assert_eq!(b.session_count(), 1);
    }

    #[test]
    fn daily_reset_zeroes_all_sessions() {
        let b = SessionBudget::new();
        b.set_limits("s1", default_limits());
        b.set_limits("s2", default_limits());
        b.record_tokens("s1", 50).unwrap();
        b.record_tokens("s2", 80).unwrap();
        // Force reset by calling daily_reset (last_reset is None so all qualify)
        let count = b.daily_reset();
        assert_eq!(count, 2);
        assert_eq!(b.snapshot("s1").unwrap().tokens_used, 0);
        assert_eq!(b.snapshot("s2").unwrap().tokens_used, 0);
    }
}
