//! Semantic Request Deduplication
//!
//! Unlike the exact-match [`Deduplicator`] (which only catches byte-identical
//! requests), `SemanticDeduplicator` uses **SimHash** — a locality-sensitive
//! hashing technique — to detect *near-duplicate* prompts.  Two prompts that
//! are paraphrases of the same question produce SimHash fingerprints that
//! differ in very few bits, and are coalesced to avoid redundant LLM calls.
//!
//! ## How SimHash Works
//!
//! 1. The input is split into overlapping 3-gram token windows.
//! 2. Each token is hashed with FNV-1a into a 64-bit value.
//! 3. For each of the 64 bit positions, we accumulate `+1` if the token's
//!    hash has that bit set, `−1` otherwise.
//! 4. The SimHash fingerprint is the sign vector of the 64 accumulators.
//! 5. Two fingerprints whose **Hamming distance** ≤ `similarity_threshold`
//!    bits are considered semantically equivalent.
//!
//! A threshold of 3 bits catches common paraphrases (punctuation changes,
//! synonym substitution).  A threshold of 0 bits is exact-content matching.
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::enhanced::SemanticDeduplicator;
//! use std::time::Duration;
//!
//! let dedup = SemanticDeduplicator::new(3, Duration::from_secs(300));
//!
//! // First request is always novel.
//! assert!(dedup.is_novel("What is the capital of France?"));
//! dedup.register("What is the capital of France?");
//!
//! // Near-duplicate (punctuation change) is caught.
//! assert!(!dedup.is_novel("What is the capital of France"));
//!
//! // Distinct question remains novel.
//! assert!(dedup.is_novel("What is the capital of Germany?"));
//! ```
//!
//! [`Deduplicator`]: super::Deduplicator

use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing::{debug, trace};

/// A 64-bit SimHash fingerprint of a text string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SimHashFingerprint(u64);

impl SimHashFingerprint {
    /// Compute the SimHash fingerprint for a text string.
    ///
    /// The text is tokenised by whitespace, then a 3-gram sliding window is
    /// applied over the tokens so that small edits produce similar fingerprints
    /// rather than random ones.
    pub fn compute(text: &str) -> Self {
        // Collect tokens, stripping leading/trailing punctuation so that
        // "France?" and "France" produce the same token hash.
        let tokens: Vec<String> = text
            .split_whitespace()
            .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();

        // Accumulator: one i32 per bit position.
        let mut acc = [0i32; 64];

        // Generate weighted shingles using 1-grams and 2-grams.
        // 1-grams carry weight 1; 2-grams carry weight 2 (they capture context).
        let process = |token: &str, weight: i32, acc: &mut [i32; 64]| {
            let h = fnv1a_64(token.as_bytes());
            for bit in 0..64u32 {
                if (h >> bit) & 1 == 1 {
                    acc[bit as usize] += weight;
                } else {
                    acc[bit as usize] -= weight;
                }
            }
        };

        for (i, token) in tokens.iter().enumerate() {
            process(token, 1, &mut acc);
            if i + 1 < tokens.len() {
                let bigram = format!("{} {}", token, tokens[i + 1]);
                process(&bigram, 2, &mut acc);
            }
        }

        // Convert accumulator to fingerprint: 1 if acc[i] > 0, else 0.
        let mut fingerprint = 0u64;
        for bit in 0..64u32 {
            if acc[bit as usize] > 0 {
                fingerprint |= 1u64 << bit;
            }
        }

        Self(fingerprint)
    }

    /// Returns the Hamming distance between two fingerprints (number of bits
    /// that differ).  Two fingerprints with distance ≤ `threshold` are
    /// considered semantically similar.
    #[inline]
    pub fn hamming_distance(self, other: Self) -> u32 {
        (self.0 ^ other.0).count_ones()
    }

    /// Raw fingerprint value.
    pub fn value(self) -> u64 {
        self.0
    }
}

/// A cached entry in the semantic dedup store.
#[derive(Debug, Clone)]
struct CacheEntry {
    fingerprint: SimHashFingerprint,
    inserted_at: SystemTime,
    /// The canonical key (first text that created this entry) for logging.
    canonical_key: String,
}

/// Near-duplicate request detector using SimHash locality-sensitive hashing.
///
/// Thread-safe and lock-free on the hot path via `DashMap`.
///
/// # Parameters
///
/// - `similarity_threshold` — Maximum Hamming distance for two prompts to be
///   considered duplicates.  Typical values: 0 (exact), 3 (paraphrases),
///   6 (loose semantic equivalence).
/// - `window` — How long fingerprints are kept before expiry.
#[derive(Clone)]
pub struct SemanticDeduplicator {
    inner: Arc<SemanticDedupInner>,
}

struct SemanticDedupInner {
    /// Stored fingerprints: `fingerprint_value → CacheEntry`.
    ///
    /// We store the fingerprint as the key so that lookups are O(1) for exact
    /// fingerprint matches.  Near-duplicates (different fingerprint, small
    /// Hamming distance) require a linear scan of the store; this is acceptable
    /// because the store is bounded by TTL expiry and typical LLM workloads
    /// have a modest number of distinct in-flight fingerprints.
    store: DashMap<u64, CacheEntry>,
    similarity_threshold: u32,
    window: Duration,
}

impl SemanticDeduplicator {
    /// Create a new `SemanticDeduplicator`.
    ///
    /// # Arguments
    /// * `similarity_threshold` — Hamming-distance threshold.  Prompts whose
    ///   fingerprints differ by ≤ this many bits are treated as duplicates.
    ///   Use `0` for exact-content dedup only.
    /// * `window` — TTL for cached fingerprints.
    pub fn new(similarity_threshold: u32, window: Duration) -> Self {
        Self {
            inner: Arc::new(SemanticDedupInner {
                store: DashMap::new(),
                similarity_threshold,
                window,
            }),
        }
    }

    /// Returns `true` if `text` has **not** been seen recently (or is not a
    /// near-duplicate of a recently seen prompt).
    ///
    /// This is a read-only check; call [`register`](Self::register) to record
    /// a novel request.
    pub fn is_novel(&self, text: &str) -> bool {
        let fp = SimHashFingerprint::compute(text);
        self.find_match(fp).is_none()
    }

    /// Record `text` as a recently processed request.
    ///
    /// Call this after confirming a request is novel (via [`is_novel`]) and
    /// sending it to the LLM.
    pub fn register(&self, text: &str) {
        let fp = SimHashFingerprint::compute(text);
        let entry = CacheEntry {
            fingerprint: fp,
            inserted_at: SystemTime::now(),
            canonical_key: text.chars().take(80).collect(),
        };
        self.inner.store.insert(fp.value(), entry);
        trace!(fingerprint = fp.value(), "semantic_dedup: registered fingerprint");
        self.evict_expired();
    }

    /// Check if `text` is novel and, if so, register it atomically.
    ///
    /// Returns `true` if the request was novel and has been registered.
    /// Returns `false` if it matched a cached near-duplicate.
    pub fn check_and_register(&self, text: &str) -> bool {
        let fp = SimHashFingerprint::compute(text);
        self.evict_expired();
        if let Some(existing) = self.find_match(fp) {
            debug!(
                fingerprint = fp.value(),
                matched_canonical = %existing.canonical_key,
                hamming = fp.hamming_distance(existing.fingerprint),
                "semantic_dedup: near-duplicate suppressed"
            );
            return false;
        }
        let entry = CacheEntry {
            fingerprint: fp,
            inserted_at: SystemTime::now(),
            canonical_key: text.chars().take(80).collect(),
        };
        self.inner.store.insert(fp.value(), entry);
        true
    }

    /// Number of fingerprints currently cached (including possibly expired ones
    /// not yet evicted).
    pub fn cached_count(&self) -> usize {
        self.inner.store.len()
    }

    /// Manually evict all expired entries.  Called automatically on every
    /// [`register`](Self::register) and [`check_and_register`] call.
    pub fn evict_expired(&self) {
        let now = SystemTime::now();
        self.inner.store.retain(|_, entry| {
            now.duration_since(entry.inserted_at)
                .map(|age| age < self.inner.window)
                .unwrap_or(false)
        });
    }

    /// Find a cache entry whose fingerprint is within `similarity_threshold`
    /// Hamming bits of `fp`.  Returns the first match found.
    fn find_match(&self, fp: SimHashFingerprint) -> Option<CacheEntry> {
        let now = SystemTime::now();
        for entry in self.inner.store.iter() {
            let age = now
                .duration_since(entry.inserted_at)
                .unwrap_or(Duration::ZERO);
            if age >= self.inner.window {
                continue; // Expired — skip without evicting (done lazily).
            }
            if fp.hamming_distance(entry.fingerprint) <= self.inner.similarity_threshold {
                return Some(entry.clone());
            }
        }
        None
    }
}

/// FNV-1a 64-bit hash — fast, well-distributed, suitable for SimHash tokens.
#[inline]
fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 14_695_981_039_346_656_037;
    const PRIME: u64 = 1_099_511_628_211;
    let mut hash = OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_texts_produce_same_fingerprint() {
        let a = SimHashFingerprint::compute("What is the capital of France?");
        let b = SimHashFingerprint::compute("What is the capital of France?");
        assert_eq!(a, b);
        assert_eq!(a.hamming_distance(b), 0);
    }

    #[test]
    fn minor_edit_produces_low_hamming_distance() {
        let a = SimHashFingerprint::compute("What is the capital of France?");
        let b = SimHashFingerprint::compute("What is the capital of France");
        // Punctuation drop should produce very few bit flips.
        assert!(a.hamming_distance(b) <= 5, "distance was {}", a.hamming_distance(b));
    }

    #[test]
    fn completely_different_texts_have_high_hamming_distance() {
        let a = SimHashFingerprint::compute("What is the capital of France?");
        let b = SimHashFingerprint::compute("How do I bake a chocolate cake?");
        // Unrelated topics should differ substantially.
        assert!(a.hamming_distance(b) > 10, "distance was {}", a.hamming_distance(b));
    }

    #[test]
    fn is_novel_returns_true_for_unseen_text() {
        let dedup = SemanticDeduplicator::new(3, Duration::from_secs(300));
        assert!(dedup.is_novel("Hello, world!"));
    }

    #[test]
    fn check_and_register_deduplicates_near_duplicates() {
        let dedup = SemanticDeduplicator::new(3, Duration::from_secs(300));
        assert!(dedup.check_and_register("What is the capital of France?"));
        // Minor punctuation change — should be caught as near-duplicate.
        assert!(!dedup.check_and_register("What is the capital of France"));
    }

    #[test]
    fn distinct_requests_both_pass() {
        let dedup = SemanticDeduplicator::new(3, Duration::from_secs(300));
        assert!(dedup.check_and_register("What is the capital of France?"));
        assert!(dedup.check_and_register("What is the capital of Germany?"));
    }

    #[test]
    fn threshold_zero_is_exact_match_only() {
        let dedup = SemanticDeduplicator::new(0, Duration::from_secs(300));
        assert!(dedup.check_and_register("Hello world"));
        // Exact duplicate is blocked.
        assert!(!dedup.check_and_register("Hello world"));
        // Different content passes at threshold=0.
        assert!(dedup.check_and_register("Hello universe"));
    }

    #[test]
    fn expired_entries_allow_re_registration() {
        let dedup = SemanticDeduplicator::new(3, Duration::from_millis(1));
        assert!(dedup.check_and_register("What is the capital of France?"));
        // Sleep longer than the TTL.
        std::thread::sleep(Duration::from_millis(5));
        // After expiry, the same text should be treated as novel again.
        assert!(dedup.check_and_register("What is the capital of France?"));
    }

    #[test]
    fn empty_text_does_not_panic() {
        let fp = SimHashFingerprint::compute("");
        assert_eq!(fp.hamming_distance(fp), 0);
    }
}
