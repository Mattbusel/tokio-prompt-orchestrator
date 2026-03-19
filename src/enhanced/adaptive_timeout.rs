//! Adaptive request timeout based on a rolling P95 of recent durations.
//!
//! ## Behaviour
//!
//! [`AdaptiveTimeout`] maintains a circular buffer of the last 100 request
//! durations and computes the current timeout as:
//!
//! ```text
//! timeout = max(config_min_timeout, p95 * 2.0)
//! ```
//!
//! When fewer than 2 samples are present the configured minimum is returned,
//! ensuring the system starts conservatively.
//!
//! ## Usage
//!
//! ```rust
//! use std::time::Duration;
//! use tokio_prompt_orchestrator::enhanced::AdaptiveTimeout;
//!
//! let mut at = AdaptiveTimeout::new(Duration::from_secs(5));
//! at.record_duration(Duration::from_millis(200));
//! at.record_duration(Duration::from_millis(180));
//!
//! // Returns >= 5 s (the configured minimum)
//! let timeout = at.current_timeout();
//! assert!(timeout >= Duration::from_secs(5));
//! ```

use std::time::Duration;

/// Capacity of the circular duration buffer.
const BUFFER_CAPACITY: usize = 100;

/// P95 percentile index in a sorted 100-element array (0-indexed).
/// index 94 is the 95th percentile for 100 samples.
const P95_INDEX_100: usize = 94;

/// Adaptive timeout calculator backed by a circular buffer of recent durations.
///
/// # Panics
///
/// This type never panics.
#[derive(Debug, Clone)]
pub struct AdaptiveTimeout {
    /// Circular buffer storing the most recent request durations (nanoseconds).
    buffer: [u64; BUFFER_CAPACITY],
    /// Write head (next position to write).
    head: usize,
    /// Number of valid samples currently in the buffer (saturates at `BUFFER_CAPACITY`).
    count: usize,
    /// Minimum timeout returned regardless of the computed P95 value.
    min_timeout: Duration,
}

impl AdaptiveTimeout {
    /// Creates a new `AdaptiveTimeout` with the given minimum timeout floor.
    ///
    /// # Arguments
    /// * `min_timeout` — Minimum timeout value. The computed `p95 * 2.0` will
    ///   never go below this value.
    pub fn new(min_timeout: Duration) -> Self {
        Self {
            buffer: [0u64; BUFFER_CAPACITY],
            head: 0,
            count: 0,
            min_timeout,
        }
    }

    /// Records a completed request duration into the rolling buffer.
    ///
    /// Once the buffer is full the oldest sample is overwritten (circular).
    pub fn record_duration(&mut self, duration: Duration) {
        self.buffer[self.head] = duration.as_nanos() as u64;
        self.head = (self.head + 1) % BUFFER_CAPACITY;
        if self.count < BUFFER_CAPACITY {
            self.count += 1;
        }
    }

    /// Returns the current adaptive timeout.
    ///
    /// With fewer than 2 samples the configured `min_timeout` is returned.
    /// Otherwise: `max(min_timeout, p95 * 2.0)`.
    pub fn current_timeout(&self) -> Duration {
        if self.count < 2 {
            return self.min_timeout;
        }

        let p95_ns = self.compute_p95();
        let adaptive_ns = (p95_ns as f64 * 2.0) as u64;
        let adaptive = Duration::from_nanos(adaptive_ns);

        adaptive.max(self.min_timeout)
    }

    /// Returns the number of samples currently recorded.
    pub fn sample_count(&self) -> usize {
        self.count
    }

    /// Computes the P95 of the current sample window.
    ///
    /// Sorts a copy of the active portion of the buffer and returns the value
    /// at index `floor(0.95 * count)`.
    fn compute_p95(&self) -> u64 {
        let n = self.count;
        let mut samples = vec![0u64; n];

        // The active window may wrap around; copy in logical order.
        if self.count < BUFFER_CAPACITY {
            // Buffer not yet full: samples are in [0..count].
            samples[..n].copy_from_slice(&self.buffer[..n]);
        } else {
            // Buffer full: head points to the oldest entry.
            let tail = BUFFER_CAPACITY - self.head;
            samples[..tail].copy_from_slice(&self.buffer[self.head..]);
            samples[tail..].copy_from_slice(&self.buffer[..self.head]);
        }

        samples.sort_unstable();

        let p95_idx = if n == BUFFER_CAPACITY {
            P95_INDEX_100
        } else {
            // For smaller sample counts: floor(0.95 * n), clamped to n-1.
            ((n as f64 * 0.95) as usize).min(n - 1)
        };

        samples[p95_idx]
    }
}

// ── Unit tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_returns_min_timeout() {
        let min = Duration::from_secs(10);
        let at = AdaptiveTimeout::new(min);
        assert_eq!(at.current_timeout(), min);
        assert_eq!(at.sample_count(), 0);
    }

    #[test]
    fn test_single_sample_returns_min_timeout() {
        let min = Duration::from_secs(10);
        let mut at = AdaptiveTimeout::new(min);
        at.record_duration(Duration::from_millis(50));
        assert_eq!(at.current_timeout(), min);
    }

    #[test]
    fn test_adaptive_timeout_above_min_when_p95_large() {
        let min = Duration::from_secs(1);
        let mut at = AdaptiveTimeout::new(min);

        // Insert 100 samples: 93 fast (100ms) + 7 very slow (5s).
        // After sorting, indices 93–99 are the 7 slow values.
        // P95 = index 94 (0-based) = a slow value (5s).
        // timeout = max(1s, 5s * 2) = 10s.
        for _ in 0..93 {
            at.record_duration(Duration::from_millis(100));
        }
        for _ in 0..7 {
            at.record_duration(Duration::from_millis(5_000)); // 5s outliers
        }

        let timeout = at.current_timeout();
        // p95 = 5s; timeout = 10s >> 1s min.
        assert!(
            timeout >= Duration::from_secs(5),
            "timeout {timeout:?} should be >= 5s when p95 is ~5s"
        );
    }

    #[test]
    fn test_min_timeout_floor_respected() {
        let min = Duration::from_secs(30);
        let mut at = AdaptiveTimeout::new(min);

        // All samples are tiny — p95 * 2 < min.
        for _ in 0..100 {
            at.record_duration(Duration::from_nanos(1));
        }

        assert_eq!(
            at.current_timeout(),
            min,
            "min_timeout floor must not be undercut"
        );
    }

    #[test]
    fn test_circular_buffer_overwrites_oldest() {
        let min = Duration::from_millis(100);
        let mut at = AdaptiveTimeout::new(min);

        // Fill with large values.
        for _ in 0..BUFFER_CAPACITY {
            at.record_duration(Duration::from_secs(10));
        }

        // Overwrite all with small values.
        for _ in 0..BUFFER_CAPACITY {
            at.record_duration(Duration::from_nanos(1));
        }

        // After overwrite, p95 * 2 should be tiny, so min_timeout wins.
        assert_eq!(at.current_timeout(), min);
    }

    #[test]
    fn test_sample_count_saturates_at_capacity() {
        let mut at = AdaptiveTimeout::new(Duration::from_secs(1));
        for i in 0..200 {
            at.record_duration(Duration::from_millis(i as u64));
        }
        assert_eq!(at.sample_count(), BUFFER_CAPACITY);
    }

    #[test]
    fn test_p95_computed_correctly_for_uniform_distribution() {
        let min = Duration::from_millis(1);
        let mut at = AdaptiveTimeout::new(min);

        // Insert exactly 100 samples: values 1..=100 ms.
        for i in 1u64..=100 {
            at.record_duration(Duration::from_millis(i));
        }

        // P95 of [1..100] ms = 95ms; timeout = max(1ms, 95ms * 2) = 190ms.
        let timeout = at.current_timeout();
        assert_eq!(timeout, Duration::from_millis(190));
    }
}
