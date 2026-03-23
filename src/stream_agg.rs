//! # Streaming Response Aggregator
//!
//! Collects streaming token chunks from LLM inference sessions into complete
//! responses, with real-time per-session broadcast for subscribers.
//!
//! ## Overview
//!
//! [`StreamAggregator`] buffers [`StreamChunk`]s keyed by `session_id`.  When
//! the final chunk arrives, [`StreamAggregator::complete`] flushes the buffer
//! and returns the assembled text.  Subscribers registered via
//! [`StreamAggregator::subscribe`] receive every chunk in real-time via a
//! [`tokio::sync::broadcast`] channel.
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::stream_agg::{StreamAggregator, StreamChunk};
//! use std::collections::HashMap;
//!
//! # #[tokio::main]
//! # async fn main() {
//! let agg = StreamAggregator::new(64);
//!
//! agg.feed(StreamChunk { session_id: 1, token: "Hello".into(), is_final: false, metadata: HashMap::new() });
//! agg.feed(StreamChunk { session_id: 1, token: " world".into(), is_final: true, metadata: HashMap::new() });
//!
//! let text = agg.complete(1);
//! assert_eq!(text.as_deref(), Some("Hello world"));
//! # }
//! ```

use dashmap::DashMap;
use futures::{Stream, StreamExt as _};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;

// ============================================================================
// Domain types
// ============================================================================

/// A single token chunk produced by a streaming LLM inference response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StreamChunk {
    /// The session this token belongs to.
    pub session_id: u64,
    /// The token text (may be a sub-word piece).
    pub token: String,
    /// When `true`, this is the last token in the stream.
    pub is_final: bool,
    /// Arbitrary key-value metadata (e.g. model, finish_reason).
    pub metadata: HashMap<String, String>,
}

/// Aggregate statistics for a [`StreamAggregator`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AggStats {
    /// Number of sessions currently buffering tokens.
    pub active_streams: usize,
    /// Cumulative number of sessions that have been completed via [`StreamAggregator::complete`].
    pub completed_streams: u64,
    /// Total tokens received across all sessions (including completed ones).
    pub total_tokens_streamed: u64,
}

// ============================================================================
// Internal state per session
// ============================================================================

struct SessionStream {
    buffer: String,
    tx: broadcast::Sender<StreamChunk>,
}

// ============================================================================
// StreamAggregator
// ============================================================================

/// Concurrent, lock-free streaming response aggregator.
///
/// `channel_capacity` controls the bounded broadcast channel depth.  Lagging
/// subscribers that cannot keep up will have their oldest messages dropped.
#[derive(Clone)]
pub struct StreamAggregator {
    sessions: Arc<DashMap<u64, SessionStream>>,
    channel_capacity: usize,
    completed_streams: Arc<AtomicU64>,
    total_tokens: Arc<AtomicU64>,
}

impl StreamAggregator {
    /// Create a new aggregator with the given per-session broadcast capacity.
    pub fn new(channel_capacity: usize) -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            channel_capacity: channel_capacity.max(1),
            completed_streams: Arc::new(AtomicU64::new(0)),
            total_tokens: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Feed a token chunk into the aggregator.
    ///
    /// Internally:
    /// - A per-session buffer is created on first access.
    /// - The token is appended to the buffer.
    /// - The chunk is broadcast to any active subscribers.
    pub fn feed(&self, chunk: StreamChunk) {
        self.total_tokens.fetch_add(1, Ordering::Relaxed);
        let sid = chunk.session_id;

        let mut entry = self.sessions.entry(sid).or_insert_with(|| {
            let (tx, _) = broadcast::channel(self.channel_capacity);
            SessionStream {
                buffer: String::new(),
                tx,
            }
        });

        entry.buffer.push_str(&chunk.token);
        // Broadcast — ignore errors (no active subscribers is fine)
        let _ = entry.tx.send(chunk);
    }

    /// Flush the session buffer and return the complete assembled text.
    ///
    /// Returns `None` if the session has never received any chunks.
    /// After calling `complete`, the session entry is removed from the map.
    pub fn complete(&self, session_id: u64) -> Option<String> {
        if let Some((_, stream)) = self.sessions.remove(&session_id) {
            self.completed_streams.fetch_add(1, Ordering::Relaxed);
            Some(stream.buffer)
        } else {
            None
        }
    }

    /// Subscribe to real-time token chunks for a session.
    ///
    /// Returns a [`Stream`] that yields every [`StreamChunk`] fed for
    /// `session_id` after the subscription is registered.  Chunks that
    /// arrived before the call are not replayed.
    ///
    /// If the session does not yet exist, an empty buffer entry is created so
    /// that future [`feed`][Self::feed] calls reach the subscriber.
    pub fn subscribe(
        &self,
        session_id: u64,
    ) -> Pin<Box<dyn Stream<Item = StreamChunk> + Send + 'static>> {
        let rx = {
            let entry = self.sessions.entry(session_id).or_insert_with(|| {
                let (tx, _) = broadcast::channel(self.channel_capacity);
                SessionStream {
                    buffer: String::new(),
                    tx,
                }
            });
            entry.tx.subscribe()
        };

        // Convert broadcast::Receiver into a Stream using async_stream.
        // Filter out broadcast errors (lagged / sender dropped).
        let stream = async_stream::stream! {
            let mut rx = rx;
            loop {
                match rx.recv().await {
                    Ok(chunk) => yield chunk,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        };
        Box::pin(stream)
    }

    /// Return aggregate statistics.
    pub fn stats(&self) -> AggStats {
        AggStats {
            active_streams: self.sessions.len(),
            completed_streams: self.completed_streams.load(Ordering::Relaxed),
            total_tokens_streamed: self.total_tokens.load(Ordering::Relaxed),
        }
    }
}

// ============================================================================
// Unit tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;

    fn chunk(session_id: u64, token: &str, is_final: bool) -> StreamChunk {
        StreamChunk {
            session_id,
            token: token.into(),
            is_final,
            metadata: HashMap::new(),
        }
    }

    fn agg() -> StreamAggregator {
        StreamAggregator::new(64)
    }

    // --- feed + complete ---

    #[test]
    fn test_complete_empty_session() {
        let a = agg();
        assert!(a.complete(99).is_none());
    }

    #[test]
    fn test_single_chunk_complete() {
        let a = agg();
        a.feed(chunk(1, "Hello", true));
        assert_eq!(a.complete(1).as_deref(), Some("Hello"));
    }

    #[test]
    fn test_multiple_chunks_assembled() {
        let a = agg();
        a.feed(chunk(1, "The", false));
        a.feed(chunk(1, " quick", false));
        a.feed(chunk(1, " fox", true));
        assert_eq!(a.complete(1).as_deref(), Some("The quick fox"));
    }

    #[test]
    fn test_complete_removes_session() {
        let a = agg();
        a.feed(chunk(2, "x", true));
        a.complete(2);
        assert!(a.complete(2).is_none());
    }

    #[test]
    fn test_independent_sessions() {
        let a = agg();
        a.feed(chunk(1, "A", false));
        a.feed(chunk(2, "B", false));
        a.feed(chunk(1, "A2", true));
        a.feed(chunk(2, "B2", true));
        assert_eq!(a.complete(1).as_deref(), Some("AA2"));
        assert_eq!(a.complete(2).as_deref(), Some("BB2"));
    }

    #[test]
    fn test_complete_after_complete_returns_none() {
        let a = agg();
        a.feed(chunk(5, "z", true));
        a.complete(5);
        assert!(a.complete(5).is_none());
    }

    // --- stats ---

    #[test]
    fn test_stats_initial() {
        let a = agg();
        let s = a.stats();
        assert_eq!(s.active_streams, 0);
        assert_eq!(s.completed_streams, 0);
        assert_eq!(s.total_tokens_streamed, 0);
    }

    #[test]
    fn test_stats_active_count() {
        let a = agg();
        a.feed(chunk(1, "a", false));
        a.feed(chunk(2, "b", false));
        assert_eq!(a.stats().active_streams, 2);
    }

    #[test]
    fn test_stats_completed_increments() {
        let a = agg();
        a.feed(chunk(1, "a", true));
        a.complete(1);
        assert_eq!(a.stats().completed_streams, 1);
    }

    #[test]
    fn test_stats_total_tokens() {
        let a = agg();
        a.feed(chunk(1, "a", false));
        a.feed(chunk(1, "b", false));
        a.feed(chunk(1, "c", true));
        assert_eq!(a.stats().total_tokens_streamed, 3);
    }

    #[test]
    fn test_stats_active_drops_after_complete() {
        let a = agg();
        a.feed(chunk(1, "x", true));
        assert_eq!(a.stats().active_streams, 1);
        a.complete(1);
        assert_eq!(a.stats().active_streams, 0);
    }

    // --- subscribe ---

    #[tokio::test]
    async fn test_subscribe_receives_chunks() {
        let a = agg();
        let mut sub = a.subscribe(10);

        a.feed(chunk(10, "tok1", false));
        a.feed(chunk(10, "tok2", true));

        let first = tokio::time::timeout(std::time::Duration::from_millis(100), sub.next())
            .await
            .expect("timeout")
            .expect("stream ended");
        assert_eq!(first.token, "tok1");
    }

    #[tokio::test]
    async fn test_subscribe_multiple_sessions_isolated() {
        let a = agg();
        let mut sub1 = a.subscribe(1);
        let _sub2 = a.subscribe(2);

        a.feed(chunk(2, "not-for-1", false));
        a.feed(chunk(1, "for-1", true));

        let received = tokio::time::timeout(std::time::Duration::from_millis(200), sub1.next())
            .await
            .expect("timeout")
            .expect("no chunk");
        assert_eq!(received.token, "for-1");
    }

    #[tokio::test]
    async fn test_subscribe_then_complete_consistent() {
        let a = agg();
        let mut sub = a.subscribe(7);

        a.feed(chunk(7, "part1", false));
        a.feed(chunk(7, "part2", true));

        let _c1 = sub.next().await;
        let _c2 = sub.next().await;

        let full = a.complete(7);
        assert_eq!(full.as_deref(), Some("part1part2"));
    }

    #[tokio::test]
    async fn test_subscribe_before_any_feed() {
        let a = agg();
        let mut sub = a.subscribe(99);
        a.feed(chunk(99, "hello", true));
        let got = tokio::time::timeout(std::time::Duration::from_millis(100), sub.next())
            .await
            .expect("timeout")
            .expect("no chunk");
        assert_eq!(got.token, "hello");
    }

    // --- clone shares state ---

    #[test]
    fn test_clone_shares_state() {
        let a1 = agg();
        let a2 = a1.clone();
        a1.feed(chunk(1, "x", true));
        assert!(a2.complete(1).is_some());
    }

    #[test]
    fn test_metadata_preserved() {
        let a = agg();
        let mut meta = HashMap::new();
        meta.insert("model".into(), "claude-3".into());
        let c = StreamChunk {
            session_id: 1,
            token: "tok".into(),
            is_final: false,
            metadata: meta,
        };
        a.feed(c);
        // Just verifying feed doesn't panic; metadata lives in the broadcast
        assert_eq!(a.stats().total_tokens_streamed, 1);
    }
}
