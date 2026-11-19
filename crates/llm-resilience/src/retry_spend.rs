//! Retries capped by dollars, not by count.
//!
//! "Retry up to 3 times" says nothing about money. Three retries of a
//! 2,000-token prompt cost a fraction of a cent; three retries of a
//! 200,000-token agent context on a frontier model cost dollars, and during
//! an outage every request in the fleet does it at once. Retries are how an
//! LLM bill triples without anyone changing a line of code.
//!
//! [`RetrySpend`] is a retry budget in USD. Every retry must be paid for
//! from it before it is sent: [`RetrySpend::try_retry`] takes the estimated
//! cost of the attempt and either grants it (and books it) or refuses it.
//! The allowance per window is a fixed floor plus a share of the real spend
//! in that window:
//!
//! ```text
//! allowance = floor_usd + ratio * spend_usd      (per window, default 1 hour)
//! ```
//!
//! So a quiet service can still retry a transient error, a busy one can
//! retry in proportion to what it actually uses, and an outage cannot turn
//! into a retry storm that bills more than `floor + 10%` of the hour's
//! traffic.
//!
//! ```
//! use llm_resilience::RetrySpend;
//!
//! // $0.50 floor, plus 10% of real spend, per hour (the defaults).
//! let budget = RetrySpend::new(0.50);
//! budget.record_spend(10.0); // first attempts cost $10 this hour
//! assert!((budget.allowance_usd() - 1.50).abs() < 1e-9);
//!
//! assert!(budget.try_retry(1.00).is_ok()); // a $1 retry fits
//! let refused = budget.try_retry(0.60).unwrap_err(); // $0.60 more does not
//! assert!(refused.remaining_usd() < 0.60);
//! ```
//!
//! [`retryable_status`] is the other half: which HTTP statuses are worth
//! paying for a second time at all. A 400, 401, 403, 404 or 422 fails the
//! same way on every retry, so retrying it only spends rate limit and
//! latency.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Default share of real spend that retries may add on top of the floor.
pub const DEFAULT_RETRY_RATIO: f64 = 0.10;

/// Default accounting window.
pub const DEFAULT_RETRY_WINDOW: Duration = Duration::from_secs(3600);

/// Whether a response with this HTTP status is worth retrying.
///
/// `true` for 408 (request timeout), 409 (conflict, which OpenAI uses for a
/// transient lock), 425 (too early), 429 (rate limited), 500, 502, 503, 504
/// and 529 (Anthropic's "overloaded"). `false` for every other status: a
/// 4xx says the request itself is wrong (bad parameters, bad key, no
/// access, context too long) and the next attempt fails the same way, and a
/// 2xx or 3xx is not a failure.
///
/// ```
/// use llm_resilience::retryable_status;
///
/// assert!(retryable_status(429));
/// assert!(retryable_status(529));
/// assert!(!retryable_status(400)); // context_length_exceeded stays exceeded
/// assert!(!retryable_status(401));
/// ```
pub const fn retryable_status(status: u16) -> bool {
    matches!(status, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// Returned by [`RetrySpend::try_retry`] when the retry does not fit in
/// what is left of the window's allowance. The retry was not booked.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryRefused {
    estimated_usd: f64,
    remaining_usd: f64,
    allowance_usd: f64,
}

impl RetryRefused {
    /// What the refused retry was estimated to cost.
    pub fn estimated_usd(&self) -> f64 {
        self.estimated_usd
    }

    /// What is left of the allowance in this window.
    pub fn remaining_usd(&self) -> f64 {
        self.remaining_usd
    }

    /// The window's whole allowance (floor plus ratio times spend).
    pub fn allowance_usd(&self) -> f64 {
        self.allowance_usd
    }
}

impl fmt::Display for RetryRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "retry refused: it would cost about ${:.4} and only ${:.4} of the ${:.4} retry budget is left",
            self.estimated_usd, self.remaining_usd, self.allowance_usd
        )
    }
}

impl std::error::Error for RetryRefused {}

/// Counters for a [`RetrySpend`], for the current window and in total.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[non_exhaustive]
pub struct RetrySpendStats {
    /// Real spend recorded in the current window.
    pub window_spend_usd: f64,
    /// Retry spend booked in the current window.
    pub window_retry_usd: f64,
    /// Allowance for the current window.
    pub allowance_usd: f64,
    /// Retries granted since creation.
    pub granted: u64,
    /// Retries refused since creation.
    pub refused: u64,
    /// Retry spend booked since creation.
    pub total_retry_usd: f64,
}

#[derive(Debug)]
struct State {
    window_start: Instant,
    spend_usd: f64,
    retry_usd: f64,
    granted: u64,
    refused: u64,
    total_retry_usd: f64,
}

/// A retry budget in USD. See the [module docs](crate::retry_spend).
///
/// Cheap to clone; clones share the same budget, so one `RetrySpend` can
/// cover every client, route and task in a process.
#[derive(Clone)]
pub struct RetrySpend {
    floor_usd: f64,
    ratio: f64,
    window: Duration,
    state: Arc<Mutex<State>>,
}

impl fmt::Debug for RetrySpend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RetrySpend")
            .field("floor_usd", &self.floor_usd)
            .field("ratio", &self.ratio)
            .field("window", &self.window)
            .field("stats", &self.stats())
            .finish()
    }
}

/// Negative, NaN or infinite amounts count as zero.
fn clean(usd: f64) -> f64 {
    if usd.is_finite() && usd > 0.0 {
        usd
    } else {
        0.0
    }
}

impl RetrySpend {
    /// A budget of `floor_usd` plus 10% of real spend per hour.
    pub fn new(floor_usd: f64) -> Self {
        Self {
            floor_usd: clean(floor_usd),
            ratio: DEFAULT_RETRY_RATIO,
            window: DEFAULT_RETRY_WINDOW,
            state: Arc::new(Mutex::new(State {
                window_start: Instant::now(),
                spend_usd: 0.0,
                retry_usd: 0.0,
                granted: 0,
                refused: 0,
                total_retry_usd: 0.0,
            })),
        }
    }

    /// Change the share of real spend added to the allowance (default
    /// `0.10`). `0.0` makes the floor a hard cap.
    #[must_use]
    pub fn with_ratio(mut self, ratio: f64) -> Self {
        self.ratio = clean(ratio);
        self
    }

    /// Change the accounting window (default one hour). Spend and retry
    /// spend both start from zero when a window ends.
    #[must_use]
    pub fn with_window(mut self, window: Duration) -> Self {
        self.window = window.max(Duration::from_millis(1));
        self
    }

    fn roll(&self, s: &mut State) {
        if s.window_start.elapsed() >= self.window {
            s.window_start = Instant::now();
            s.spend_usd = 0.0;
            s.retry_usd = 0.0;
        }
    }

    fn allowance(&self, s: &State) -> f64 {
        self.floor_usd + self.ratio * s.spend_usd
    }

    /// Record real spend (first attempts, or every call), which raises the
    /// allowance by `ratio` times `usd`. Retries booked with
    /// [`try_retry`](Self::try_retry) are not recorded here again.
    pub fn record_spend(&self, usd: f64) {
        let mut s = self.state.lock();
        self.roll(&mut s);
        s.spend_usd += clean(usd);
    }

    /// Ask to pay for one retry estimated at `estimated_usd`. On `Ok` the
    /// cost is booked against the window's allowance and the caller may
    /// send the retry; on `Err` nothing is booked and the caller should
    /// fail the request with the error it already has.
    ///
    /// A retry estimated at $0 (a free local model) is always granted.
    pub fn try_retry(&self, estimated_usd: f64) -> Result<(), RetryRefused> {
        let cost = clean(estimated_usd);
        let mut s = self.state.lock();
        self.roll(&mut s);
        let allowance = self.allowance(&s);
        let remaining = (allowance - s.retry_usd).max(0.0);
        if cost <= remaining {
            s.retry_usd += cost;
            s.total_retry_usd += cost;
            s.granted += 1;
            Ok(())
        } else {
            s.refused += 1;
            Err(RetryRefused {
                estimated_usd: cost,
                remaining_usd: remaining,
                allowance_usd: allowance,
            })
        }
    }

    /// What is left of this window's allowance.
    pub fn remaining_usd(&self) -> f64 {
        let mut s = self.state.lock();
        self.roll(&mut s);
        (self.allowance(&s) - s.retry_usd).max(0.0)
    }

    /// This window's whole allowance: floor plus ratio times spend.
    pub fn allowance_usd(&self) -> f64 {
        let mut s = self.state.lock();
        self.roll(&mut s);
        self.allowance(&s)
    }

    /// Counters for the current window and since creation.
    pub fn stats(&self) -> RetrySpendStats {
        let mut s = self.state.lock();
        self.roll(&mut s);
        RetrySpendStats {
            window_spend_usd: s.spend_usd,
            window_retry_usd: s.retry_usd,
            allowance_usd: self.allowance(&s),
            granted: s.granted,
            refused: s.refused,
            total_retry_usd: s.total_retry_usd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_alone_caps_retries() {
        let b = RetrySpend::new(1.0);
        assert!(b.try_retry(0.4).is_ok());
        assert!(b.try_retry(0.4).is_ok());
        let e = b.try_retry(0.4).unwrap_err();
        assert!((e.remaining_usd() - 0.2).abs() < 1e-9);
        assert!((e.allowance_usd() - 1.0).abs() < 1e-9);
        let st = b.stats();
        assert_eq!((st.granted, st.refused), (2, 1));
        assert!((st.total_retry_usd - 0.8).abs() < 1e-9);
    }

    #[test]
    fn real_spend_raises_the_allowance() {
        let b = RetrySpend::new(0.0);
        assert!(b.try_retry(0.01).is_err());
        b.record_spend(100.0);
        assert!((b.allowance_usd() - 10.0).abs() < 1e-9);
        assert!(b.try_retry(9.99).is_ok());
        assert!(b.try_retry(0.02).is_err());
    }

    #[test]
    fn free_retries_always_fit_and_bad_numbers_count_as_zero() {
        let b = RetrySpend::new(0.0).with_ratio(0.0);
        assert!(b.try_retry(0.0).is_ok());
        assert!(b.try_retry(-5.0).is_ok());
        assert!(b.try_retry(f64::NAN).is_ok());
        assert!(b.try_retry(f64::INFINITY).is_ok());
        b.record_spend(f64::NAN);
        assert_eq!(b.allowance_usd(), 0.0);
    }

    #[test]
    fn a_new_window_starts_from_zero() {
        let b = RetrySpend::new(0.1).with_window(Duration::from_millis(20));
        assert!(b.try_retry(0.1).is_ok());
        assert!(b.try_retry(0.1).is_err());
        std::thread::sleep(Duration::from_millis(30));
        assert!(b.try_retry(0.1).is_ok());
        assert_eq!(b.stats().granted, 2);
    }

    #[test]
    fn clones_share_one_budget() {
        let a = RetrySpend::new(1.0);
        let b = a.clone();
        assert!(a.try_retry(0.7).is_ok());
        assert!(b.try_retry(0.7).is_err());
    }

    #[test]
    fn statuses() {
        for s in [408, 409, 425, 429, 500, 502, 503, 504, 529] {
            assert!(retryable_status(s), "{s}");
        }
        for s in [200, 301, 400, 401, 402, 403, 404, 413, 422, 501, 505] {
            assert!(!retryable_status(s), "{s}");
        }
    }
}
