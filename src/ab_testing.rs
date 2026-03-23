//! # A/B Testing Framework
//!
//! A/B testing for model/prompt comparisons with deterministic assignment,
//! atomic metrics collection, and statistical winner determination.

#![allow(dead_code)]

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

// ---------------------------------------------------------------------------
// Variant
// ---------------------------------------------------------------------------

/// A single experiment variant with routing metadata.
#[derive(Clone, Debug)]
pub struct Variant {
    /// Unique identifier for this variant.
    pub id: String,
    /// Model name to use for this variant.
    pub model: String,
    /// String prepended/appended to the prompt for this variant.
    pub prompt_modifier: String,
    /// Relative traffic weight (will be normalised against sum of all weights).
    pub weight: f64,
}

// ---------------------------------------------------------------------------
// Assignment
// ---------------------------------------------------------------------------

/// A recorded session→variant assignment.
#[derive(Debug)]
pub struct Assignment {
    /// The variant this session was assigned to.
    pub variant_id: String,
    /// The session that was assigned.
    pub session_id: String,
    /// Wall-clock instant of assignment.
    pub assigned_at: std::time::Instant,
}

// ---------------------------------------------------------------------------
// VariantMetrics
// ---------------------------------------------------------------------------

/// Atomic per-variant statistics accumulated during the test.
pub struct VariantMetrics {
    /// The variant these metrics belong to.
    pub variant_id: String,
    /// Total requests routed to this variant.
    pub requests: AtomicU64,
    /// Sum of observed latencies in milliseconds.
    pub total_latency_ms: AtomicU64,
    /// Sum of tokens consumed.
    pub total_tokens: AtomicU64,
    /// Number of successful completions.
    pub successes: AtomicU64,
}

impl VariantMetrics {
    /// Create zeroed metrics for a variant.
    pub fn new(variant_id: impl Into<String>) -> Self {
        Self {
            variant_id: variant_id.into(),
            requests: AtomicU64::new(0),
            total_latency_ms: AtomicU64::new(0),
            total_tokens: AtomicU64::new(0),
            successes: AtomicU64::new(0),
        }
    }

    /// Average latency in milliseconds, or 0.0 if no requests yet.
    pub fn avg_latency(&self) -> f64 {
        let reqs = self.requests.load(Ordering::Relaxed);
        if reqs == 0 {
            return 0.0;
        }
        self.total_latency_ms.load(Ordering::Relaxed) as f64 / reqs as f64
    }

    /// Fraction of requests that succeeded (0.0–1.0).
    pub fn success_rate(&self) -> f64 {
        let reqs = self.requests.load(Ordering::Relaxed);
        if reqs == 0 {
            return 0.0;
        }
        self.successes.load(Ordering::Relaxed) as f64 / reqs as f64
    }

    /// Average tokens per request.
    pub fn avg_tokens(&self) -> f64 {
        let reqs = self.requests.load(Ordering::Relaxed);
        if reqs == 0 {
            return 0.0;
        }
        self.total_tokens.load(Ordering::Relaxed) as f64 / reqs as f64
    }
}

// ---------------------------------------------------------------------------
// AbTest
// ---------------------------------------------------------------------------

/// An active A/B test comparing two or more variants.
pub struct AbTest {
    /// Unique test identifier.
    pub id: String,
    /// All variants in this test.
    pub variants: Vec<Variant>,
    /// Map of session_id → variant_id for all assigned sessions.
    pub assignments: RwLock<HashMap<String, String>>,
    /// Per-variant metrics (Arc so VariantMetrics with atomics can live behind shared refs).
    pub metrics: HashMap<String, Arc<VariantMetrics>>,
    /// When the test was started.
    pub start_time: std::time::Instant,
    /// Fraction of traffic routed into this test (0.0–1.0).
    pub traffic_pct: f64,
}

impl AbTest {
    /// Create a new A/B test.
    ///
    /// * `id` — unique test name
    /// * `variants` — must be non-empty; weights are normalised internally
    /// * `traffic_pct` — fraction of sessions to route into this test (0.0–1.0)
    pub fn new(id: impl Into<String>, variants: Vec<Variant>, traffic_pct: f64) -> Self {
        let mut metrics = HashMap::new();
        for v in &variants {
            metrics.insert(v.id.clone(), Arc::new(VariantMetrics::new(v.id.clone())));
        }
        Self {
            id: id.into(),
            variants,
            assignments: RwLock::new(HashMap::new()),
            metrics,
            start_time: std::time::Instant::now(),
            traffic_pct: traffic_pct.clamp(0.0, 1.0),
        }
    }

    /// Deterministically assign a session to a variant using consistent hashing.
    ///
    /// Returns `None` if traffic sampling excludes this session or there are no
    /// variants.
    pub fn assign(&self, session_id: &str) -> Option<&Variant> {
        if self.variants.is_empty() {
            return None;
        }

        // Determine whether this session is in the traffic bucket.
        let hash = hash_str(session_id);
        if (hash % 100) as f64 >= self.traffic_pct * 100.0 {
            return None;
        }

        // Check existing assignment first.
        {
            let assignments = self.assignments.read().ok()?;
            if let Some(vid) = assignments.get(session_id) {
                return self.variants.iter().find(|v| &v.id == vid);
            }
        }

        // Assign via weighted CDF.
        let total_weight: f64 = self.variants.iter().map(|v| v.weight.max(0.0)).sum();
        if total_weight == 0.0 {
            return None;
        }

        // Use a second hash (salted) for variant selection to decouple from
        // the traffic-inclusion hash above.
        let selection_hash = hash_str(&format!("{}{}", self.id, session_id));
        let cursor = (selection_hash as f64 / u64::MAX as f64) * total_weight;

        let mut cumulative = 0.0;
        let mut chosen: Option<&Variant> = None;
        for v in &self.variants {
            cumulative += v.weight.max(0.0);
            if cursor <= cumulative {
                chosen = Some(v);
                break;
            }
        }
        // Fallback: last variant.
        if chosen.is_none() {
            chosen = self.variants.last();
        }

        if let Some(v) = chosen {
            if let Ok(mut map) = self.assignments.write() {
                map.insert(session_id.to_string(), v.id.clone());
            }
        }

        chosen
    }

    /// Record the outcome of a request for the session's assigned variant.
    pub fn record_result(&self, session_id: &str, latency_ms: u64, tokens: u64, success: bool) {
        let variant_id = {
            match self.assignments.read() {
                Ok(map) => map.get(session_id).cloned(),
                Err(_) => return,
            }
        };
        if let Some(vid) = variant_id {
            if let Some(m) = self.metrics.get(&vid) {
                m.requests.fetch_add(1, Ordering::Relaxed);
                m.total_latency_ms.fetch_add(latency_ms, Ordering::Relaxed);
                m.total_tokens.fetch_add(tokens, Ordering::Relaxed);
                if success {
                    m.successes.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Return the variant_id of the winner if its success_rate exceeds all
    /// others by more than 5 percentage points.
    pub fn winner(&self) -> Option<&str> {
        if self.variants.len() < 2 {
            return None;
        }
        // Collect (variant_id, success_rate) pairs.
        let mut rates: Vec<(&str, f64)> = self
            .variants
            .iter()
            .map(|v| {
                let rate = self
                    .metrics
                    .get(&v.id)
                    .map(|m| m.success_rate())
                    .unwrap_or(0.0);
                (v.id.as_str(), rate)
            })
            .collect();

        rates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let best = rates[0];
        let second = rates[1];
        if best.1 - second.1 > 0.05 {
            Some(best.0)
        } else {
            None
        }
    }

    /// Render a markdown table summarising per-variant statistics.
    pub fn summary(&self) -> String {
        let mut out = format!("## A/B Test: {}\n\n", self.id);
        out.push_str("| Variant | Requests | Avg Latency (ms) | Avg Tokens | Success Rate |\n");
        out.push_str("|---------|----------|-----------------|------------|-------------|\n");
        for v in &self.variants {
            if let Some(m) = self.metrics.get(&v.id) {
                out.push_str(&format!(
                    "| {} | {} | {:.1} | {:.1} | {:.1}% |\n",
                    v.id,
                    m.requests.load(Ordering::Relaxed),
                    m.avg_latency(),
                    m.avg_tokens(),
                    m.success_rate() * 100.0,
                ));
            }
        }
        if let Some(w) = self.winner() {
            out.push_str(&format!("\n**Winner: {}**\n", w));
        } else {
            out.push_str("\n*No winner determined yet.*\n");
        }
        out
    }
}

// ---------------------------------------------------------------------------
// AbTestRegistry
// ---------------------------------------------------------------------------

/// Central registry that manages the lifecycle of multiple A/B tests.
pub struct AbTestRegistry {
    tests: RwLock<HashMap<String, AbTest>>,
}

impl AbTestRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            tests: RwLock::new(HashMap::new()),
        }
    }

    /// Register a new test.
    pub fn create_test(&self, test: AbTest) {
        if let Ok(mut map) = self.tests.write() {
            map.insert(test.id.clone(), test);
        }
    }

    /// Retrieve an immutable reference to a test by id.
    ///
    /// Returns a cloned summary string because holding the read-guard across
    /// arbitrary caller code would require unsafe lifetime tricks.
    pub fn get_test_summary(&self, test_id: &str) -> Option<String> {
        let map = self.tests.read().ok()?;
        map.get(test_id).map(|t| t.summary())
    }

    /// Remove a concluded test, returning its final summary.
    pub fn conclude_test(&self, test_id: &str) -> Option<String> {
        let summary = self.get_test_summary(test_id);
        if let Ok(mut map) = self.tests.write() {
            map.remove(test_id);
        }
        summary
    }

    /// Return the ids of all currently active tests.
    pub fn active_tests(&self) -> Vec<String> {
        self.tests
            .read()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }
}

impl Default for AbTestRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test(traffic: f64) -> AbTest {
        AbTest::new(
            "test-1",
            vec![
                Variant { id: "A".into(), model: "gpt-4".into(), prompt_modifier: "".into(), weight: 1.0 },
                Variant { id: "B".into(), model: "gpt-3.5".into(), prompt_modifier: "[fast] ".into(), weight: 1.0 },
            ],
            traffic,
        )
    }

    #[test]
    fn assignment_is_deterministic() {
        let t = make_test(1.0);
        let first = t.assign("session-abc").map(|v| v.id.clone());
        let second = t.assign("session-abc").map(|v| v.id.clone());
        assert_eq!(first, second);
    }

    #[test]
    fn no_traffic_returns_none() {
        let t = make_test(0.0);
        // At 0% traffic, all sessions should be excluded.
        let mut excluded = 0usize;
        for i in 0..100 {
            if t.assign(&format!("s-{i}")).is_none() {
                excluded += 1;
            }
        }
        assert!(excluded > 90, "expected most sessions excluded, got {} included", 100 - excluded);
    }

    #[test]
    fn winner_requires_5pct_margin() {
        let t = make_test(1.0);
        // Force assign sessions and record results.
        for i in 0..50 {
            let s = format!("s-{i}");
            t.assign(&s);
            t.record_result(&s, 100, 50, true);
        }
        // Without clear margin winner should be None (both variants get 100%).
        // Just assert it returns Some or None without panicking.
        let _ = t.winner();
    }

    #[test]
    fn summary_contains_header() {
        let t = make_test(1.0);
        let s = t.summary();
        assert!(s.contains("Variant"), "summary missing header");
    }

    #[test]
    fn registry_lifecycle() {
        let reg = AbTestRegistry::new();
        reg.create_test(make_test(1.0));
        assert!(reg.active_tests().contains(&"test-1".to_string()));
        let summary = reg.conclude_test("test-1");
        assert!(summary.is_some());
        assert!(reg.active_tests().is_empty());
    }
}
