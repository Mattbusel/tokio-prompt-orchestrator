//! Cache Pre-Warming
//!
//! Implements a cache pre-warming system that generates and tracks warming jobs
//! based on common query patterns, ensuring the prompt cache is populated with
//! frequently-requested content before real traffic arrives.
//!
//! ## Key Types
//!
//! - [`WarmingStrategy`] — controls which patterns are expanded into jobs
//! - [`WarmingPattern`] — a parameterised template with variable substitutions
//! - [`CacheWarmer`] — orchestrates job generation and status tracking

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

// ── Enums ────────────────────────────────────────────────────────────────────

/// Strategy governing which patterns the warmer expands into jobs.
#[derive(Debug, Clone)]
pub enum WarmingStrategy {
    /// Warm the top-`k` patterns ranked by [`WarmingPattern::frequency_weight`].
    TopK { k: usize },
    /// Expand every registered pattern fully.
    AllTemplates,
    /// Expand only patterns whose `template` starts with `"scheduled:"`.
    ScheduledPatterns,
    /// Use caller-supplied literal queries (bypasses pattern expansion).
    UserDefined(Vec<String>),
}

/// Life-cycle status of a single warming job.
#[derive(Debug, Clone)]
pub enum WarmingStatus {
    /// Waiting to be executed.
    Pending,
    /// Currently being executed.
    Running,
    /// Successfully cached.
    Completed {
        /// Wall-clock time at which the response was stored in cache.
        cached_at: Instant,
    },
    /// Execution failed with the given reason.
    Failed(String),
    /// Skipped (e.g. already cached or strategy filter excluded it).
    Skipped,
}

// ── Structs ───────────────────────────────────────────────────────────────────

/// A single unit of warming work.
#[derive(Debug, Clone)]
pub struct WarmingJob {
    /// Unique identifier (monotonically increasing).
    pub id: u64,
    /// The fully-expanded query string to warm.
    pub query: String,
    /// Target model identifier (e.g. `"claude-3-5-sonnet"`).
    pub model: String,
    /// Scheduling priority — higher values are processed first.
    pub priority: u8,
    /// When this job was scheduled.
    pub scheduled_at: Instant,
    /// Current status.
    pub status: WarmingStatus,
}

/// A template with named variable slots and their possible values.
///
/// # Example
///
/// ```
/// use tokio_prompt_orchestrator::cache_warmer::WarmingPattern;
///
/// let mut p = WarmingPattern {
///     template: "explain {topic} in simple terms".to_string(),
///     variables: vec![
///         ("topic".to_string(), vec!["recursion".to_string(), "ownership".to_string()]),
///     ],
///     frequency_weight: 1.0,
/// };
/// let queries = p.expand();
/// assert_eq!(queries.len(), 2);
/// ```
#[derive(Debug, Clone)]
pub struct WarmingPattern {
    /// Template string where `{var_name}` is replaced by each possible value.
    pub template: String,
    /// Variable name → list of possible values for that variable.
    pub variables: Vec<(String, Vec<String>)>,
    /// Relative weight for `TopK` strategy ranking (higher = more important).
    pub frequency_weight: f64,
}

impl WarmingPattern {
    /// Expand this pattern into concrete query strings via cartesian product.
    ///
    /// At most 100 queries are returned to prevent combinatorial explosion.
    pub fn expand(&self) -> Vec<String> {
        // Start with the template as the only candidate.
        let mut results: Vec<String> = vec![self.template.clone()];

        for (var_name, values) in &self.variables {
            if values.is_empty() {
                continue;
            }
            let placeholder = format!("{{{}}}", var_name);
            let mut next: Vec<String> = Vec::with_capacity(results.len() * values.len());
            'outer: for base in &results {
                for val in values {
                    if next.len() >= 100 {
                        break 'outer;
                    }
                    next.push(base.replace(&placeholder, val));
                }
                if next.len() >= 100 {
                    break;
                }
            }
            results = next;
            if results.len() >= 100 {
                results.truncate(100);
                break;
            }
        }

        results
    }
}

/// Aggregate statistics for a [`CacheWarmer`] session.
#[derive(Debug, Clone, Default)]
pub struct WarmingStats {
    /// Total jobs ever generated.
    pub total_jobs: u64,
    /// Jobs that completed successfully.
    pub completed: u64,
    /// Jobs that failed.
    pub failed: u64,
    /// Cache entries added (equal to completed).
    pub cache_entries_added: u64,
    /// Approximate wall-clock ms spent on completed jobs (recorded externally).
    pub time_spent_ms: u64,
}

/// Orchestrates cache pre-warming across a set of [`WarmingPattern`]s.
pub struct CacheWarmer {
    /// Registered patterns.
    pub patterns: Vec<WarmingPattern>,
    /// Active strategy.
    pub strategy: WarmingStrategy,
    /// All jobs (pending, running, done).
    jobs: Mutex<Vec<WarmingJob>>,
    /// Monotonic job-id counter.
    next_id: AtomicU64,
    // Stats counters
    stat_total: AtomicU64,
    stat_completed: AtomicU64,
    stat_failed: AtomicU64,
    stat_time_ms: AtomicU64,
}

impl CacheWarmer {
    /// Create a new warmer with the given strategy and no patterns.
    pub fn new(strategy: WarmingStrategy) -> Self {
        Self {
            patterns: Vec::new(),
            strategy,
            jobs: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            stat_total: AtomicU64::new(0),
            stat_completed: AtomicU64::new(0),
            stat_failed: AtomicU64::new(0),
            stat_time_ms: AtomicU64::new(0),
        }
    }

    /// Register a new pattern.
    pub fn add_pattern(&mut self, pattern: WarmingPattern) {
        self.patterns.push(pattern);
    }

    /// Generate warming jobs according to the current strategy.
    ///
    /// Jobs are appended to the internal queue and also returned to the caller.
    pub fn generate_jobs(&self) -> Vec<WarmingJob> {
        let queries: Vec<String> = match &self.strategy {
            WarmingStrategy::AllTemplates => {
                self.patterns.iter().flat_map(|p| p.expand()).collect()
            }
            WarmingStrategy::TopK { k } => {
                let mut sorted = self.patterns.clone();
                sorted.sort_by(|a, b| {
                    b.frequency_weight
                        .partial_cmp(&a.frequency_weight)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                sorted.iter().take(*k).flat_map(|p| p.expand()).collect()
            }
            WarmingStrategy::ScheduledPatterns => self
                .patterns
                .iter()
                .filter(|p| p.template.starts_with("scheduled:"))
                .flat_map(|p| p.expand())
                .collect(),
            WarmingStrategy::UserDefined(queries) => queries.clone(),
        };

        let now = Instant::now();
        let mut new_jobs: Vec<WarmingJob> = queries
            .into_iter()
            .map(|query| {
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                WarmingJob {
                    id,
                    query,
                    model: "default".to_string(),
                    priority: 128,
                    scheduled_at: now,
                    status: WarmingStatus::Pending,
                }
            })
            .collect();

        self.stat_total
            .fetch_add(new_jobs.len() as u64, Ordering::Relaxed);

        let mut guard = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        guard.append(&mut new_jobs.clone());

        new_jobs
    }

    /// Mark a job as successfully completed.
    pub fn mark_completed(&self, job_id: u64) {
        let mut guard = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = guard.iter_mut().find(|j| j.id == job_id) {
            job.status = WarmingStatus::Completed {
                cached_at: Instant::now(),
            };
            self.stat_completed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Mark a job as failed with a descriptive reason.
    pub fn mark_failed(&self, job_id: u64, reason: &str) {
        let mut guard = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = guard.iter_mut().find(|j| j.id == job_id) {
            job.status = WarmingStatus::Failed(reason.to_string());
            self.stat_failed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Return clones of all pending jobs.
    pub fn pending_jobs(&self) -> Vec<WarmingJob> {
        let guard = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .iter()
            .filter(|j| matches!(j.status, WarmingStatus::Pending))
            .cloned()
            .collect()
    }

    /// Snapshot current statistics.
    pub fn stats(&self) -> WarmingStats {
        let completed = self.stat_completed.load(Ordering::Relaxed);
        WarmingStats {
            total_jobs: self.stat_total.load(Ordering::Relaxed),
            completed,
            failed: self.stat_failed.load(Ordering::Relaxed),
            cache_entries_added: completed,
            time_spent_ms: self.stat_time_ms.load(Ordering::Relaxed),
        }
    }

    /// Record additional wall-clock time spent on warming (in milliseconds).
    pub fn record_time_ms(&self, ms: u64) {
        self.stat_time_ms.fetch_add(ms, Ordering::Relaxed);
    }

    /// Return a default set of common warming patterns.
    ///
    /// Covers the four most common LLM task families: explain, summarise,
    /// translate, and code generation.
    pub fn default_patterns() -> Vec<WarmingPattern> {
        vec![
            WarmingPattern {
                template: "explain {topic} in simple terms".to_string(),
                variables: vec![(
                    "topic".to_string(),
                    vec![
                        "recursion".to_string(),
                        "machine learning".to_string(),
                        "async/await".to_string(),
                        "the TCP handshake".to_string(),
                        "gradient descent".to_string(),
                    ],
                )],
                frequency_weight: 0.9,
            },
            WarmingPattern {
                template: "summarize the following text: {text}".to_string(),
                variables: vec![(
                    "text".to_string(),
                    vec![
                        "Lorem ipsum dolor sit amet".to_string(),
                        "The quick brown fox jumps over the lazy dog".to_string(),
                    ],
                )],
                frequency_weight: 0.8,
            },
            WarmingPattern {
                template: "translate \"{phrase}\" to {language}".to_string(),
                variables: vec![
                    (
                        "phrase".to_string(),
                        vec!["Hello, world!".to_string(), "Thank you".to_string()],
                    ),
                    (
                        "language".to_string(),
                        vec![
                            "Spanish".to_string(),
                            "French".to_string(),
                            "German".to_string(),
                        ],
                    ),
                ],
                frequency_weight: 0.7,
            },
            WarmingPattern {
                template: "write code for {task} in {language}".to_string(),
                variables: vec![
                    (
                        "task".to_string(),
                        vec![
                            "a binary search".to_string(),
                            "a linked list".to_string(),
                            "a REST API endpoint".to_string(),
                        ],
                    ),
                    (
                        "language".to_string(),
                        vec![
                            "Rust".to_string(),
                            "Python".to_string(),
                            "TypeScript".to_string(),
                        ],
                    ),
                ],
                frequency_weight: 0.85,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_cartesian_product() {
        let p = WarmingPattern {
            template: "explain {topic} to a {audience}".to_string(),
            variables: vec![
                (
                    "topic".to_string(),
                    vec!["Rust".to_string(), "Python".to_string()],
                ),
                (
                    "audience".to_string(),
                    vec!["beginner".to_string(), "expert".to_string()],
                ),
            ],
            frequency_weight: 1.0,
        };
        let results = p.expand();
        assert_eq!(results.len(), 4);
        assert!(results.contains(&"explain Rust to a beginner".to_string()));
        assert!(results.contains(&"explain Python to a expert".to_string()));
    }

    #[test]
    fn expand_respects_100_limit() {
        let values: Vec<String> = (0..200).map(|i| i.to_string()).collect();
        let p = WarmingPattern {
            template: "item {n}".to_string(),
            variables: vec![("n".to_string(), values)],
            frequency_weight: 1.0,
        };
        assert!(p.expand().len() <= 100);
    }

    #[test]
    fn generate_jobs_and_lifecycle() {
        let mut warmer = CacheWarmer::new(WarmingStrategy::AllTemplates);
        warmer.add_pattern(WarmingPattern {
            template: "ping".to_string(),
            variables: vec![],
            frequency_weight: 1.0,
        });
        let jobs = warmer.generate_jobs();
        assert_eq!(jobs.len(), 1);
        let id = jobs[0].id;
        warmer.mark_completed(id);
        let stats = warmer.stats();
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.cache_entries_added, 1);
        assert_eq!(warmer.pending_jobs().len(), 0);
    }

    #[test]
    fn default_patterns_non_empty() {
        let patterns = CacheWarmer::default_patterns();
        assert!(!patterns.is_empty());
        for p in &patterns {
            assert!(!p.expand().is_empty());
        }
    }
}
