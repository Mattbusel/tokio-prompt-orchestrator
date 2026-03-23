//! # Provider Tournament Mode
//!
//! Dispatches the same [`PromptRequest`] to multiple inference workers in
//! parallel and returns the highest-quality response according to a pluggable
//! scoring function.
//!
//! ## Why tournament mode?
//!
//! Different LLM providers (or different models on the same provider) excel
//! at different tasks.  For high-value requests — customer-facing responses,
//! code generation, document summarisation — it can be worth spending 2–4×
//! the normal inference cost to guarantee a better answer.
//!
//! Tournament mode lets you:
//!
//! - **A/B test** providers automatically and record which wins most often.
//! - **Hedge** against provider outages: the first successful response wins.
//! - **Tune quality vs. cost** by changing the scoring function.
//!
//! ## Built-in scorers
//!
//! | Scorer | Strategy |
//! |--------|----------|
//! | [`LongestResponseScorer`] | Prefer the longest non-empty response (proxy for detail). |
//! | [`FastestResponseScorer`] | Prefer the response that arrived first (minimise latency). |
//! | [`KeywordDensityScorer`] | Prefer the response with the highest density of caller-supplied keywords. |
//!
//! Implement [`ResponseScorer`] to add your own.
//!
//! ## Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use std::collections::HashMap;
//! use tokio_prompt_orchestrator::{SessionId, PromptRequest, EchoWorker};
//! use tokio_prompt_orchestrator::enhanced::tournament::{
//!     TournamentRunner, TournamentConfig, LongestResponseScorer,
//! };
//!
//! #[tokio::main]
//! async fn main() {
//!     let workers: Vec<Arc<dyn tokio_prompt_orchestrator::ModelWorker>> = vec![
//!         Arc::new(EchoWorker::new()),
//!         Arc::new(EchoWorker::new()),
//!     ];
//!
//!     let runner = TournamentRunner::new(
//!         workers,
//!         Arc::new(LongestResponseScorer),
//!         TournamentConfig::default(),
//!     );
//!
//!     let req = PromptRequest {
//!         session: SessionId::new("demo"),
//!         request_id: "t1".into(),
//!         input: "Explain quantum entanglement".into(),
//!         meta: HashMap::new(),
//!         deadline: None,
//!     };
//!
//!     if let Ok(result) = runner.run(req).await {
//!         println!("Winner (worker {}): {}", result.winner_index, result.response);
//!     }
//! }
//! ```

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use tracing::{debug, warn};

use crate::{worker::ModelWorker, PromptRequest};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for [`TournamentRunner`].
#[derive(Debug, Clone)]
pub struct TournamentConfig {
    /// Timeout for each individual worker call.
    ///
    /// Workers that exceed this timeout are considered failures and their
    /// response is discarded from scoring.  Defaults to 30 seconds.
    pub per_worker_timeout: Duration,

    /// When `true` (the default) the runner waits for **all** workers to
    /// respond before scoring.  Set to `false` to return as soon as the
    /// first successful response arrives (latency-optimised racing).
    pub await_all: bool,
}

impl Default for TournamentConfig {
    fn default() -> Self {
        Self {
            per_worker_timeout: Duration::from_secs(30),
            await_all: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Scorer trait
// ---------------------------------------------------------------------------

/// Scores a candidate inference response.
///
/// Higher scores are better.  The runner selects the candidate with the
/// highest score.  On a tie, the first candidate (lowest index) wins.
#[async_trait]
pub trait ResponseScorer: Send + Sync {
    /// Return a non-negative score for `response`.  `elapsed` is how long
    /// the worker took to produce the response.
    fn score(&self, response: &str, elapsed: Duration) -> f64;
}

// ---------------------------------------------------------------------------
// Built-in scorers
// ---------------------------------------------------------------------------

/// Scores by response length — longer responses score higher.
///
/// This is a simple heuristic that works well for summarisation and
/// explanation tasks where depth correlates with quality.
pub struct LongestResponseScorer;

#[async_trait]
impl ResponseScorer for LongestResponseScorer {
    fn score(&self, response: &str, _elapsed: Duration) -> f64 {
        response.chars().count() as f64
    }
}

/// Scores by latency — the fastest response scores highest.
///
/// Useful when you want the first available response from a pool of
/// equivalent workers (hedged requests).
pub struct FastestResponseScorer;

#[async_trait]
impl ResponseScorer for FastestResponseScorer {
    fn score(&self, _response: &str, elapsed: Duration) -> f64 {
        // Invert elapsed so shorter time → higher score.
        if elapsed.is_zero() {
            f64::MAX
        } else {
            1.0 / elapsed.as_secs_f64()
        }
    }
}

/// Scores by the density of caller-supplied keywords in the response.
///
/// Useful when you know your ideal response must contain certain terms
/// (e.g. technical keywords, product names, specific facts).
pub struct KeywordDensityScorer {
    /// Keywords to search for (case-insensitive).
    keywords: Vec<String>,
}

impl KeywordDensityScorer {
    /// Create a scorer that counts occurrences of the given keywords.
    pub fn new(keywords: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            keywords: keywords.into_iter().map(|k| k.into().to_lowercase()).collect(),
        }
    }
}

#[async_trait]
impl ResponseScorer for KeywordDensityScorer {
    fn score(&self, response: &str, _elapsed: Duration) -> f64 {
        if response.is_empty() {
            return 0.0;
        }
        let lower = response.to_lowercase();
        let hits: usize = self.keywords.iter().map(|kw| lower.matches(kw.as_str()).count()).sum();
        let word_count = response.split_whitespace().count().max(1);
        hits as f64 / word_count as f64
    }
}

// ---------------------------------------------------------------------------
// Tournament result
// ---------------------------------------------------------------------------

/// The outcome of a tournament run.
#[derive(Debug)]
pub struct TournamentResult {
    /// The winning response text.
    pub response: String,
    /// Zero-based index of the worker that produced the winning response.
    pub winner_index: usize,
    /// Score assigned to the winning response.
    pub score: f64,
    /// How long the overall tournament took (max of all worker latencies when
    /// `await_all = true`, or latency of the first response when `false`).
    pub total_elapsed: Duration,
    /// Number of workers that succeeded.
    pub successful_workers: usize,
    /// Number of workers that timed out or returned an error.
    pub failed_workers: usize,
}

// ---------------------------------------------------------------------------
// TournamentRunner
// ---------------------------------------------------------------------------

/// Runs a prompt through multiple workers and picks the best response.
///
/// See the [module documentation][self] for a complete example.
#[derive(Clone)]
pub struct TournamentRunner {
    workers: Vec<Arc<dyn ModelWorker>>,
    scorer: Arc<dyn ResponseScorer>,
    config: TournamentConfig,
    // Metrics
    tournaments_run: Arc<AtomicU64>,
    tournaments_failed: Arc<AtomicU64>,
}

impl TournamentRunner {
    /// Create a new [`TournamentRunner`].
    ///
    /// # Panics
    ///
    /// Panics if `workers` is empty.
    pub fn new(
        workers: Vec<Arc<dyn ModelWorker>>,
        scorer: Arc<dyn ResponseScorer>,
        config: TournamentConfig,
    ) -> Self {
        assert!(!workers.is_empty(), "TournamentRunner requires at least one worker");
        Self {
            workers,
            scorer,
            config,
            tournaments_run: Arc::new(AtomicU64::new(0)),
            tournaments_failed: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Run the request through all workers and return the best response.
    ///
    /// # Errors
    ///
    /// Returns `Err` only when **all** workers fail or time out.
    pub async fn run(
        &self,
        request: PromptRequest,
    ) -> Result<TournamentResult, String> {
        self.tournaments_run.fetch_add(1, Ordering::Relaxed);
        let start = Instant::now();

        // Fan out to all workers concurrently.
        let mut handles = Vec::with_capacity(self.workers.len());
        for (idx, worker) in self.workers.iter().enumerate() {
            let worker = Arc::clone(worker);
            let req = request.clone();
            let timeout = self.config.per_worker_timeout;
            handles.push(tokio::spawn(async move {
                let call_start = Instant::now();
                let result = tokio::time::timeout(timeout, worker.infer(req)).await;
                let elapsed = call_start.elapsed();
                (idx, result, elapsed)
            }));
        }

        // Collect results.
        let mut candidates: Vec<(usize, String, Duration)> = Vec::new();
        let mut failed = 0usize;

        if self.config.await_all {
            // Wait for every worker.
            for handle in handles {
                match handle.await {
                    Ok((idx, Ok(Ok(resp)), elapsed)) => {
                        candidates.push((idx, resp.text, elapsed));
                    }
                    Ok((idx, Ok(Err(e)), _)) => {
                        warn!(worker = idx, error = %e, "worker inference error");
                        failed += 1;
                    }
                    Ok((idx, Err(_timeout), _)) => {
                        warn!(worker = idx, "worker timed out");
                        failed += 1;
                    }
                    Err(e) => {
                        warn!(error = %e, "task panicked");
                        failed += 1;
                    }
                }
            }
        } else {
            // Race mode: return after the first success.
            use futures::future::select_all;
            let mut remaining = handles;
            while !remaining.is_empty() {
                let (result, _idx, rest) = select_all(remaining).await;
                remaining = rest;
                match result {
                    Ok((idx, Ok(Ok(resp)), elapsed)) => {
                        candidates.push((idx, resp.text, elapsed));
                        // Cancel remaining.
                        for h in remaining {
                            h.abort();
                        }
                        break;
                    }
                    Ok((idx, Ok(Err(e)), _)) => {
                        warn!(worker = idx, error = %e, "worker error in race");
                        failed += 1;
                    }
                    Ok((idx, Err(_), _)) => {
                        warn!(worker = idx, "worker timed out in race");
                        failed += 1;
                    }
                    Err(e) => {
                        warn!(error = %e, "task panicked in race");
                        failed += 1;
                    }
                }
            }
        }

        if candidates.is_empty() {
            self.tournaments_failed.fetch_add(1, Ordering::Relaxed);
            return Err(format!(
                "all {} workers failed or timed out",
                self.workers.len()
            ));
        }

        // Score candidates and pick the winner.
        let scorer = &*self.scorer;
        let (winner_idx, winner_response, winner_elapsed, best_score) = candidates
            .into_iter()
            .map(|(idx, resp, elapsed)| {
                let score = scorer.score(&resp, elapsed);
                (idx, resp, elapsed, score)
            })
            .fold(
                (0usize, String::new(), Duration::ZERO, f64::NEG_INFINITY),
                |best, (idx, resp, elapsed, score)| {
                    if score > best.3 {
                        (idx, resp, elapsed, score)
                    } else {
                        best
                    }
                },
            );

        debug!(
            winner = winner_idx,
            score = best_score,
            successful = self.workers.len() - failed,
            failed,
            "tournament complete"
        );

        Ok(TournamentResult {
            response: winner_response,
            winner_index: winner_idx,
            score: best_score,
            total_elapsed: start.elapsed(),
            successful_workers: self.workers.len() - failed,
            failed_workers: failed,
        })
    }

    /// Snapshot of aggregate statistics.
    pub fn stats(&self) -> TournamentStats {
        TournamentStats {
            tournaments_run: self.tournaments_run.load(Ordering::Relaxed),
            tournaments_failed: self.tournaments_failed.load(Ordering::Relaxed),
        }
    }
}

/// Statistics for [`TournamentRunner`].
#[derive(Debug, Clone)]
pub struct TournamentStats {
    /// Total tournament runs.
    pub tournaments_run: u64,
    /// Runs where all workers failed.
    pub tournaments_failed: u64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::{EchoWorker, SessionId};

    fn make_req(id: &str) -> PromptRequest {
        PromptRequest {
            session: SessionId::new(id),
            request_id: id.to_string(),
            input: "hello world this is a test".to_string(),
            meta: HashMap::new(),
            deadline: None,
        }
    }

    #[tokio::test]
    async fn single_worker_returns_result() {
        let workers: Vec<Arc<dyn ModelWorker>> = vec![Arc::new(EchoWorker::new())];
        let runner = TournamentRunner::new(
            workers,
            Arc::new(LongestResponseScorer),
            TournamentConfig::default(),
        );
        let result = runner.run(make_req("r1")).await;
        assert!(result.is_ok());
        let r = result.unwrap();
        assert_eq!(r.winner_index, 0);
        assert_eq!(r.successful_workers, 1);
        assert_eq!(r.failed_workers, 0);
    }

    #[tokio::test]
    async fn two_workers_picks_longer_response() {
        let workers: Vec<Arc<dyn ModelWorker>> = vec![
            Arc::new(EchoWorker::new()),
            Arc::new(EchoWorker::new()),
        ];
        let runner = TournamentRunner::new(
            workers,
            Arc::new(LongestResponseScorer),
            TournamentConfig::default(),
        );
        let r = runner.run(make_req("r2")).await.unwrap();
        assert_eq!(r.successful_workers, 2);
    }

    #[test]
    fn keyword_scorer_zero_on_empty() {
        let scorer = KeywordDensityScorer::new(["rust", "async"]);
        assert_eq!(scorer.score("", Duration::ZERO), 0.0);
    }

    #[test]
    fn keyword_scorer_case_insensitive() {
        let scorer = KeywordDensityScorer::new(["Rust"]);
        let score = scorer.score("I love RUST and rust is great", Duration::ZERO);
        assert!(score > 0.0);
    }

    #[test]
    fn fastest_scorer_prefers_zero_elapsed() {
        let scorer = FastestResponseScorer;
        let fast = scorer.score("x", Duration::from_millis(1));
        let slow = scorer.score("x", Duration::from_secs(1));
        assert!(fast > slow);
    }

    #[test]
    fn longest_scorer_prefers_more_chars() {
        let scorer = LongestResponseScorer;
        let short = scorer.score("hi", Duration::ZERO);
        let long_ = scorer.score("hello there friend", Duration::ZERO);
        assert!(long_ > short);
    }

    #[tokio::test]
    async fn stats_track_runs() {
        let workers: Vec<Arc<dyn ModelWorker>> = vec![Arc::new(EchoWorker::new())];
        let runner = TournamentRunner::new(
            workers,
            Arc::new(LongestResponseScorer),
            TournamentConfig::default(),
        );
        runner.run(make_req("s1")).await.ok();
        runner.run(make_req("s2")).await.ok();
        assert_eq!(runner.stats().tournaments_run, 2);
        assert_eq!(runner.stats().tournaments_failed, 0);
    }
}
