//! # Job Scheduler
//!
//! Cron-like job scheduler supporting interval, one-shot, and wall-clock
//! cron scheduling with async handlers.
//!
//! ## Example
//!
//! ```rust
//! use std::time::Duration;
//! use tokio_prompt_orchestrator::job_scheduler::{Schedule, Scheduler};
//!
//! # #[tokio::main]
//! # async fn main() {
//! let scheduler = Scheduler::new();
//! let id = scheduler.schedule("my-job", Schedule::Interval(Duration::from_secs(60)), || {
//!     Box::pin(async { println!("tick!"); })
//! });
//! // scheduler.start(); // spawns background loop
//! # }
//! ```

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures::future::BoxFuture;
use tokio::{sync::Mutex, task::JoinHandle, time::sleep};

// ── Schedule ──────────────────────────────────────────────────────────────────

/// Defines when a [`Job`] should fire.
#[derive(Debug, Clone)]
pub enum Schedule {
    /// Fire every fixed duration after the previous run.
    Interval(Duration),
    /// Fire exactly once at the specified [`Instant`].
    Once(Instant),
    /// Fire at a specific wall-clock time.  `None` fields are wildcards.
    Cron {
        /// Hour of day (0–23), or `None` to match any hour.
        hour: Option<u8>,
        /// Minute of hour (0–59), or `None` to match any minute.
        minute: Option<u8>,
        /// Second of minute (0–59), or `None` to match any second.
        second: Option<u8>,
    },
}

impl Schedule {
    /// Compute the next fire [`Instant`] relative to `now`.
    ///
    /// For [`Schedule::Cron`], this performs wall-clock arithmetic using
    /// [`SystemTime`] to find the next second that matches all non-`None`
    /// fields.
    pub fn next_tick(&self, now: Instant) -> Instant {
        match self {
            Schedule::Interval(d) => now + *d,
            Schedule::Once(at) => *at,
            Schedule::Cron { hour, minute, second } => {
                cron_next_tick(*hour, *minute, *second, now)
            }
        }
    }
}

/// Compute the next wall-clock second that satisfies the cron constraints and
/// map it back to a monotonic [`Instant`].
fn cron_next_tick(
    hour: Option<u8>,
    minute: Option<u8>,
    second: Option<u8>,
    now: Instant,
) -> Instant {
    // Current wall-clock time (seconds since epoch).
    let wall_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Search forward one second at a time (up to 24 h ahead).
    let max_search = 24 * 3600_u64;
    let mut candidate = wall_now + 1; // at least one second in the future

    for _ in 0..max_search {
        // Decompose candidate into h/m/s.
        let s = candidate % 60;
        let m = (candidate / 60) % 60;
        let h = (candidate / 3600) % 24;

        let h_ok = hour.map_or(true, |hh| hh as u64 == h);
        let m_ok = minute.map_or(true, |mm| mm as u64 == m);
        let s_ok = second.map_or(true, |ss| ss as u64 == s);

        if h_ok && m_ok && s_ok {
            // Convert wall-clock delta back to a monotonic Instant.
            let delta_secs = candidate.saturating_sub(wall_now);
            return now + Duration::from_secs(delta_secs);
        }
        candidate += 1;
    }

    // Fallback: 24 h from now (should never happen with valid cron fields).
    now + Duration::from_secs(86_400)
}

// ── JobId ─────────────────────────────────────────────────────────────────────

/// Opaque, unique identifier for a registered [`Job`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobId(u64);

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "job:{}", self.0)
    }
}

// ── JobHandler ────────────────────────────────────────────────────────────────

/// Type alias for an async job handler that can be cloned and sent across threads.
pub type JobHandler = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

// ── Job ───────────────────────────────────────────────────────────────────────

/// Metadata and scheduling state for a registered job.
#[derive(Debug, Clone)]
pub struct Job {
    /// Unique identifier assigned by the [`Scheduler`].
    pub id: JobId,
    /// Human-readable name.
    pub name: String,
    /// Schedule that determines when this job fires.
    pub schedule: Schedule,
    /// The last time this job started executing, if it has ever run.
    pub last_run: Option<Instant>,
    /// Total number of completed executions.
    pub run_count: u64,
    /// Whether this job should be considered by the run loop.
    pub enabled: bool,
}

impl Job {
    fn new(id: JobId, name: String, schedule: Schedule) -> Self {
        Self {
            id,
            name,
            schedule,
            last_run: None,
            run_count: 0,
            enabled: true,
        }
    }

    /// Compute the next scheduled tick based on `last_run` or `now`.
    fn next_tick(&self, now: Instant) -> Option<Instant> {
        match &self.schedule {
            Schedule::Once(_) if self.run_count > 0 => None, // already fired
            _ => Some(self.schedule.next_tick(self.last_run.unwrap_or(now - Duration::from_secs(1)))),
        }
    }
}

// ── JobStats ──────────────────────────────────────────────────────────────────

/// A snapshot of job scheduling statistics.
#[derive(Debug, Clone)]
pub struct JobStats {
    /// The job's unique identifier.
    pub id: JobId,
    /// The job's human-readable name.
    pub name: String,
    /// Total number of completed executions.
    pub run_count: u64,
    /// When the job last ran (monotonic).
    pub last_run: Option<Instant>,
    /// When the job will next run (monotonic), if applicable.
    pub next_run: Option<Instant>,
    /// Whether the job is currently enabled.
    pub enabled: bool,
}

// ── Scheduler ─────────────────────────────────────────────────────────────────

type JobMap = HashMap<JobId, (Job, JobHandler)>;

/// Async job scheduler with interval, one-shot, and cron-style scheduling.
///
/// Create a [`Scheduler`], register jobs with [`Scheduler::schedule`], then
/// call [`Scheduler::start`] to launch the background run loop.
#[derive(Clone)]
pub struct Scheduler {
    jobs: Arc<Mutex<JobMap>>,
    next_id: Arc<AtomicU64>,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler {
    /// Create a new, empty scheduler.
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Register a new job and return its [`JobId`].
    ///
    /// `handler` is called every time the schedule fires.  It must return a
    /// `BoxFuture<'static, ()>`.
    pub async fn schedule<F>(&self, name: &str, schedule: Schedule, handler: F) -> JobId
    where
        F: Fn() -> BoxFuture<'static, ()> + Send + Sync + 'static,
    {
        let id = JobId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let job = Job::new(id, name.to_string(), schedule);
        let handler: JobHandler = Arc::new(handler);
        self.jobs.lock().await.insert(id, (job, handler));
        id
    }

    /// Cancel a job by ID.  Returns `true` if the job existed.
    pub async fn cancel(&self, id: JobId) -> bool {
        self.jobs.lock().await.remove(&id).is_some()
    }

    /// Enable or disable a job.  Disabled jobs are skipped by the run loop.
    pub async fn enable(&self, id: JobId, enabled: bool) {
        let mut jobs = self.jobs.lock().await;
        if let Some((job, _)) = jobs.get_mut(&id) {
            job.enabled = enabled;
        }
    }

    /// Return a snapshot of stats for all registered jobs.
    pub async fn stats(&self) -> Vec<JobStats> {
        let now = Instant::now();
        let jobs = self.jobs.lock().await;
        jobs.values()
            .map(|(job, _)| JobStats {
                id: job.id,
                name: job.name.clone(),
                run_count: job.run_count,
                last_run: job.last_run,
                next_run: job.next_tick(now),
                enabled: job.enabled,
            })
            .collect()
    }

    /// Background run loop.  Checks all jobs every 100 ms and fires any whose
    /// `next_tick` has arrived.  Each handler is spawned in its own Tokio task.
    pub async fn run_loop(&self) {
        loop {
            sleep(Duration::from_millis(100)).await;
            let now = Instant::now();

            // Collect jobs that should fire this tick.
            let mut to_fire: Vec<(JobId, JobHandler)> = Vec::new();
            {
                let mut jobs = self.jobs.lock().await;
                for (job, handler) in jobs.values_mut() {
                    if !job.enabled {
                        continue;
                    }
                    let next = match job.next_tick(now) {
                        Some(t) => t,
                        None => continue, // Once job already ran
                    };
                    if next <= now {
                        to_fire.push((job.id, Arc::clone(handler)));
                        job.last_run = Some(now);
                        job.run_count += 1;
                    }
                }
            }

            // Spawn each handler independently.
            for (_id, handler) in to_fire {
                tokio::spawn(handler());
            }
        }
    }

    /// Spawn the run loop as a background Tokio task and return its handle.
    pub fn start(&self) -> JoinHandle<()> {
        let scheduler = self.clone();
        tokio::spawn(async move { scheduler.run_loop().await })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AOrdering};
    use std::sync::Arc;
    use tokio::time::sleep;

    #[tokio::test]
    async fn test_interval_fires_repeatedly() {
        let counter = Arc::new(AtomicUsize::new(0));
        let scheduler = Scheduler::new();

        let c = Arc::clone(&counter);
        scheduler
            .schedule("counter", Schedule::Interval(Duration::from_millis(10)), move || {
                let cc = Arc::clone(&c);
                Box::pin(async move {
                    cc.fetch_add(1, AOrdering::Relaxed);
                })
            })
            .await;

        let handle = scheduler.start();
        // Wait long enough for multiple firings (at least ~3).
        sleep(Duration::from_millis(250)).await;
        handle.abort();

        let count = counter.load(AOrdering::Relaxed);
        assert!(count >= 3, "expected >= 3 firings, got {}", count);
    }

    #[tokio::test]
    async fn test_cancel_stops_firing() {
        let counter = Arc::new(AtomicUsize::new(0));
        let scheduler = Scheduler::new();

        let c = Arc::clone(&counter);
        let id = scheduler
            .schedule("cancel-me", Schedule::Interval(Duration::from_millis(10)), move || {
                let cc = Arc::clone(&c);
                Box::pin(async move {
                    cc.fetch_add(1, AOrdering::Relaxed);
                })
            })
            .await;

        let handle = scheduler.start();
        // Let it fire a couple of times.
        sleep(Duration::from_millis(80)).await;
        scheduler.cancel(id).await;
        let count_after_cancel = counter.load(AOrdering::Relaxed);
        // Wait more time — count should not grow.
        sleep(Duration::from_millis(150)).await;
        handle.abort();

        let final_count = counter.load(AOrdering::Relaxed);
        assert_eq!(count_after_cancel, final_count, "job fired after cancel");
    }

    #[tokio::test]
    async fn test_disabled_job_skips() {
        let counter = Arc::new(AtomicUsize::new(0));
        let scheduler = Scheduler::new();

        let c = Arc::clone(&counter);
        let id = scheduler
            .schedule("disabled", Schedule::Interval(Duration::from_millis(10)), move || {
                let cc = Arc::clone(&c);
                Box::pin(async move {
                    cc.fetch_add(1, AOrdering::Relaxed);
                })
            })
            .await;

        // Disable before starting.
        scheduler.enable(id, false).await;

        let handle = scheduler.start();
        sleep(Duration::from_millis(150)).await;
        handle.abort();

        assert_eq!(counter.load(AOrdering::Relaxed), 0, "disabled job must not fire");
    }

    #[tokio::test]
    async fn test_once_fires_once() {
        let counter = Arc::new(AtomicUsize::new(0));
        let scheduler = Scheduler::new();

        let c = Arc::clone(&counter);
        scheduler
            .schedule(
                "once",
                Schedule::Once(Instant::now() + Duration::from_millis(30)),
                move || {
                    let cc = Arc::clone(&c);
                    Box::pin(async move {
                        cc.fetch_add(1, AOrdering::Relaxed);
                    })
                },
            )
            .await;

        let handle = scheduler.start();
        sleep(Duration::from_millis(250)).await;
        handle.abort();

        assert_eq!(counter.load(AOrdering::Relaxed), 1, "once job should fire exactly once");
    }

    #[test]
    fn test_schedule_next_tick_interval() {
        let now = Instant::now();
        let sched = Schedule::Interval(Duration::from_secs(5));
        let next = sched.next_tick(now);
        assert!(next > now);
        assert!(next <= now + Duration::from_secs(6));
    }

    #[test]
    fn test_schedule_next_tick_once() {
        let target = Instant::now() + Duration::from_secs(10);
        let sched = Schedule::Once(target);
        assert_eq!(sched.next_tick(Instant::now()), target);
    }

    #[test]
    fn test_cron_next_tick_is_future() {
        let now = Instant::now();
        let sched = Schedule::Cron { hour: None, minute: None, second: None };
        let next = sched.next_tick(now);
        assert!(next > now, "cron next tick should be in the future");
    }
}
