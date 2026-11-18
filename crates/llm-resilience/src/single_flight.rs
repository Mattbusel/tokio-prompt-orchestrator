//! A generic single-flight primitive: concurrent calls with the same key run
//! the work once and all get the result.
//!
//! [`Dedup<K, V>`] works for any key and any cloneable value, so it is just as
//! useful for database lookups, token refreshes, config fetches or image
//! renders as it is for LLM calls. It has no dependency on the rest of the
//! pipeline.
//!
//! ```
//! use std::sync::atomic::{AtomicUsize, Ordering};
//! use std::sync::Arc;
//! use std::time::Duration;
//! use llm_resilience::Dedup;
//!
//! # #[tokio::main]
//! # async fn main() {
//! let dedup: Dedup<String, u64> = Dedup::new();
//! let calls = Arc::new(AtomicUsize::new(0));
//!
//! let fetch = |id: &str| {
//!     let calls = calls.clone();
//!     dedup.run(id.to_string(), move || async move {
//!         calls.fetch_add(1, Ordering::SeqCst);
//!         tokio::time::sleep(Duration::from_millis(20)).await;
//!         42
//!     })
//! };
//!
//! // Three concurrent calls for the same key: one fetch.
//! let (a, b, c) = tokio::join!(fetch("user:7"), fetch("user:7"), fetch("user:7"));
//! assert_eq!((a, b, c), (42, 42, 42));
//! assert_eq!(calls.load(Ordering::SeqCst), 1);
//! # }
//! ```
//!
//! # Semantics
//!
//! - **One leader per key.** The first caller for a key runs its closure.
//!   Callers that arrive while it runs wait for its result instead of running
//!   their own. Once the leader finishes, the key is free again: the result is
//!   not cached (put a cache in front if you want one).
//! - **Cancellation is safe.** If the leader's future is dropped (a timeout, a
//!   client disconnect) or its closure panics, the waiters are woken and one
//!   of them becomes the new leader and runs its own closure. Nobody waits
//!   forever and no panic is propagated to the waiters.
//! - **Errors are values.** Use `V = Result<T, E>` (with `E: Clone`, or an
//!   `Arc<E>`) and the waiters get the leader's error too, which is usually
//!   what you want during an outage.
//! - **No `'static` or `Send` bound on the work.** The leader's future runs
//!   inline in the leader's task, so it may borrow from its caller.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::watch;

/// Coalesces concurrent work by key: one call runs, the rest share its result.
///
/// See the [module docs](crate::single_flight) for the full semantics.
/// `Dedup` is cheap to clone; clones share the same in-flight table and
/// statistics.
///
/// For prompt deduplication with a time-to-live cache and semantic matching,
/// see `tokio_prompt_orchestrator::enhanced::Deduplicator`. `Dedup` is the
/// small, general building block: any key, any value, no cache.
pub struct Dedup<K, V> {
    inner: Arc<Inner<K, V>>,
}

struct Inner<K, V> {
    in_flight: Mutex<HashMap<K, Entry<V>>>,
    next_id: AtomicU64,
    leaders: AtomicU64,
    coalesced: AtomicU64,
}

struct Entry<V> {
    id: u64,
    rx: watch::Receiver<Option<V>>,
}

impl<K, V> Clone for Dedup<K, V> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<K, V> std::fmt::Debug for Dedup<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dedup")
            .field("in_flight", &self.inner.in_flight.lock().len())
            .field("leaders", &self.inner.leaders.load(Ordering::Relaxed))
            .field("coalesced", &self.inner.coalesced.load(Ordering::Relaxed))
            .finish()
    }
}

impl<K, V> Default for Dedup<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

/// Counters for a [`Dedup`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DedupStats {
    /// Calls that ran their own closure (became the leader for their key).
    pub leaders: u64,
    /// Calls that received another call's result instead of running.
    pub coalesced: u64,
    /// Keys with work running right now.
    pub in_flight: usize,
}

/// The outcome of [`Dedup::claim`].
pub enum Claim<K, V>
where
    K: Hash + Eq + Clone,
{
    /// This caller is the leader and must do the work, then call
    /// [`Leader::complete`]. Dropping the [`Leader`] without completing it
    /// hands the key to one of the waiters.
    Leader(Leader<K, V>),
    /// Another caller did the work; this is its result.
    Joined(V),
}

impl<K, V> std::fmt::Debug for Claim<K, V>
where
    K: Hash + Eq + Clone,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Claim::Leader(_) => f.write_str("Claim::Leader"),
            Claim::Joined(_) => f.write_str("Claim::Joined"),
        }
    }
}

/// Proof that this caller owns a key in a [`Dedup`]; returned by
/// [`Dedup::claim`].
///
/// Call [`complete`](Self::complete) with the result to wake everyone
/// waiting on the key. If the `Leader` is dropped instead (an error you do
/// not want to share, a cancellation, a panic), the waiters wake up and one
/// of them becomes the new leader.
pub struct Leader<K, V>
where
    K: Hash + Eq + Clone,
{
    inner: Arc<Inner<K, V>>,
    key: K,
    id: u64,
    tx: Option<watch::Sender<Option<V>>>,
}

impl<K, V> std::fmt::Debug for Leader<K, V>
where
    K: Hash + Eq + Clone,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Leader").field("id", &self.id).finish()
    }
}

impl<K, V> Leader<K, V>
where
    K: Hash + Eq + Clone,
{
    /// The key this leader owns.
    pub fn key(&self) -> &K {
        &self.key
    }

    /// Publish `value` to every caller waiting on this key and release the
    /// key.
    pub fn complete(self, value: V) {
        if let Some(tx) = &self.tx {
            // Fails only when nobody is waiting, which is fine.
            let _ = tx.send(Some(value));
        }
        // Drop releases the key (removes the entry, then the sender).
    }
}

impl<K, V> Drop for Leader<K, V>
where
    K: Hash + Eq + Clone,
{
    fn drop(&mut self) {
        // Remove our entry first, so waiters woken by the sender closing do
        // not find a stale entry for a leader that is gone.
        let removed = {
            let mut map = self.inner.in_flight.lock();
            match map.get(&self.key) {
                Some(entry) if entry.id == self.id => map.remove(&self.key),
                _ => None,
            }
        };
        drop(removed);
        drop(self.tx.take());
    }
}

impl<K, V> Dedup<K, V> {
    /// An empty `Dedup`.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                in_flight: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(0),
                leaders: AtomicU64::new(0),
                coalesced: AtomicU64::new(0),
            }),
        }
    }

    /// Number of keys with work running right now.
    pub fn in_flight(&self) -> usize {
        self.inner.in_flight.lock().len()
    }

    /// Counters since this `Dedup` was created.
    pub fn stats(&self) -> DedupStats {
        DedupStats {
            leaders: self.inner.leaders.load(Ordering::Relaxed),
            coalesced: self.inner.coalesced.load(Ordering::Relaxed),
            in_flight: self.in_flight(),
        }
    }
}

impl<K, V> Dedup<K, V>
where
    K: Hash + Eq + Clone,
    V: Clone,
{
    /// Run `work` for `key`, unless a call for the same key is already
    /// running, in which case wait for that call and return its result.
    ///
    /// `work` is called at most once, and only if this caller becomes the
    /// leader. If the leader this call was waiting on is cancelled or panics,
    /// this call may become the leader itself and run `work` after all.
    ///
    /// ```
    /// # use llm_resilience::Dedup;
    /// # #[tokio::main]
    /// # async fn main() {
    /// let dedup: Dedup<&'static str, Result<String, String>> = Dedup::new();
    /// let v = dedup.run("config", || async { Ok("loaded".to_string()) }).await;
    /// assert_eq!(v.as_deref(), Ok("loaded"));
    /// # }
    /// ```
    pub async fn run<F, Fut>(&self, key: K, work: F) -> V
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V>,
    {
        match self.claim(key).await {
            Claim::Joined(value) => value,
            Claim::Leader(leader) => {
                let value = work().await;
                leader.complete(value.clone());
                value
            }
        }
    }

    /// Like [`run`](Self::run), but also says whether this call shared
    /// another call's result (`true`) or ran `work` itself (`false`).
    pub async fn run_with_status<F, Fut>(&self, key: K, work: F) -> (V, bool)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V>,
    {
        match self.claim(key).await {
            Claim::Joined(value) => (value, true),
            Claim::Leader(leader) => {
                let value = work().await;
                leader.complete(value.clone());
                (value, false)
            }
        }
    }

    /// The low-level form of [`run`](Self::run): either become the leader
    /// for `key` or wait for the current leader's result.
    ///
    /// Use it when the leader needs to share something different from what
    /// it returns itself, for example sharing a buffered copy of a response
    /// while returning the original, or deciding after the fact not to share
    /// an error (drop the [`Leader`] instead of completing it).
    ///
    /// ```
    /// use llm_resilience::{single_flight::Claim, Dedup};
    /// # #[tokio::main]
    /// # async fn main() {
    /// let dedup: Dedup<u32, String> = Dedup::new();
    /// match dedup.claim(1).await {
    ///     Claim::Leader(leader) => leader.complete("computed".into()),
    ///     Claim::Joined(value) => println!("shared: {value}"),
    /// }
    /// # }
    /// ```
    pub async fn claim(&self, key: K) -> Claim<K, V> {
        loop {
            let rx = {
                let mut map = self.inner.in_flight.lock();
                match map.get(&key) {
                    // A live leader: wait for it. `has_changed` errors when
                    // the leader's sender is gone, i.e. a stale entry.
                    Some(entry) if entry.rx.has_changed().is_ok() => entry.rx.clone(),
                    _ => {
                        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
                        let (tx, rx) = watch::channel(None);
                        map.insert(key.clone(), Entry { id, rx });
                        self.inner.leaders.fetch_add(1, Ordering::Relaxed);
                        return Claim::Leader(Leader {
                            inner: Arc::clone(&self.inner),
                            key,
                            id,
                            tx: Some(tx),
                        });
                    }
                }
            };
            if let Some(value) = wait_for(rx).await {
                self.inner.coalesced.fetch_add(1, Ordering::Relaxed);
                return Claim::Joined(value);
            }
            // The leader went away without a result: try to take over.
        }
    }

    /// Whether work for `key` is running right now.
    pub fn is_in_flight(&self, key: &K) -> bool {
        self.inner.in_flight.lock().contains_key(key)
    }
}

/// Wait for a leader's value. `None` when the leader went away without one.
async fn wait_for<V: Clone>(mut rx: watch::Receiver<Option<V>>) -> Option<V> {
    loop {
        if let Some(value) = rx.borrow_and_update().clone() {
            return Some(value);
        }
        if rx.changed().await.is_err() {
            // Sender dropped. It may have sent just before going away.
            return rx.borrow().clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    #[tokio::test]
    async fn concurrent_calls_run_once() {
        let dedup: Dedup<&str, usize> = Dedup::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let dedup = dedup.clone();
            let calls = calls.clone();
            handles.push(tokio::spawn(async move {
                dedup
                    .run("k", || async move {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        calls.fetch_add(1, Ordering::SeqCst) + 7
                    })
                    .await
            }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap(), 7);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let stats = dedup.stats();
        assert_eq!(stats.leaders, 1);
        assert_eq!(stats.coalesced, 15);
        assert_eq!(stats.in_flight, 0);
    }

    #[tokio::test]
    async fn different_keys_run_independently() {
        let dedup: Dedup<u32, u32> = Dedup::new();
        let (a, b) = tokio::join!(dedup.run(1, || async { 10 }), dedup.run(2, || async { 20 }));
        assert_eq!((a, b), (10, 20));
        assert_eq!(dedup.stats().leaders, 2);
    }

    #[tokio::test]
    async fn sequential_calls_are_not_cached() {
        let dedup: Dedup<u32, u32> = Dedup::new();
        assert_eq!(dedup.run(1, || async { 1 }).await, 1);
        assert_eq!(dedup.run(1, || async { 2 }).await, 2);
        assert_eq!(dedup.in_flight(), 0);
    }

    #[tokio::test]
    async fn errors_are_shared() {
        let dedup: Dedup<u8, Result<u8, String>> = Dedup::new();
        let slow_err = dedup.run(0, || async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            Err::<u8, _>("boom".to_string())
        });
        let joiner = async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            dedup
                .run_with_status(0, || async { Ok::<_, String>(1) })
                .await
        };
        let (a, (b, shared)) = tokio::join!(slow_err, joiner);
        assert_eq!(a, Err("boom".into()));
        assert_eq!(b, Err("boom".into()));
        assert!(shared);
    }

    #[tokio::test]
    async fn cancelled_leader_hands_over_to_a_waiter() {
        let dedup: Dedup<u8, u8> = Dedup::new();
        let d2 = dedup.clone();
        let leader = tokio::spawn(async move {
            d2.run(0, || async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                1
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(dedup.is_in_flight(&0));
        let d3 = dedup.clone();
        let waiter = tokio::spawn(async move { d3.run_with_status(0, || async { 2 }).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        leader.abort();
        let (v, shared) = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("waiter must not hang")
            .unwrap();
        assert_eq!(v, 2);
        assert!(!shared, "the waiter ran its own work");
        assert_eq!(dedup.in_flight(), 0);
    }

    #[tokio::test]
    async fn panicking_leader_does_not_poison_waiters() {
        let dedup: Dedup<u8, u8> = Dedup::new();
        let d2 = dedup.clone();
        let leader = tokio::spawn(async move {
            d2.run(0, || async {
                tokio::time::sleep(Duration::from_millis(30)).await;
                panic!("leader blew up");
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        let v = dedup.run(0, || async { 9 }).await;
        assert_eq!(v, 9);
        assert!(leader.await.is_err());
        assert_eq!(dedup.in_flight(), 0);
    }

    #[tokio::test]
    async fn dropped_leader_claim_releases_the_key() {
        let dedup: Dedup<u8, u8> = Dedup::new();
        match dedup.claim(3).await {
            Claim::Leader(l) => {
                assert_eq!(*l.key(), 3);
                assert!(dedup.is_in_flight(&3));
                drop(l);
            }
            Claim::Joined(_) => panic!("first claim must lead"),
        }
        assert!(!dedup.is_in_flight(&3));
        assert!(matches!(dedup.claim(3).await, Claim::Leader(_)));
    }

    #[tokio::test]
    async fn work_may_borrow_from_the_caller() {
        let dedup: Dedup<u8, usize> = Dedup::new();
        let local = String::from("borrowed");
        let n = dedup.run(1, || async { local.len() }).await;
        assert_eq!(n, 8);
    }
}
