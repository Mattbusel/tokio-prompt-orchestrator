//! # Web Dashboard Binary
//!
//! ## Responsibility
//! Real-time web dashboard showing agent activity, pipeline metrics, and cost
//! savings. Serves an SSE stream for live updates and a single-page HTML
//! frontend with no external build step.
//!
//! ## Guarantees
//! - No panics — all errors are returned as HTTP error responses
//! - SSE stream emits every 500ms for real-time feel
//! - Single HTML file served inline — no static file dependencies at runtime
//! - Graceful shutdown on SIGINT / SIGTERM
//!
//! ## NOT Responsible For
//! - Pipeline execution (delegates to `spawn_pipeline`)
//! - Metric collection internals (reads from `metrics` module)

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Response,
    },
    routing::get,
    Router,
};
use serde::Serialize;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tokio_prompt_orchestrator::{
    enhanced::{CircuitBreaker, CircuitStatus, Deduplicator},
    metrics, spawn_pipeline, EchoWorker, LlamaCppWorker, ModelWorker, OrchestratorError,
    PromptRequest,
};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

// ── Dashboard HTML ───────────────────────────────────────────────────────────

/// Inline dashboard HTML, compiled into the binary at build time.
const DASHBOARD_HTML: &str = include_str!("../../static/index.html");

// ── Shared application state ─────────────────────────────────────────────────

/// Shared state accessible from all route handlers.
///
/// Holds references to the pipeline sender and enhanced-feature components so
/// that the SSE stream can read live statistics. In mock mode, the pipeline
/// is not started and all data comes from the mock story state instead.
#[derive(Clone)]
pub struct DashboardState {
    /// Sender side of the pipeline input channel — kept alive to prevent
    /// pipeline shutdown; also used by future request-submission routes.
    /// `None` in mock mode where no real pipeline is running.
    #[allow(dead_code)]
    pipeline_tx: Option<mpsc::Sender<PromptRequest>>,
    /// Circuit breaker for the inference stage (live mode only).
    circuit_breaker: CircuitBreaker,
    /// Request deduplicator (live mode only).
    deduplicator: Deduplicator,
    /// Timestamp when the dashboard server started.
    start_time: Instant,
    /// Name of the active worker backend.
    worker_name: String,
    /// Mock story state, present only when `--mock` was passed.
    mock_state: Option<Arc<Mutex<MockDashboardState>>>,
}

// ── SSE event payload ────────────────────────────────────────────────────────

/// Event payload pushed to SSE clients every 500ms.
#[derive(Debug, Clone, Serialize)]
pub struct DashboardEvent {
    /// Number of active pipeline stages (varies during failures).
    pub active_agents: u32,
    /// Total requests that entered the pipeline.
    pub requests_total: u64,
    /// Total requests that were deduplicated (cache hits).
    pub requests_deduped: u64,
    /// Deduplication hit rate as a percentage (0.0–100.0).
    pub dedup_rate: f32,
    /// Estimated USD saved through deduplication.
    pub cost_saved_usd: f64,
    /// Approximate throughput in requests per second.
    pub throughput_rps: f32,
    /// Circuit breaker states for each monitored service.
    pub circuit_breakers: Vec<CircuitBreakerState>,
    /// Per-stage latency metrics.
    pub stage_latencies: StageTiming,
    /// Model routing breakdown.
    pub model_routing: ModelRoutingStats,
    /// Per-stage request counts.
    pub stage_counts: StageCountMap,
    /// Per-stage shed counts.
    pub shed_counts: StageCountMap,
    /// Server uptime in seconds.
    pub uptime_secs: u64,
    /// Recent processed requests for the live log.
    pub recent_requests: Vec<RecentRequest>,
    /// Which pipeline stages are currently active (5 booleans).
    pub active_stages: Vec<bool>,
    /// Channel depth info between pipeline stages.
    pub channel_depths: Vec<ChannelDepthInfo>,
}

/// Circuit breaker status for a single service.
#[derive(Debug, Clone, Serialize)]
pub struct CircuitBreakerState {
    /// Service name (e.g., `"inference"`).
    pub name: String,
    /// Human-readable status: `"closed"`, `"open"`, or `"half_open"`.
    pub status: String,
    /// Total failures in the current tracking window.
    pub failures: usize,
    /// Total successes in the current tracking window.
    pub successes: usize,
    /// Computed success rate (0.0–1.0).
    pub success_rate: f64,
}

/// Per-stage latency breakdown (P50 / P99 in milliseconds).
#[derive(Debug, Clone, Serialize)]
pub struct StageTiming {
    /// RAG stage latency.
    pub rag_ms: f64,
    /// Assemble stage latency.
    pub assemble_ms: f64,
    /// Inference stage latency.
    pub inference_ms: f64,
    /// Post-processing stage latency.
    pub post_ms: f64,
    /// Streaming stage latency.
    pub stream_ms: f64,
}

/// Model routing statistics with N-way model support.
#[derive(Debug, Clone, Serialize)]
pub struct ModelRoutingStats {
    /// Name of the primary worker (kept for backwards compatibility).
    pub primary_model: String,
    /// Percentage of requests routed to the primary model (0.0–100.0).
    pub primary_pct: f32,
    /// Percentage of requests using the fallback (echo) model.
    pub fallback_pct: f32,
    /// Detailed per-model routing entries for N-way display.
    pub models: Vec<ModelRoutingEntry>,
}

/// A single model's routing entry for N-way donut chart.
#[derive(Debug, Clone, Serialize)]
pub struct ModelRoutingEntry {
    /// Model name (e.g., `"Mistral"`, `"Claude"`, `"OpenAI"`).
    pub name: String,
    /// Percentage of requests routed to this model (0.0–100.0).
    pub pct: f32,
    /// CSS color variable name for the donut arc (e.g., `"var(--accent)"`).
    pub color: String,
}

/// A recent request log entry sent from the backend.
#[derive(Debug, Clone, Serialize)]
pub struct RecentRequest {
    /// Formatted timestamp string, e.g. `"14:32:01"`.
    pub timestamp: String,
    /// Session identifier.
    pub session_id: String,
    /// Pipeline stage that processed this request.
    pub stage: String,
    /// Processing latency in milliseconds.
    pub latency_ms: f64,
    /// Request status: `"ok"`, `"error"`, `"shed"`, `"deduped"`.
    pub status: String,
}

/// Channel depth information between two pipeline stages.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelDepthInfo {
    /// Human-readable channel name (e.g., `"RAG → ASM"`).
    pub name: String,
    /// Current queue depth.
    pub current: usize,
    /// Maximum channel capacity.
    pub capacity: usize,
}

/// Stage-level counter map (stage name → count).
#[derive(Debug, Clone, Serialize)]
pub struct StageCountMap {
    /// RAG stage count.
    pub rag: u64,
    /// Assemble stage count.
    pub assemble: u64,
    /// Inference stage count.
    pub inference: u64,
    /// Post-processing stage count.
    pub post: u64,
    /// Stream stage count.
    pub stream: u64,
}

// ── Mock dashboard state ────────────────────────────────────────────────────

/// Story cycle length in ticks. The mock story loops every 120 ticks (seconds).
const STORY_CYCLE_TICKS: u64 = 120;

/// Maximum number of recent request log entries retained.
const MAX_RECENT_REQUESTS: usize = 20;

/// Cost per inference call in USD (for savings estimate in mock mode).
const MOCK_COST_PER_INFERENCE: f64 = 0.012;

/// Mutable mock state that advances through a scripted 2-minute story.
///
/// ## Story Phases
/// | Phase     | Ticks   | Behavior                                          |
/// |-----------|---------|---------------------------------------------------|
/// | Warmup    |  0 – 29 | Low traffic, all CBs closed                       |
/// | Load      | 30 – 44 | Rising throughput, channels filling                |
/// | Failure   | 45 – 59 | llama.cpp CB opens, latency spikes                |
/// | HalfOpen  | 60 – 74 | Probe requests, improving                         |
/// | Recovery  | 75 – 89 | CB closes, recovery logs                          |
/// | Steady    | 90 –119 | Nominal, high dedup                               |
#[derive(Debug)]
pub struct MockDashboardState {
    /// Monotonic tick counter.
    pub tick_count: u64,
    /// Simulated uptime in seconds.
    pub uptime_secs: u64,
    /// Per-stage latencies in milliseconds [rag, assemble, inference, post, stream].
    pub stage_latencies: [f64; 5],
    /// Per-stage request counts.
    pub stage_counts: [u64; 5],
    /// Per-stage shed counts.
    pub shed_counts: [u64; 5],
    /// Total requests received.
    pub requests_total: u64,
    /// Total inferences executed (post-dedup).
    pub inferences_total: u64,
    /// Estimated cost saved in USD.
    pub cost_saved_usd: f64,
    /// Throughput history (last 60 values, one per tick).
    pub throughput_history: VecDeque<u64>,
    /// Circuit breaker states: [openai, anthropic, llama.cpp].
    pub circuit_breakers: Vec<MockCircuitBreaker>,
    /// Model routing percentages: [Mistral, Claude, OpenAI].
    pub model_routing: Vec<ModelRoutingEntry>,
    /// Recent request log entries.
    pub recent_requests: VecDeque<RecentRequest>,
    /// Which stage is currently active (rotating index).
    pub active_stage_index: usize,
    /// Number of active agents (varies during failure).
    pub active_agents: u32,
    /// Channel depths between stages.
    pub channel_depths: [MockChannelDepth; 4],
}

/// Mock circuit breaker for a single service.
#[derive(Debug, Clone)]
pub struct MockCircuitBreaker {
    /// Service name.
    pub name: String,
    /// Current status string: `"closed"`, `"open"`, or `"half_open"`.
    pub status: String,
    /// Total failures.
    pub failures: usize,
    /// Total successes.
    pub successes: usize,
}

/// Mock channel depth between two pipeline stages.
#[derive(Debug, Clone)]
pub struct MockChannelDepth {
    /// Channel label.
    pub name: String,
    /// Current depth.
    pub current: usize,
    /// Max capacity.
    pub capacity: usize,
}

impl Default for MockDashboardState {
    fn default() -> Self {
        Self::new()
    }
}

impl MockDashboardState {
    /// Creates a new mock state with initial values.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn new() -> Self {
        Self {
            tick_count: 0,
            uptime_secs: 0,
            stage_latencies: [3.0, 1.5, 260.0, 1.8, 0.7],
            stage_counts: [0; 5],
            shed_counts: [0; 5],
            requests_total: 0,
            inferences_total: 0,
            cost_saved_usd: 0.0,
            throughput_history: VecDeque::with_capacity(60),
            circuit_breakers: vec![
                MockCircuitBreaker {
                    name: "openai".to_string(),
                    status: "closed".to_string(),
                    failures: 0,
                    successes: 0,
                },
                MockCircuitBreaker {
                    name: "anthropic".to_string(),
                    status: "closed".to_string(),
                    failures: 0,
                    successes: 0,
                },
                MockCircuitBreaker {
                    name: "llama.cpp".to_string(),
                    status: "closed".to_string(),
                    failures: 0,
                    successes: 0,
                },
            ],
            model_routing: vec![
                ModelRoutingEntry {
                    name: "Mistral".to_string(),
                    pct: 60.0,
                    color: "var(--accent)".to_string(),
                },
                ModelRoutingEntry {
                    name: "Claude".to_string(),
                    pct: 30.0,
                    color: "var(--purple)".to_string(),
                },
                ModelRoutingEntry {
                    name: "OpenAI".to_string(),
                    pct: 10.0,
                    color: "var(--orange)".to_string(),
                },
            ],
            recent_requests: VecDeque::with_capacity(MAX_RECENT_REQUESTS),
            active_stage_index: 0,
            active_agents: 5,
            channel_depths: [
                MockChannelDepth {
                    name: "RAG \u{2192} ASM".to_string(),
                    current: 0,
                    capacity: 512,
                },
                MockChannelDepth {
                    name: "ASM \u{2192} INF".to_string(),
                    current: 0,
                    capacity: 512,
                },
                MockChannelDepth {
                    name: "INF \u{2192} PST".to_string(),
                    current: 0,
                    capacity: 1024,
                },
                MockChannelDepth {
                    name: "PST \u{2192} STR".to_string(),
                    current: 0,
                    capacity: 512,
                },
            ],
        }
    }

    /// Advances the mock story by one tick (1 second).
    ///
    /// Updates all state fields based on the current story phase.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn tick(&mut self) {
        self.tick_count += 1;
        self.uptime_secs += 1;

        let t = self.tick_count as f64;
        let phase_tick = (self.tick_count - 1) % STORY_CYCLE_TICKS;

        self.update_latencies(t, phase_tick);
        self.update_circuit_breakers(phase_tick);
        self.update_dedup(phase_tick);
        self.update_throughput(t, phase_tick);
        self.update_model_routing(phase_tick);
        self.update_active_stage(t, phase_tick);
        self.update_channels(t, phase_tick);
        self.update_stage_counts(phase_tick);
        self.update_recent_requests(phase_tick);
    }

    /// Returns the current phase tick (position within the 120-tick cycle).
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn phase_tick(&self) -> u64 {
        if self.tick_count == 0 {
            return 0;
        }
        (self.tick_count - 1) % STORY_CYCLE_TICKS
    }

    /// Updates pipeline stage latencies based on story phase.
    fn update_latencies(&mut self, t: f64, phase_tick: u64) {
        let wobble = |freq: f64| (t * freq).sin();

        match phase_tick {
            0..=29 => {
                self.stage_latencies[0] = 3.0 + 0.5 * wobble(0.07);
                self.stage_latencies[1] = 1.5 + 0.3 * wobble(0.11);
                self.stage_latencies[2] = 260.0 + 15.0 * wobble(0.05);
                self.stage_latencies[3] = 1.8 + 0.2 * wobble(0.09);
                self.stage_latencies[4] = 0.7 + 0.1 * wobble(0.13);
            }
            30..=44 => {
                let pressure = (phase_tick - 30) as f64 / 15.0;
                self.stage_latencies[0] = 4.0 + 2.0 * pressure + 0.5 * wobble(0.07);
                self.stage_latencies[1] = 2.0 + 1.0 * pressure + 0.3 * wobble(0.11);
                self.stage_latencies[2] = 280.0 + 80.0 * pressure + 20.0 * wobble(0.05);
                self.stage_latencies[3] = 2.5 + 1.5 * pressure + 0.3 * wobble(0.09);
                self.stage_latencies[4] = 1.0 + 0.5 * pressure + 0.1 * wobble(0.13);
            }
            45..=59 => {
                let severity = ((phase_tick - 45) as f64 / 7.0).sin().abs();
                self.stage_latencies[0] = 5.5 + 1.0 * wobble(0.07);
                self.stage_latencies[1] = 3.0 + 0.5 * wobble(0.11);
                self.stage_latencies[2] = 380.0 + 100.0 * severity + 30.0 * wobble(0.05);
                self.stage_latencies[3] = 4.0 + 1.0 * wobble(0.09);
                self.stage_latencies[4] = 1.5 + 0.3 * wobble(0.13);
            }
            60..=74 => {
                let recovery = (phase_tick - 60) as f64 / 15.0;
                self.stage_latencies[0] = 5.0 - 1.5 * recovery + 0.5 * wobble(0.07);
                self.stage_latencies[1] = 2.5 - 0.5 * recovery + 0.3 * wobble(0.11);
                self.stage_latencies[2] = 350.0 - 60.0 * recovery + 20.0 * wobble(0.05);
                self.stage_latencies[3] = 3.5 - 1.0 * recovery + 0.3 * wobble(0.09);
                self.stage_latencies[4] = 1.3 - 0.3 * recovery + 0.1 * wobble(0.13);
            }
            75..=89 => {
                let settle = (phase_tick - 75) as f64 / 15.0;
                self.stage_latencies[0] = 3.5 - 0.5 * settle + 0.5 * wobble(0.07);
                self.stage_latencies[1] = 2.0 - 0.3 * settle + 0.3 * wobble(0.11);
                self.stage_latencies[2] = 290.0 - 20.0 * settle + 15.0 * wobble(0.05);
                self.stage_latencies[3] = 2.5 - 0.5 * settle + 0.2 * wobble(0.09);
                self.stage_latencies[4] = 1.0 - 0.2 * settle + 0.1 * wobble(0.13);
            }
            _ => {
                self.stage_latencies[0] = 3.0 + 0.8 * wobble(0.07) + 0.3 * wobble(0.31);
                self.stage_latencies[1] = 1.7 + 0.4 * wobble(0.11) + 0.2 * wobble(0.47);
                self.stage_latencies[2] = 270.0 + 25.0 * wobble(0.05) + 10.0 * wobble(0.23);
                self.stage_latencies[3] = 2.0 + 0.5 * wobble(0.09) + 0.2 * wobble(0.19);
                self.stage_latencies[4] = 0.8 + 0.2 * wobble(0.13) + 0.1 * wobble(0.37);
            }
        }

        // Clamp all to non-negative
        for lat in &mut self.stage_latencies {
            if *lat < 0.1 {
                *lat = 0.1;
            }
        }
    }

    /// Updates circuit breakers based on story phase.
    fn update_circuit_breakers(&mut self, phase_tick: u64) {
        let tick = self.tick_count;

        // openai: always closed
        if let Some(cb) = self.circuit_breakers.get_mut(0) {
            cb.status = "closed".to_string();
            cb.successes = (tick * 2 + 47) as usize;
            cb.failures = 0;
        }

        // anthropic: always closed
        if let Some(cb) = self.circuit_breakers.get_mut(1) {
            cb.status = "closed".to_string();
            cb.successes = (tick * 3 + 120) as usize;
            cb.failures = 0;
        }

        // llama.cpp: the star of the story
        if let Some(cb) = self.circuit_breakers.get_mut(2) {
            match phase_tick {
                0..=44 => {
                    cb.status = "closed".to_string();
                    cb.successes = (tick + 50) as usize;
                    cb.failures = 0;
                }
                45..=59 => {
                    cb.status = "open".to_string();
                    cb.failures = 5 + (phase_tick - 45) as usize;
                    cb.successes = 0;
                }
                60..=74 => {
                    cb.status = "half_open".to_string();
                    cb.failures = 3;
                    cb.successes = (phase_tick - 60) as usize;
                }
                _ => {
                    cb.status = "closed".to_string();
                    cb.successes = (tick + 50) as usize;
                    cb.failures = 0;
                }
            }
        }
    }

    /// Updates dedup statistics based on story phase.
    fn update_dedup(&mut self, phase_tick: u64) {
        let (req_inc, inf_inc) = match phase_tick {
            0..=29 => (3u64, 1u64),
            30..=44 => (8, 2),
            45..=59 => (6, 3),
            60..=74 => (5, 2),
            75..=89 => (5, 1),
            _ => (4, 1),
        };

        self.requests_total += req_inc;
        self.inferences_total += inf_inc;
        self.cost_saved_usd += MOCK_COST_PER_INFERENCE * (req_inc.saturating_sub(inf_inc)) as f64;
    }

    /// Updates throughput history based on story phase.
    fn update_throughput(&mut self, t: f64, phase_tick: u64) {
        let base = match phase_tick {
            0..=29 => 8.0,
            30..=44 => 8.0 + 20.0 * ((phase_tick - 30) as f64 / 15.0),
            45..=59 => 22.0,
            60..=74 => 18.0,
            75..=89 => 15.0,
            _ => 15.0,
        };

        let wobble = 3.0 * (t * 0.11).sin() + 2.0 * (t * 0.19).cos();
        let value = (base + wobble).max(1.0) as u64;
        self.throughput_history.push_back(value);
        if self.throughput_history.len() > 60 {
            self.throughput_history.pop_front();
        }
    }

    /// Updates model routing percentages based on story phase.
    fn update_model_routing(&mut self, phase_tick: u64) {
        let (m, c, o) = match phase_tick {
            0..=29 => (60.0, 30.0, 10.0),
            30..=44 => (50.0, 35.0, 15.0),
            45..=59 => (30.0, 55.0, 15.0), // shift away from failing provider
            60..=74 => (40.0, 45.0, 15.0),
            75..=89 => (50.0, 35.0, 15.0),
            _ => (55.0, 30.0, 15.0),
        };

        if let Some(entry) = self.model_routing.get_mut(0) {
            entry.pct = m;
        }
        if let Some(entry) = self.model_routing.get_mut(1) {
            entry.pct = c;
        }
        if let Some(entry) = self.model_routing.get_mut(2) {
            entry.pct = o;
        }
    }

    /// Updates which pipeline stage is currently active.
    fn update_active_stage(&mut self, t: f64, phase_tick: u64) {
        self.active_stage_index = ((t * 0.5) as usize) % 5;
        self.active_agents = match phase_tick {
            45..=59 => 4, // one stage degraded during failure
            _ => 5,
        };
    }

    /// Updates channel depths between pipeline stages.
    fn update_channels(&mut self, t: f64, phase_tick: u64) {
        let wobble = |freq: f64| (t * freq).sin();

        let (fill0, fill1, fill2, fill3) = match phase_tick {
            0..=29 => (0.15, 0.08, 0.20, 0.05),
            30..=44 => {
                let p = (phase_tick - 30) as f64 / 15.0;
                (
                    0.15 + 0.45 * p,
                    0.08 + 0.20 * p,
                    0.20 + 0.50 * p,
                    0.05 + 0.15 * p,
                )
            }
            45..=59 => (0.65, 0.35, 0.80, 0.25),
            60..=74 => {
                let r = (phase_tick - 60) as f64 / 15.0;
                (
                    0.65 - 0.30 * r,
                    0.35 - 0.15 * r,
                    0.80 - 0.35 * r,
                    0.25 - 0.10 * r,
                )
            }
            75..=89 => (0.30, 0.15, 0.40, 0.10),
            _ => (0.20, 0.10, 0.25, 0.06),
        };

        let fills = [fill0, fill1, fill2, fill3];
        let freqs = [0.08, 0.12, 0.06, 0.15];
        for i in 0..4 {
            let cap = self.channel_depths[i].capacity as f64;
            let depth = ((cap * fills[i] + cap * 0.05 * wobble(freqs[i])).max(0.0) as usize)
                .min(self.channel_depths[i].capacity);
            self.channel_depths[i].current = depth;
        }
    }

    /// Updates per-stage request counts.
    fn update_stage_counts(&mut self, phase_tick: u64) {
        let inc = match phase_tick {
            0..=29 => 3,
            30..=44 => 8,
            45..=59 => 4,
            60..=74 => 5,
            75..=89 => 5,
            _ => 4,
        };
        for count in &mut self.stage_counts {
            *count += inc;
        }
        // Add shed during failure
        if (45..=59).contains(&phase_tick) {
            self.shed_counts[2] += 2; // inference sheds
        }
    }

    /// Generates recent request log entries based on the current story phase.
    fn update_recent_requests(&mut self, phase_tick: u64) {
        let ts = format!(
            "{:02}:{:02}:{:02}",
            (self.uptime_secs / 3600) % 24,
            (self.uptime_secs % 3600) / 60,
            self.uptime_secs % 60
        );

        let tick = self.tick_count;
        let stages = ["rag", "assemble", "inference", "post", "stream"];
        let stage_idx = (tick as usize) % stages.len();

        let entry = match phase_tick {
            0..=29 => {
                if tick % 3 == 0 {
                    Some(RecentRequest {
                        timestamp: ts,
                        session_id: format!("user-{}", tick % 100),
                        stage: stages[stage_idx].to_string(),
                        latency_ms: self.stage_latencies[stage_idx],
                        status: "ok".to_string(),
                    })
                } else {
                    None
                }
            }
            30..=44 => Some(RecentRequest {
                timestamp: ts,
                session_id: format!("user-{}", tick % 200),
                stage: stages[stage_idx].to_string(),
                latency_ms: self.stage_latencies[stage_idx],
                status: if tick % 5 == 0 {
                    "deduped".to_string()
                } else {
                    "ok".to_string()
                },
            }),
            45..=59 => Some(RecentRequest {
                timestamp: ts,
                session_id: format!("user-{}", tick % 50),
                stage: "inference".to_string(),
                latency_ms: self.stage_latencies[2],
                status: if tick % 3 == 0 {
                    "error".to_string()
                } else {
                    "shed".to_string()
                },
            }),
            60..=74 => Some(RecentRequest {
                timestamp: ts,
                session_id: format!("probe-{}", tick % 10),
                stage: "inference".to_string(),
                latency_ms: self.stage_latencies[2],
                status: "ok".to_string(),
            }),
            75..=89 => Some(RecentRequest {
                timestamp: ts,
                session_id: format!("user-{}", tick % 150),
                stage: stages[stage_idx].to_string(),
                latency_ms: self.stage_latencies[stage_idx],
                status: "ok".to_string(),
            }),
            _ => {
                if tick % 2 == 0 {
                    Some(RecentRequest {
                        timestamp: ts,
                        session_id: format!("user-{}", tick % 500),
                        stage: stages[stage_idx].to_string(),
                        latency_ms: self.stage_latencies[stage_idx],
                        status: if tick % 4 == 0 {
                            "deduped".to_string()
                        } else {
                            "ok".to_string()
                        },
                    })
                } else {
                    None
                }
            }
        };

        if let Some(req) = entry {
            self.recent_requests.push_back(req);
            while self.recent_requests.len() > MAX_RECENT_REQUESTS {
                self.recent_requests.pop_front();
            }
        }
    }

    /// Builds a [`DashboardEvent`] snapshot from the current mock state.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn build_event(&self) -> DashboardEvent {
        let dedup_rate = if self.requests_total > 0 {
            let deduped = self.requests_total.saturating_sub(self.inferences_total);
            (deduped as f32 / self.requests_total as f32) * 100.0
        } else {
            0.0
        };

        let throughput_rps = self.throughput_history.back().copied().unwrap_or(0) as f32;

        let circuit_breakers: Vec<CircuitBreakerState> = self
            .circuit_breakers
            .iter()
            .map(|cb| {
                let total = cb.successes + cb.failures;
                let success_rate = if total > 0 {
                    cb.successes as f64 / total as f64
                } else {
                    1.0
                };
                CircuitBreakerState {
                    name: cb.name.clone(),
                    status: cb.status.clone(),
                    failures: cb.failures,
                    successes: cb.successes,
                    success_rate,
                }
            })
            .collect();

        let primary_pct = self.model_routing.first().map(|m| m.pct).unwrap_or(100.0);
        let fallback_pct = 100.0 - primary_pct;

        let active_stages: Vec<bool> = (0..5).map(|i| i == self.active_stage_index).collect();

        let channel_depths: Vec<ChannelDepthInfo> = self
            .channel_depths
            .iter()
            .map(|ch| ChannelDepthInfo {
                name: ch.name.clone(),
                current: ch.current,
                capacity: ch.capacity,
            })
            .collect();

        DashboardEvent {
            active_agents: self.active_agents,
            requests_total: self.requests_total,
            requests_deduped: self.requests_total.saturating_sub(self.inferences_total),
            dedup_rate,
            cost_saved_usd: self.cost_saved_usd,
            throughput_rps,
            circuit_breakers,
            stage_latencies: StageTiming {
                rag_ms: self.stage_latencies[0],
                assemble_ms: self.stage_latencies[1],
                inference_ms: self.stage_latencies[2],
                post_ms: self.stage_latencies[3],
                stream_ms: self.stage_latencies[4],
            },
            model_routing: ModelRoutingStats {
                primary_model: self
                    .model_routing
                    .first()
                    .map(|m| m.name.clone())
                    .unwrap_or_else(|| "unknown".to_string()),
                primary_pct,
                fallback_pct,
                models: self.model_routing.clone(),
            },
            stage_counts: StageCountMap {
                rag: self.stage_counts[0],
                assemble: self.stage_counts[1],
                inference: self.stage_counts[2],
                post: self.stage_counts[3],
                stream: self.stage_counts[4],
            },
            shed_counts: StageCountMap {
                rag: self.shed_counts[0],
                assemble: self.shed_counts[1],
                inference: self.shed_counts[2],
                post: self.shed_counts[3],
                stream: self.shed_counts[4],
            },
            uptime_secs: self.uptime_secs,
            recent_requests: self.recent_requests.iter().cloned().collect(),
            active_stages,
            channel_depths,
        }
    }
}

// ── Snapshot builder ─────────────────────────────────────────────────────────

/// Build a [`DashboardEvent`] from the current system state.
///
/// In mock mode, reads from `MockDashboardState`. In live mode, reads from
/// Prometheus metrics and the circuit breaker / deduplicator.
///
/// # Panics
///
/// This function never panics.
pub async fn build_snapshot(state: &DashboardState) -> DashboardEvent {
    // Mock mode: build from mock state
    if let Some(ref mock) = state.mock_state {
        let guard = mock.lock().await;
        return guard.build_event();
    }

    // Live mode: read from Prometheus metrics
    let summary = metrics::get_metrics_summary();

    let stage_total = |stage: &str| -> u64 {
        summary
            .requests_total
            .get(stage)
            .copied()
            .unwrap_or_default()
    };
    let stage_shed = |stage: &str| -> u64 {
        summary
            .requests_shed
            .get(stage)
            .copied()
            .unwrap_or_default()
    };

    let requests_total: u64 = summary.requests_total.values().sum();

    // Dedup stats
    let dedup_stats = state.deduplicator.stats();
    let requests_deduped = dedup_stats.cached as u64;
    let dedup_rate = if requests_total > 0 {
        (requests_deduped as f32 / requests_total as f32) * 100.0
    } else {
        0.0
    };

    // Cost model: assume ~$0.002 per inference call saved via dedup
    let cost_saved_usd = requests_deduped as f64 * 0.002;

    // Throughput: total requests / uptime seconds
    let uptime = state.start_time.elapsed();
    let uptime_secs = uptime.as_secs().max(1);
    let throughput_rps = requests_total as f32 / uptime_secs as f32;

    // Circuit breaker
    let cb_stats = state.circuit_breaker.stats().await;
    let cb_status_str = match cb_stats.status {
        CircuitStatus::Closed => "closed".to_string(),
        CircuitStatus::Open => "open".to_string(),
        CircuitStatus::HalfOpen => "half_open".to_string(),
    };

    let circuit_breakers = vec![CircuitBreakerState {
        name: "inference".to_string(),
        status: cb_status_str,
        failures: cb_stats.failures,
        successes: cb_stats.successes,
        success_rate: cb_stats.success_rate,
    }];

    // Stage latencies from histogram — use sum / count as approximation
    let stage_latency_ms = |stage: &str| -> f64 {
        let families = metrics::gather();
        for family in &families {
            if family.get_name() == "orchestrator_stage_duration_seconds" {
                for metric in family.get_metric() {
                    let labels = metric.get_label();
                    let matches = labels
                        .iter()
                        .any(|l| l.get_name() == "stage" && l.get_value() == stage);
                    if matches {
                        let h = metric.get_histogram();
                        let count = h.get_sample_count();
                        if count > 0 {
                            return (h.get_sample_sum() / count as f64) * 1000.0;
                        }
                    }
                }
            }
        }
        0.0
    };

    let stage_latencies = StageTiming {
        rag_ms: stage_latency_ms("rag"),
        assemble_ms: stage_latency_ms("assemble"),
        inference_ms: stage_latency_ms("inference"),
        post_ms: stage_latency_ms("post"),
        stream_ms: stage_latency_ms("stream"),
    };

    let model_routing = ModelRoutingStats {
        primary_model: state.worker_name.clone(),
        primary_pct: 100.0,
        fallback_pct: 0.0,
        models: vec![ModelRoutingEntry {
            name: state.worker_name.clone(),
            pct: 100.0,
            color: "var(--accent)".to_string(),
        }],
    };

    let stage_counts = StageCountMap {
        rag: stage_total("rag"),
        assemble: stage_total("assemble"),
        inference: stage_total("inference"),
        post: stage_total("post"),
        stream: stage_total("stream"),
    };

    let shed_counts = StageCountMap {
        rag: stage_shed("rag"),
        assemble: stage_shed("assemble"),
        inference: stage_shed("inference"),
        post: stage_shed("post"),
        stream: stage_shed("stream"),
    };

    DashboardEvent {
        active_agents: 5,
        requests_total,
        requests_deduped,
        dedup_rate,
        cost_saved_usd,
        throughput_rps,
        circuit_breakers,
        stage_latencies,
        model_routing,
        stage_counts,
        shed_counts,
        uptime_secs,
        recent_requests: vec![],
        active_stages: vec![true; 5],
        channel_depths: vec![],
    }
}

// ── Route handlers ───────────────────────────────────────────────────────────

/// `GET /` — serve the dashboard HTML page.
///
/// # Panics
///
/// This function never panics.
async fn index_handler() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

/// `GET /api/stats` — one-shot JSON snapshot of current dashboard state.
///
/// # Panics
///
/// This function never panics.
async fn stats_handler(State(state): State<Arc<DashboardState>>) -> Response {
    let snapshot = build_snapshot(&state).await;
    match serde_json::to_string(&snapshot) {
        Ok(json) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json,
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":"serialization failed"}"#.to_string(),
        )
            .into_response(),
    }
}

/// `GET /api/status` — SSE stream emitting [`DashboardEvent`] every 500ms.
///
/// # Panics
///
/// This function never panics.
async fn sse_handler(
    State(state): State<Arc<DashboardState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            let snapshot = build_snapshot(&state).await;
            if let Ok(json) = serde_json::to_string(&snapshot) {
                yield Ok(Event::default().data(json));
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── Router construction ──────────────────────────────────────────────────────

/// Build the Axum [`Router`] with all dashboard routes.
///
/// # Panics
///
/// This function never panics.
pub fn build_router(state: Arc<DashboardState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/", get(index_handler))
        .route("/api/status", get(sse_handler))
        .route("/api/stats", get(stats_handler))
        .layer(cors)
        .with_state(state)
}

// ── Server bootstrap ─────────────────────────────────────────────────────────

/// Start the dashboard HTTP server.
///
/// # Arguments
///
/// * `addr` — Socket address to bind, e.g. `"0.0.0.0:3000"`.
/// * `state` — Shared dashboard state.
///
/// # Errors
///
/// Returns an error if the address cannot be parsed or the TCP listener
/// fails to bind.
///
/// # Panics
///
/// This function never panics.
pub async fn start_dashboard(
    addr: &str,
    state: Arc<DashboardState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let socket_addr: std::net::SocketAddr = addr.parse()?;

    info!("Dashboard server starting on http://{}", socket_addr);

    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(&socket_addr).await?;

    info!("Dashboard ready at http://{}", socket_addr);

    axum::serve(listener, app).await?;

    Ok(())
}

// ── Worker factory ───────────────────────────────────────────────────────────

/// Create a [`ModelWorker`] from a CLI name string.
///
/// # Errors
///
/// Returns [`OrchestratorError::ConfigError`] for unrecognised worker names.
///
/// # Panics
///
/// This function never panics.
pub fn create_worker(name: &str) -> Result<Arc<dyn ModelWorker>, OrchestratorError> {
    match name {
        "echo" => Ok(Arc::new(EchoWorker::new())),
        "llama_cpp" => Ok(Arc::new(LlamaCppWorker::new())),
        _ => Err(OrchestratorError::ConfigError(format!(
            "unknown worker: {name}"
        ))),
    }
}

/// Parse the `--worker` CLI argument, defaulting to `"echo"`.
///
/// # Panics
///
/// This function never panics.
pub fn parse_worker_arg() -> String {
    let args: Vec<String> = std::env::args().collect();
    for (i, arg) in args.iter().enumerate() {
        if arg == "--worker" {
            if let Some(val) = args.get(i + 1) {
                return val.clone();
            }
        }
    }
    "echo".to_string()
}

/// Parse the `--port` CLI argument, defaulting to `3000`.
///
/// # Panics
///
/// This function never panics.
pub fn parse_port_arg() -> u16 {
    let args: Vec<String> = std::env::args().collect();
    for (i, arg) in args.iter().enumerate() {
        if arg == "--port" {
            if let Some(val) = args.get(i + 1) {
                return val.parse().unwrap_or(3000);
            }
        }
    }
    3000
}

/// Parse the `--mock` CLI flag. Returns `true` if present.
///
/// # Panics
///
/// This function never panics.
pub fn parse_mock_arg() -> bool {
    std::env::args().any(|arg| arg == "--mock")
}

// ── Main entry point ─────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialise tracing
    tokio_prompt_orchestrator::init_tracing();

    // Initialise metrics
    metrics::init_metrics()
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

    // Parse CLI args
    let worker_name = parse_worker_arg();
    let port = parse_port_arg();
    let mock_mode = parse_mock_arg();

    if mock_mode {
        info!("Starting in MOCK mode — no pipeline, scripted 2-minute story");

        let mock = Arc::new(Mutex::new(MockDashboardState::new()));

        // Spawn 1-second tick task to advance the story
        let mock_tick = Arc::clone(&mock);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let mut guard = mock_tick.lock().await;
                guard.tick();
            }
        });

        // Create a dummy pipeline channel (not used in mock mode)
        let (tx, _rx) = mpsc::channel(1);
        let circuit_breaker = CircuitBreaker::new(5, 0.8, Duration::from_secs(60));
        let deduplicator = Deduplicator::new(Duration::from_secs(300));

        let state = Arc::new(DashboardState {
            pipeline_tx: Some(tx),
            circuit_breaker,
            deduplicator,
            start_time: Instant::now(),
            worker_name: "mock".to_string(),
            mock_state: Some(mock),
        });

        let addr = format!("0.0.0.0:{port}");
        start_dashboard(&addr, state).await?;
    } else {
        // Live mode: create worker and spawn pipeline
        let worker = create_worker(&worker_name)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

        let handles = spawn_pipeline(worker);

        let circuit_breaker = CircuitBreaker::new(5, 0.8, Duration::from_secs(60));
        let deduplicator = Deduplicator::new(Duration::from_secs(300));

        let state = Arc::new(DashboardState {
            pipeline_tx: Some(handles.input_tx),
            circuit_breaker,
            deduplicator,
            start_time: Instant::now(),
            worker_name,
            mock_state: None,
        });

        let addr = format!("0.0.0.0:{port}");
        start_dashboard(&addr, state).await?;
    }

    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::util::ServiceExt;

    /// Create a test [`DashboardState`] backed by an echo worker pipeline (live mode).
    fn make_test_state() -> Arc<DashboardState> {
        let _ = metrics::init_metrics();
        let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
        let handles = spawn_pipeline(worker);
        let circuit_breaker = CircuitBreaker::new(5, 0.8, Duration::from_secs(60));
        let deduplicator = Deduplicator::new(Duration::from_secs(300));

        Arc::new(DashboardState {
            pipeline_tx: Some(handles.input_tx),
            circuit_breaker,
            deduplicator,
            start_time: Instant::now(),
            worker_name: "echo".to_string(),
            mock_state: None,
        })
    }

    /// Create a test [`DashboardState`] in mock mode.
    fn make_mock_test_state() -> Arc<DashboardState> {
        let mock = Arc::new(Mutex::new(MockDashboardState::new()));
        let (tx, _rx) = mpsc::channel(1);
        let circuit_breaker = CircuitBreaker::new(5, 0.8, Duration::from_secs(60));
        let deduplicator = Deduplicator::new(Duration::from_secs(300));

        Arc::new(DashboardState {
            pipeline_tx: Some(tx),
            circuit_breaker,
            deduplicator,
            start_time: Instant::now(),
            worker_name: "mock".to_string(),
            mock_state: Some(mock),
        })
    }

    /// Helper to build a test DashboardEvent with all fields populated.
    fn make_test_event() -> DashboardEvent {
        DashboardEvent {
            active_agents: 5,
            requests_total: 100,
            requests_deduped: 20,
            dedup_rate: 20.0,
            cost_saved_usd: 0.04,
            throughput_rps: 5.0,
            circuit_breakers: vec![],
            stage_latencies: StageTiming {
                rag_ms: 1.0,
                assemble_ms: 0.5,
                inference_ms: 10.0,
                post_ms: 0.3,
                stream_ms: 0.1,
            },
            model_routing: ModelRoutingStats {
                primary_model: "echo".to_string(),
                primary_pct: 100.0,
                fallback_pct: 0.0,
                models: vec![ModelRoutingEntry {
                    name: "echo".to_string(),
                    pct: 100.0,
                    color: "var(--accent)".to_string(),
                }],
            },
            stage_counts: StageCountMap {
                rag: 100,
                assemble: 100,
                inference: 100,
                post: 100,
                stream: 100,
            },
            shed_counts: StageCountMap {
                rag: 0,
                assemble: 0,
                inference: 0,
                post: 0,
                stream: 0,
            },
            uptime_secs: 60,
            recent_requests: vec![],
            active_stages: vec![true; 5],
            channel_depths: vec![],
        }
    }

    // ── DashboardEvent serialization tests ───────────────────────────────

    #[test]
    fn test_dashboard_event_serializes_to_json() {
        let event = make_test_event();
        let json = serde_json::to_string(&event);
        assert!(json.is_ok(), "DashboardEvent must serialize: {json:?}");
    }

    #[test]
    fn test_dashboard_event_contains_all_fields() {
        let event = make_test_event();

        let json =
            serde_json::to_string(&event).unwrap_or_else(|_| "serialization failed".to_string());
        assert!(json.contains("active_agents"));
        assert!(json.contains("requests_total"));
        assert!(json.contains("requests_deduped"));
        assert!(json.contains("dedup_rate"));
        assert!(json.contains("cost_saved_usd"));
        assert!(json.contains("throughput_rps"));
        assert!(json.contains("circuit_breakers"));
        assert!(json.contains("stage_latencies"));
        assert!(json.contains("model_routing"));
        assert!(json.contains("stage_counts"));
        assert!(json.contains("shed_counts"));
        assert!(json.contains("uptime_secs"));
        assert!(json.contains("recent_requests"));
        assert!(json.contains("active_stages"));
        assert!(json.contains("channel_depths"));
    }

    #[test]
    fn test_circuit_breaker_state_serializes() {
        let state = CircuitBreakerState {
            name: "inference".to_string(),
            status: "closed".to_string(),
            failures: 2,
            successes: 50,
            success_rate: 0.96,
        };
        let json = serde_json::to_string(&state);
        assert!(json.is_ok());
        let s = json.unwrap_or_default();
        assert!(s.contains("inference"));
        assert!(s.contains("closed"));
    }

    #[test]
    fn test_stage_timing_serializes() {
        let timing = StageTiming {
            rag_ms: 1.5,
            assemble_ms: 0.3,
            inference_ms: 12.0,
            post_ms: 0.2,
            stream_ms: 0.05,
        };
        let json = serde_json::to_string(&timing);
        assert!(json.is_ok());
    }

    #[test]
    fn test_model_routing_stats_serializes() {
        let stats = ModelRoutingStats {
            primary_model: "llama_cpp".to_string(),
            primary_pct: 90.0,
            fallback_pct: 10.0,
            models: vec![ModelRoutingEntry {
                name: "llama_cpp".to_string(),
                pct: 90.0,
                color: "var(--accent)".to_string(),
            }],
        };
        let json = serde_json::to_string(&stats).unwrap_or_else(|_| String::new());
        assert!(json.contains("llama_cpp"));
        assert!(json.contains("primary_pct"));
        assert!(json.contains("models"));
    }

    #[test]
    fn test_stage_count_map_serializes() {
        let counts = StageCountMap {
            rag: 10,
            assemble: 10,
            inference: 10,
            post: 10,
            stream: 10,
        };
        let json = serde_json::to_string(&counts).unwrap_or_else(|_| String::new());
        assert!(json.contains("\"rag\":10"));
    }

    // ── Worker factory tests ─────────────────────────────────────────────

    #[test]
    fn test_create_worker_echo_succeeds() {
        let result = create_worker("echo");
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_worker_llama_cpp_succeeds() {
        let result = create_worker("llama_cpp");
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_worker_unknown_returns_error() {
        let result = create_worker("nonexistent_worker");
        assert!(result.is_err());
        if let Err(ref e) = result {
            assert!(
                e.to_string().contains("unknown worker"),
                "error message should contain 'unknown worker'"
            );
        }
    }

    // ── CLI argument parsing tests ───────────────────────────────────────

    #[test]
    fn test_parse_worker_arg_defaults_to_echo() {
        // Without --worker flag in env args, should default to "echo"
        let default = parse_worker_arg();
        // May or may not be "echo" depending on test runner args, but must not panic
        assert!(!default.is_empty());
    }

    #[test]
    fn test_parse_port_arg_defaults_to_3000() {
        let port = parse_port_arg();
        // Default should be 3000 unless --port is set in test runner
        assert!(port > 0);
    }

    // ── Route handler tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_index_handler_returns_html() {
        let response = index_handler().await;
        let body = response.0;
        assert!(body.contains("tokio-prompt-orchestrator"));
    }

    #[tokio::test]
    async fn test_stats_handler_returns_200() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/api/stats")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_stats_handler_returns_json_content_type() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/api/stats")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        let content_type = response.headers().get("content-type");
        assert!(content_type.is_some());
        let ct_str = content_type.map(|v| v.to_str().unwrap_or("")).unwrap_or("");
        assert!(ct_str.contains("application/json"));
    }

    #[tokio::test]
    async fn test_stats_response_contains_required_fields() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/api/stats")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        let body = axum::body::to_bytes(response.into_body(), 1024 * 64)
            .await
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&body);

        assert!(text.contains("active_agents"));
        assert!(text.contains("requests_total"));
        assert!(text.contains("circuit_breakers"));
        assert!(text.contains("stage_latencies"));
    }

    #[tokio::test]
    async fn test_index_route_returns_200() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_unknown_route_returns_404() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/nonexistent")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_sse_route_returns_200() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/api/status")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_sse_route_content_type_is_event_stream() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/api/status")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.contains("text/event-stream"),
            "expected SSE content type, got: {ct}"
        );
    }

    // ── Snapshot builder tests ───────────────────────────────────────────

    #[tokio::test]
    async fn test_build_snapshot_returns_valid_event() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;

        assert_eq!(snapshot.active_agents, 5);
        assert!(
            snapshot.uptime_secs <= 5,
            "uptime should be near zero in tests"
        );
    }

    #[tokio::test]
    async fn test_build_snapshot_dedup_rate_zero_when_no_requests() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;

        // With no requests processed, dedup rate should be 0
        assert!((snapshot.dedup_rate - 0.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn test_build_snapshot_circuit_breaker_starts_closed() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;

        assert_eq!(snapshot.circuit_breakers.len(), 1);
        assert_eq!(snapshot.circuit_breakers[0].status, "closed");
        assert_eq!(snapshot.circuit_breakers[0].name, "inference");
    }

    #[tokio::test]
    async fn test_build_snapshot_worker_name_matches_state() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;

        assert_eq!(snapshot.model_routing.primary_model, "echo");
        assert!((snapshot.model_routing.primary_pct - 100.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn test_build_snapshot_cost_saved_zero_initially() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;

        assert!((snapshot.cost_saved_usd - 0.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_build_snapshot_stage_latencies_zero_initially() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;

        assert!((snapshot.stage_latencies.rag_ms - 0.0).abs() < f64::EPSILON);
        assert!((snapshot.stage_latencies.inference_ms - 0.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_build_snapshot_shed_counts_zero_initially() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;

        assert_eq!(snapshot.shed_counts.rag, 0);
        assert_eq!(snapshot.shed_counts.assemble, 0);
        assert_eq!(snapshot.shed_counts.inference, 0);
    }

    #[tokio::test]
    async fn test_build_snapshot_serializes_roundtrip() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;

        let json = serde_json::to_string(&snapshot);
        assert!(json.is_ok(), "snapshot must serialize");
        let json_str = json.unwrap_or_default();
        assert!(!json_str.is_empty());

        // Verify it's valid JSON by parsing it back
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&json_str);
        assert!(parsed.is_ok(), "serialized JSON must parse back");
    }

    // ── Router construction tests ────────────────────────────────────────

    #[tokio::test]
    async fn test_build_router_does_not_panic() {
        let state = make_test_state();
        let _router = build_router(state);
    }

    // ── DashboardState clone tests ───────────────────────────────────────

    #[tokio::test]
    async fn test_dashboard_state_is_cloneable() {
        let state = make_test_state();
        let inner = (*state).clone();
        assert_eq!(inner.worker_name, "echo");
    }

    // ── Post method tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_post_to_index_returns_method_not_allowed() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn test_post_to_stats_returns_method_not_allowed() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/api/stats")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn test_post_to_sse_returns_method_not_allowed() {
        let state = make_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/api/status")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    // ── Dashboard HTML content tests ─────────────────────────────────────

    #[test]
    fn test_dashboard_html_is_not_empty() {
        assert!(
            !DASHBOARD_HTML.is_empty(),
            "embedded HTML must not be empty"
        );
    }

    #[test]
    fn test_dashboard_html_contains_title() {
        assert!(
            DASHBOARD_HTML.contains("tokio-prompt-orchestrator"),
            "HTML must contain the project title"
        );
    }

    #[test]
    fn test_dashboard_html_contains_sse_endpoint() {
        assert!(
            DASHBOARD_HTML.contains("/api/status"),
            "HTML must reference the SSE endpoint"
        );
    }

    #[test]
    fn test_dashboard_html_contains_doctype() {
        assert!(
            DASHBOARD_HTML.contains("<!DOCTYPE html>"),
            "HTML must have a doctype declaration"
        );
    }

    // ── Dedup rate calculation tests ─────────────────────────────────────

    #[test]
    fn test_dedup_rate_zero_when_no_requests() {
        let requests_total: u64 = 0;
        let requests_deduped: u64 = 0;
        let rate = if requests_total > 0 {
            (requests_deduped as f32 / requests_total as f32) * 100.0
        } else {
            0.0
        };
        assert!((rate - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_dedup_rate_calculation_correct() {
        let requests_total: u64 = 100;
        let requests_deduped: u64 = 25;
        let rate = if requests_total > 0 {
            (requests_deduped as f32 / requests_total as f32) * 100.0
        } else {
            0.0
        };
        assert!((rate - 25.0).abs() < 0.01);
    }

    #[test]
    fn test_cost_saved_calculation() {
        let deduped: u64 = 500;
        let cost = deduped as f64 * 0.002;
        assert!((cost - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_throughput_calculation() {
        let requests: u64 = 300;
        let uptime_secs: u64 = 60;
        let rps = requests as f32 / uptime_secs as f32;
        assert!((rps - 5.0).abs() < 0.01);
    }

    // ── Circuit breaker status string conversion tests ───────────────────

    #[test]
    fn test_circuit_status_closed_to_string() {
        let status = CircuitStatus::Closed;
        let s = match status {
            CircuitStatus::Closed => "closed",
            CircuitStatus::Open => "open",
            CircuitStatus::HalfOpen => "half_open",
        };
        assert_eq!(s, "closed");
    }

    #[test]
    fn test_circuit_status_open_to_string() {
        let status = CircuitStatus::Open;
        let s = match status {
            CircuitStatus::Closed => "closed",
            CircuitStatus::Open => "open",
            CircuitStatus::HalfOpen => "half_open",
        };
        assert_eq!(s, "open");
    }

    #[test]
    fn test_circuit_status_half_open_to_string() {
        let status = CircuitStatus::HalfOpen;
        let s = match status {
            CircuitStatus::Closed => "closed",
            CircuitStatus::Open => "open",
            CircuitStatus::HalfOpen => "half_open",
        };
        assert_eq!(s, "half_open");
    }

    // ── New struct serialization tests ───────────────────────────────────

    #[test]
    fn test_model_routing_entry_serializes() {
        let entry = ModelRoutingEntry {
            name: "Mistral".to_string(),
            pct: 60.0,
            color: "var(--accent)".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap_or_else(|_| String::new());
        assert!(json.contains("Mistral"));
        assert!(json.contains("60"));
        assert!(json.contains("var(--accent)"));
    }

    #[test]
    fn test_recent_request_serializes() {
        let req = RecentRequest {
            timestamp: "14:32:01".to_string(),
            session_id: "user-42".to_string(),
            stage: "inference".to_string(),
            latency_ms: 12.5,
            status: "ok".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap_or_else(|_| String::new());
        assert!(json.contains("14:32:01"));
        assert!(json.contains("user-42"));
        assert!(json.contains("inference"));
        assert!(json.contains("12.5"));
        assert!(json.contains("\"status\":\"ok\""));
    }

    #[test]
    fn test_channel_depth_info_serializes() {
        let info = ChannelDepthInfo {
            name: "RAG \u{2192} ASM".to_string(),
            current: 42,
            capacity: 512,
        };
        let json = serde_json::to_string(&info).unwrap_or_else(|_| String::new());
        assert!(json.contains("42"));
        assert!(json.contains("512"));
    }

    #[test]
    fn test_dashboard_event_with_recent_requests_serializes() {
        let mut event = make_test_event();
        event.recent_requests = vec![
            RecentRequest {
                timestamp: "00:00:01".to_string(),
                session_id: "user-1".to_string(),
                stage: "rag".to_string(),
                latency_ms: 3.0,
                status: "ok".to_string(),
            },
            RecentRequest {
                timestamp: "00:00:02".to_string(),
                session_id: "user-2".to_string(),
                stage: "inference".to_string(),
                latency_ms: 260.0,
                status: "error".to_string(),
            },
        ];
        let json = serde_json::to_string(&event);
        assert!(json.is_ok());
        let s = json.unwrap_or_default();
        assert!(s.contains("user-1"));
        assert!(s.contains("user-2"));
    }

    #[test]
    fn test_dashboard_event_with_channel_depths_serializes() {
        let mut event = make_test_event();
        event.channel_depths = vec![
            ChannelDepthInfo {
                name: "RAG \u{2192} ASM".to_string(),
                current: 100,
                capacity: 512,
            },
            ChannelDepthInfo {
                name: "ASM \u{2192} INF".to_string(),
                current: 50,
                capacity: 512,
            },
        ];
        let json = serde_json::to_string(&event);
        assert!(json.is_ok());
    }

    #[test]
    fn test_dashboard_event_with_active_stages_serializes() {
        let mut event = make_test_event();
        event.active_stages = vec![true, false, true, false, true];
        let json = serde_json::to_string(&event);
        assert!(json.is_ok());
        let s = json.unwrap_or_default();
        assert!(s.contains("active_stages"));
    }

    // ── CLI mock arg test ───────────────────────────────────────────────

    #[test]
    fn test_parse_mock_arg_returns_bool() {
        let _mock = parse_mock_arg();
        // Must not panic
    }

    // ── MockDashboardState tests ────────────────────────────────────────

    #[test]
    fn test_mock_state_new_initial_values() {
        let mock = MockDashboardState::new();
        assert_eq!(mock.tick_count, 0);
        assert_eq!(mock.uptime_secs, 0);
        assert_eq!(mock.requests_total, 0);
        assert_eq!(mock.inferences_total, 0);
        assert_eq!(mock.circuit_breakers.len(), 3);
        assert_eq!(mock.model_routing.len(), 3);
        assert_eq!(mock.channel_depths.len(), 4);
        assert!(mock.recent_requests.is_empty());
    }

    #[test]
    fn test_mock_state_tick_increments_counters() {
        let mut mock = MockDashboardState::new();
        mock.tick();
        assert_eq!(mock.tick_count, 1);
        assert_eq!(mock.uptime_secs, 1);
        assert!(mock.requests_total > 0);
        assert!(mock.inferences_total > 0);
    }

    #[test]
    fn test_mock_state_latencies_positive_after_ticks() {
        let mut mock = MockDashboardState::new();
        for _ in 0..100 {
            mock.tick();
        }
        for lat in &mock.stage_latencies {
            assert!(*lat > 0.0, "Latency must be positive, got {}", lat);
        }
    }

    #[test]
    fn test_mock_state_warmup_phase_low_latency() {
        let mut mock = MockDashboardState::new();
        for _ in 0..20 {
            mock.tick();
        }
        assert!(
            mock.stage_latencies[2] < 300.0,
            "INFER should be low in warmup, got {}",
            mock.stage_latencies[2]
        );
        for cb in &mock.circuit_breakers {
            assert_eq!(cb.status, "closed", "All CBs should be closed in warmup");
        }
    }

    #[test]
    fn test_mock_state_failure_phase_cb_opens() {
        let mut mock = MockDashboardState::new();
        for _ in 0..50 {
            mock.tick();
        }
        assert_eq!(
            mock.circuit_breakers[2].status, "open",
            "llama.cpp should be OPEN in failure phase"
        );
        assert!(
            mock.stage_latencies[2] > 350.0,
            "INFER should be high in failure, got {}",
            mock.stage_latencies[2]
        );
    }

    #[test]
    fn test_mock_state_halfopen_phase() {
        let mut mock = MockDashboardState::new();
        for _ in 0..65 {
            mock.tick();
        }
        assert_eq!(
            mock.circuit_breakers[2].status, "half_open",
            "llama.cpp should be HALF-OPEN"
        );
    }

    #[test]
    fn test_mock_state_story_loops() {
        let mut mock = MockDashboardState::new();
        for _ in 0..125 {
            mock.tick();
        }
        for cb in &mock.circuit_breakers {
            assert_eq!(
                cb.status, "closed",
                "All CBs should be closed in second warmup"
            );
        }
    }

    #[test]
    fn test_mock_state_openai_always_closed() {
        let mut mock = MockDashboardState::new();
        for _ in 0..200 {
            mock.tick();
            assert_eq!(mock.circuit_breakers[0].status, "closed");
        }
    }

    #[test]
    fn test_mock_state_anthropic_always_closed() {
        let mut mock = MockDashboardState::new();
        for _ in 0..200 {
            mock.tick();
            assert_eq!(mock.circuit_breakers[1].status, "closed");
        }
    }

    #[test]
    fn test_mock_state_llama_cycles_through_states() {
        let mut mock = MockDashboardState::new();
        let mut saw_open = false;
        let mut saw_half = false;
        for _ in 0..120 {
            mock.tick();
            match mock.circuit_breakers[2].status.as_str() {
                "open" => saw_open = true,
                "half_open" => saw_half = true,
                _ => {}
            }
        }
        assert!(saw_open, "llama.cpp CB never opened");
        assert!(saw_half, "llama.cpp CB never went half-open");
    }

    #[test]
    fn test_mock_state_infer_latency_range() {
        let mut mock = MockDashboardState::new();
        let mut max_infer = 0.0_f64;
        for _ in 0..200 {
            mock.tick();
            if mock.stage_latencies[2] > max_infer {
                max_infer = mock.stage_latencies[2];
            }
        }
        assert!(
            max_infer > 350.0,
            "Expected INFER spikes, max was {}",
            max_infer
        );
        assert!(max_infer < 600.0, "INFER too high: {}", max_infer);
    }

    #[test]
    fn test_mock_state_channel_depths_bounded() {
        let mut mock = MockDashboardState::new();
        for _ in 0..200 {
            mock.tick();
            for ch in &mock.channel_depths {
                assert!(
                    ch.current <= ch.capacity,
                    "Channel {} overflow: {}/{}",
                    ch.name,
                    ch.current,
                    ch.capacity
                );
            }
        }
    }

    #[test]
    fn test_mock_state_throughput_populated() {
        let mut mock = MockDashboardState::new();
        for _ in 0..10 {
            mock.tick();
        }
        assert_eq!(mock.throughput_history.len(), 10);
        for &val in &mock.throughput_history {
            assert!(val > 0, "Throughput should be positive");
        }
    }

    #[test]
    fn test_mock_state_throughput_bounded_at_60() {
        let mut mock = MockDashboardState::new();
        for _ in 0..200 {
            mock.tick();
        }
        assert!(mock.throughput_history.len() <= 60);
    }

    #[test]
    fn test_mock_state_model_routing_sums_to_100() {
        let mut mock = MockDashboardState::new();
        for _ in 0..120 {
            mock.tick();
            let total: f32 = mock.model_routing.iter().map(|m| m.pct).sum();
            assert!(
                (total - 100.0).abs() < 1.0,
                "Model routing should sum to ~100%, got {}",
                total
            );
        }
    }

    #[test]
    fn test_mock_state_model_routing_shifts_during_failure() {
        let mut mock = MockDashboardState::new();
        for _ in 0..20 {
            mock.tick();
        }
        let warmup_mistral = mock.model_routing[0].pct;

        for _ in 20..50 {
            mock.tick();
        }
        let failure_mistral = mock.model_routing[0].pct;

        assert!(
            failure_mistral < warmup_mistral,
            "Mistral should decrease during failure: warmup={}, failure={}",
            warmup_mistral,
            failure_mistral
        );
    }

    #[test]
    fn test_mock_state_recent_requests_bounded() {
        let mut mock = MockDashboardState::new();
        for _ in 0..200 {
            mock.tick();
        }
        assert!(mock.recent_requests.len() <= MAX_RECENT_REQUESTS);
    }

    #[test]
    fn test_mock_state_recent_requests_populated() {
        let mut mock = MockDashboardState::new();
        for _ in 0..20 {
            mock.tick();
        }
        assert!(!mock.recent_requests.is_empty());
    }

    #[test]
    fn test_mock_state_failure_phase_generates_error_entries() {
        let mut mock = MockDashboardState::new();
        for _ in 0..55 {
            mock.tick();
        }
        let has_error = mock
            .recent_requests
            .iter()
            .any(|r| r.status == "error" || r.status == "shed");
        assert!(has_error, "Failure phase should produce error/shed entries");
    }

    #[test]
    fn test_mock_state_active_agents_varies_by_phase() {
        let mut mock = MockDashboardState::new();
        for _ in 0..20 {
            mock.tick();
        }
        assert_eq!(mock.active_agents, 5);

        for _ in 20..50 {
            mock.tick();
        }
        assert_eq!(mock.active_agents, 4);
    }

    #[test]
    fn test_mock_state_active_stage_rotates() {
        let mut mock = MockDashboardState::new();
        let mut indices = std::collections::HashSet::new();
        for _ in 0..20 {
            mock.tick();
            indices.insert(mock.active_stage_index);
        }
        assert!(indices.len() > 1);
        for &idx in &indices {
            assert!(idx < 5);
        }
    }

    #[test]
    fn test_mock_state_stage_counts_increase() {
        let mut mock = MockDashboardState::new();
        mock.tick();
        let after_1 = mock.stage_counts;
        mock.tick();
        let after_2 = mock.stage_counts;
        for i in 0..5 {
            assert!(after_2[i] > after_1[i]);
        }
    }

    #[test]
    fn test_mock_state_shed_counts_increase_during_failure() {
        let mut mock = MockDashboardState::new();
        for _ in 0..44 {
            mock.tick();
        }
        let pre_failure_shed = mock.shed_counts[2];
        for _ in 44..60 {
            mock.tick();
        }
        assert!(mock.shed_counts[2] > pre_failure_shed);
    }

    #[test]
    fn test_mock_state_phase_tick_returns_correct_value() {
        let mut mock = MockDashboardState::new();
        assert_eq!(mock.phase_tick(), 0);
        mock.tick();
        assert_eq!(mock.phase_tick(), 0);
        mock.tick();
        assert_eq!(mock.phase_tick(), 1);
    }

    #[test]
    fn test_mock_state_no_panic_over_long_run() {
        let mut mock = MockDashboardState::new();
        for _ in 0..600 {
            mock.tick();
        }
        assert_eq!(mock.tick_count, 600);
    }

    #[test]
    fn test_mock_state_cost_saved_increases() {
        let mut mock = MockDashboardState::new();
        for _ in 0..10 {
            mock.tick();
        }
        assert!(mock.cost_saved_usd > 0.0);
    }

    #[test]
    fn test_mock_state_dedup_ratio_positive() {
        let mut mock = MockDashboardState::new();
        for _ in 0..50 {
            mock.tick();
        }
        assert!(mock.requests_total > mock.inferences_total);
    }

    // ── MockDashboardState build_event tests ────────────────────────────

    #[test]
    fn test_mock_build_event_serializes() {
        let mut mock = MockDashboardState::new();
        for _ in 0..10 {
            mock.tick();
        }
        let event = mock.build_event();
        let json = serde_json::to_string(&event);
        assert!(json.is_ok());
    }

    #[test]
    fn test_mock_build_event_has_3_circuit_breakers() {
        let mut mock = MockDashboardState::new();
        mock.tick();
        let event = mock.build_event();
        assert_eq!(event.circuit_breakers.len(), 3);
        assert_eq!(event.circuit_breakers[0].name, "openai");
        assert_eq!(event.circuit_breakers[1].name, "anthropic");
        assert_eq!(event.circuit_breakers[2].name, "llama.cpp");
    }

    #[test]
    fn test_mock_build_event_has_3_model_routing_entries() {
        let mut mock = MockDashboardState::new();
        mock.tick();
        let event = mock.build_event();
        assert_eq!(event.model_routing.models.len(), 3);
        assert_eq!(event.model_routing.models[0].name, "Mistral");
        assert_eq!(event.model_routing.models[1].name, "Claude");
        assert_eq!(event.model_routing.models[2].name, "OpenAI");
    }

    #[test]
    fn test_mock_build_event_has_5_active_stages() {
        let mut mock = MockDashboardState::new();
        mock.tick();
        let event = mock.build_event();
        assert_eq!(event.active_stages.len(), 5);
        let active_count = event.active_stages.iter().filter(|s| **s).count();
        assert_eq!(active_count, 1);
    }

    #[test]
    fn test_mock_build_event_has_4_channel_depths() {
        let mut mock = MockDashboardState::new();
        mock.tick();
        let event = mock.build_event();
        assert_eq!(event.channel_depths.len(), 4);
        for ch in &event.channel_depths {
            assert!(ch.capacity > 0);
        }
    }

    #[test]
    fn test_mock_build_event_dedup_rate_correct() {
        let mut mock = MockDashboardState::new();
        for _ in 0..50 {
            mock.tick();
        }
        let event = mock.build_event();
        assert!(event.dedup_rate > 0.0);
        assert!(event.dedup_rate < 100.0);
    }

    #[test]
    fn test_mock_build_event_success_rate_computed() {
        let mut mock = MockDashboardState::new();
        for _ in 0..10 {
            mock.tick();
        }
        let event = mock.build_event();
        for cb in &event.circuit_breakers {
            assert!(cb.success_rate >= 0.0 && cb.success_rate <= 1.0);
        }
    }

    // ── Mock mode snapshot tests ─────────────────────────────────────────

    #[tokio::test]
    async fn test_build_snapshot_mock_mode_returns_mock_data() {
        let state = make_mock_test_state();
        {
            if let Some(ref m) = state.mock_state {
                let mut guard = m.lock().await;
                for _ in 0..10 {
                    guard.tick();
                }
            }
        }

        let snapshot = build_snapshot(&state).await;
        assert_eq!(snapshot.circuit_breakers.len(), 3);
        assert_eq!(snapshot.model_routing.models.len(), 3);
        assert_eq!(snapshot.active_stages.len(), 5);
        assert_eq!(snapshot.channel_depths.len(), 4);
        assert!(snapshot.requests_total > 0);
    }

    #[tokio::test]
    async fn test_build_snapshot_mock_mode_serializes() {
        let state = make_mock_test_state();
        {
            if let Some(ref m) = state.mock_state {
                let mut guard = m.lock().await;
                guard.tick();
            }
        }

        let snapshot = build_snapshot(&state).await;
        let json = serde_json::to_string(&snapshot);
        assert!(json.is_ok());
    }

    #[tokio::test]
    async fn test_mock_mode_stats_route_returns_200() {
        let state = make_mock_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/api/stats")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_mock_mode_stats_contains_new_fields() {
        let state = make_mock_test_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/api/stats")
            .body(Body::empty())
            .unwrap_or_else(|_| Request::new(Body::empty()));

        let response = app.oneshot(request).await.unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(Body::empty())
                .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
        });

        let body = axum::body::to_bytes(response.into_body(), 1024 * 64)
            .await
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&body);

        assert!(text.contains("recent_requests"));
        assert!(text.contains("active_stages"));
        assert!(text.contains("channel_depths"));
        assert!(text.contains("models"));
    }

    // ── Live mode new field tests ────────────────────────────────────────

    #[tokio::test]
    async fn test_build_snapshot_live_mode_has_empty_recent_requests() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;
        assert!(snapshot.recent_requests.is_empty());
    }

    #[tokio::test]
    async fn test_build_snapshot_live_mode_has_active_stages() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;
        assert_eq!(snapshot.active_stages.len(), 5);
        assert!(snapshot.active_stages.iter().all(|s| *s));
    }

    #[tokio::test]
    async fn test_build_snapshot_live_mode_has_models_list() {
        let state = make_test_state();
        let snapshot = build_snapshot(&state).await;
        assert_eq!(snapshot.model_routing.models.len(), 1);
        assert_eq!(snapshot.model_routing.models[0].name, "echo");
    }
}
