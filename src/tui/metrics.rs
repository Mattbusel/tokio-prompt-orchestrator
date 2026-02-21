//! # Module: TUI Metrics Ingestion
//!
//! ## Responsibility
//! Provides data sources for the TUI dashboard. Two modes:
//! - **Mock mode**: Generates a scripted 2-minute story that loops. Demonstrates
//!   warmup, load ramp, failure, half-open, recovery, and steady state.
//! - **Live mode**: Connects to the orchestrator's Prometheus endpoint and parses metrics.
//!
//! ## Guarantees
//! - Mock data stays within valid numeric ranges at all times
//! - No panics on network errors in live mode (graceful degradation)
//! - Story loop is deterministic and visually compelling

use super::app::{App, CircuitState, LogEntry, LogLevel};

/// Cost per inference call in USD (used for savings estimate).
const COST_PER_INFERENCE: f64 = 0.012;

/// Story cycle length in seconds (ticks). The mock story loops every 120s.
const STORY_CYCLE_SECS: u64 = 120;

/// Mock data generator that tells a 2-minute story, then loops.
///
/// ## Story Phases
/// | Phase     | Ticks   | What happens                                      |
/// |-----------|---------|---------------------------------------------------|
/// | Warmup    |  0 – 29 | Low traffic, all circuits closed, low latencies    |
/// | Load      | 30 – 44 | Channels fill, throughput rises                    |
/// | Failure   | 45 – 59 | llama.cpp opens, INFER spikes, error logs          |
/// | HalfOpen  | 60 – 74 | Probe requests, latencies improving                |
/// | Recovery  | 75 – 89 | Circuit closes, recovery logs                      |
/// | Steady    | 90 –119 | Nominal metrics, dedup climbing, smooth operation  |
#[derive(Debug)]
pub struct MockMetrics {
    /// Base throughput for organic load pattern.
    base_throughput: f64,
}

impl Default for MockMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl MockMetrics {
    /// Creates a new mock metrics generator.
    pub fn new() -> Self {
        Self {
            base_throughput: 15.0,
        }
    }

    /// Updates the app state with one tick of mock data.
    ///
    /// # Arguments
    /// * `app` - Mutable reference to the app state to update.
    pub fn tick(&self, app: &mut App) {
        app.tick_count += 1;
        app.uptime_secs += 1;

        let t = app.tick_count as f64;
        let phase_tick = (app.tick_count - 1) % STORY_CYCLE_SECS;

        self.update_latencies(app, t, phase_tick);
        self.update_channels(app, t, phase_tick);
        self.update_circuit_breakers(app, phase_tick);
        self.update_dedup(app, phase_tick);
        self.update_throughput(app, t, phase_tick);
        self.update_health(app, t, phase_tick);
        self.update_logs(app, phase_tick);
        self.update_active_stage(app, t);
    }

    /// Updates pipeline stage latencies based on story phase.
    fn update_latencies(&self, app: &mut App, t: f64, phase_tick: u64) {
        let wobble = |freq: f64| (t * freq).sin();

        match phase_tick {
            // Warmup: low, stable latencies
            0..=29 => {
                app.stage_latencies[0] = 3.0 + 0.5 * wobble(0.07);
                app.stage_latencies[1] = 1.5 + 0.3 * wobble(0.11);
                app.stage_latencies[2] = 260.0 + 15.0 * wobble(0.05);
                app.stage_latencies[3] = 1.8 + 0.2 * wobble(0.09);
                app.stage_latencies[4] = 0.7 + 0.1 * wobble(0.13);
            }
            // Load ramp: latencies rise
            30..=44 => {
                let pressure = (phase_tick - 30) as f64 / 15.0;
                app.stage_latencies[0] = 4.0 + 2.0 * pressure + 0.5 * wobble(0.07);
                app.stage_latencies[1] = 2.0 + 1.0 * pressure + 0.3 * wobble(0.11);
                app.stage_latencies[2] = 280.0 + 80.0 * pressure + 20.0 * wobble(0.05);
                app.stage_latencies[3] = 2.5 + 1.5 * pressure + 0.3 * wobble(0.09);
                app.stage_latencies[4] = 1.0 + 0.5 * pressure + 0.1 * wobble(0.13);
            }
            // Failure: INFER spikes hard
            45..=59 => {
                let severity = ((phase_tick - 45) as f64 / 7.0).sin().abs();
                app.stage_latencies[0] = 5.5 + 1.0 * wobble(0.07);
                app.stage_latencies[1] = 3.0 + 0.5 * wobble(0.11);
                app.stage_latencies[2] = 380.0 + 100.0 * severity + 30.0 * wobble(0.05);
                app.stage_latencies[3] = 4.0 + 1.0 * wobble(0.09);
                app.stage_latencies[4] = 1.5 + 0.3 * wobble(0.13);
            }
            // HalfOpen: latencies slowly improving
            60..=74 => {
                let recovery = (phase_tick - 60) as f64 / 15.0;
                app.stage_latencies[0] = 5.0 - 1.5 * recovery + 0.5 * wobble(0.07);
                app.stage_latencies[1] = 2.5 - 0.5 * recovery + 0.3 * wobble(0.11);
                app.stage_latencies[2] = 350.0 - 60.0 * recovery + 20.0 * wobble(0.05);
                app.stage_latencies[3] = 3.5 - 1.0 * recovery + 0.3 * wobble(0.09);
                app.stage_latencies[4] = 1.3 - 0.3 * recovery + 0.1 * wobble(0.13);
            }
            // Recovery: approaching nominal
            75..=89 => {
                let settle = (phase_tick - 75) as f64 / 15.0;
                app.stage_latencies[0] = 3.5 - 0.5 * settle + 0.5 * wobble(0.07);
                app.stage_latencies[1] = 2.0 - 0.3 * settle + 0.3 * wobble(0.11);
                app.stage_latencies[2] = 290.0 - 20.0 * settle + 15.0 * wobble(0.05);
                app.stage_latencies[3] = 2.5 - 0.5 * settle + 0.2 * wobble(0.09);
                app.stage_latencies[4] = 1.0 - 0.2 * settle + 0.1 * wobble(0.13);
            }
            // Steady state: nominal
            _ => {
                app.stage_latencies[0] = 3.0 + 0.8 * wobble(0.07) + 0.3 * wobble(0.31);
                app.stage_latencies[1] = 1.7 + 0.4 * wobble(0.11) + 0.2 * wobble(0.47);
                app.stage_latencies[2] = 270.0 + 25.0 * wobble(0.05) + 10.0 * wobble(0.23);
                app.stage_latencies[3] = 2.0 + 0.5 * wobble(0.09) + 0.2 * wobble(0.19);
                app.stage_latencies[4] = 0.8 + 0.2 * wobble(0.13) + 0.1 * wobble(0.37);
            }
        }

        // Clamp all to non-negative
        for lat in &mut app.stage_latencies {
            if *lat < 0.1 {
                *lat = 0.1;
            }
        }
    }

    /// Updates channel depths based on story phase.
    fn update_channels(&self, app: &mut App, t: f64, phase_tick: u64) {
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

        let set_depth = |ch: &mut super::app::ChannelDepth, base_fill: f64, wobble_val: f64| {
            let cap = ch.capacity as f64;
            ch.current =
                ((cap * base_fill + cap * 0.05 * wobble_val).max(0.0) as usize).min(ch.capacity);
        };

        set_depth(&mut app.channel_depths[0], fill0, wobble(0.08));
        set_depth(&mut app.channel_depths[1], fill1, wobble(0.12));
        set_depth(&mut app.channel_depths[2], fill2, wobble(0.06));
        set_depth(&mut app.channel_depths[3], fill3, wobble(0.15));
    }

    /// Updates circuit breakers based on story phase.
    fn update_circuit_breakers(&self, app: &mut App, phase_tick: u64) {
        let tick = app.tick_count;

        // openai: always closed
        if let Some(cb) = app.circuit_breakers.get_mut(0) {
            cb.state = CircuitState::Closed;
            let ok_count = tick * 2 + 47;
            cb.detail = format!("({} ok)", ok_count);
        }

        // anthropic: always closed in story mode
        if let Some(cb) = app.circuit_breakers.get_mut(1) {
            cb.state = CircuitState::Closed;
            let ok_count = tick * 3 + 120;
            cb.detail = format!("({} ok)", ok_count);
        }

        // llama.cpp: the star of the story
        if let Some(cb) = app.circuit_breakers.get_mut(2) {
            match phase_tick {
                0..=44 => {
                    cb.state = CircuitState::Closed;
                    let ok_count = tick + 50;
                    cb.detail = format!("({} ok)", ok_count);
                }
                45..=59 => {
                    cb.state = CircuitState::Open;
                    let ago = phase_tick - 45;
                    cb.detail = format!("(opened {}s ago)", ago);
                }
                60..=74 => {
                    cb.state = CircuitState::HalfOpen;
                    let probe_in = 75 - phase_tick;
                    cb.detail = format!("(probe in {}s)", probe_in);
                }
                _ => {
                    cb.state = CircuitState::Closed;
                    if phase_tick < 80 {
                        cb.detail = "(recovering)".into();
                    } else {
                        let ok_count = tick + 50;
                        cb.detail = format!("({} ok)", ok_count);
                    }
                }
            }
        }
    }

    /// Updates dedup statistics based on story phase.
    fn update_dedup(&self, app: &mut App, phase_tick: u64) {
        let (req_inc, inf_inc) = match phase_tick {
            0..=29 => (3, 1),  // Warmup: low traffic, moderate dedup
            30..=44 => (8, 2), // Load: high traffic, good dedup
            45..=59 => (6, 3), // Failure: some retries inflate requests
            60..=74 => (5, 2), // HalfOpen: moderate
            75..=89 => (5, 1), // Recovery: good dedup
            _ => (4, 1),       // Steady: excellent dedup
        };

        app.requests_total += req_inc;
        app.inferences_total += inf_inc;
        app.cost_saved_usd += COST_PER_INFERENCE * (req_inc - inf_inc) as f64;
    }

    /// Generates organic throughput pattern based on story phase.
    fn update_throughput(&self, app: &mut App, t: f64, phase_tick: u64) {
        let base = match phase_tick {
            0..=29 => 8.0,
            30..=44 => 8.0 + 20.0 * ((phase_tick - 30) as f64 / 15.0),
            45..=59 => 22.0,
            60..=74 => 18.0,
            75..=89 => 15.0,
            _ => self.base_throughput,
        };

        let wobble = 3.0 * (t * 0.11).sin() + 2.0 * (t * 0.19).cos();
        let value = (base + wobble).max(1.0) as u64;
        app.push_throughput(value);
    }

    /// Updates system health metrics based on story phase.
    fn update_health(&self, app: &mut App, t: f64, phase_tick: u64) {
        let (cpu_base, mem_base) = match phase_tick {
            0..=29 => (45.0, 35.0),
            30..=44 => (60.0, 42.0),
            45..=59 => (78.0, 55.0),
            60..=74 => (65.0, 48.0),
            75..=89 => (55.0, 40.0),
            _ => (50.0, 38.0),
        };

        app.cpu_percent =
            (cpu_base + 5.0 * (t * 0.05).sin() + 2.0 * (t * 0.17).cos()).clamp(0.0, 100.0);
        app.mem_percent =
            (mem_base + 3.0 * (t * 0.03).sin() + 1.0 * (t * 0.11).cos()).clamp(0.0, 100.0);
        app.active_tasks =
            ((180.0 + 40.0 * (t * 0.06).sin() + 15.0 * (t * 0.15).cos()) as usize).max(1);
    }

    /// Generates log entries appropriate to the current story phase.
    fn update_logs(&self, app: &mut App, phase_tick: u64) {
        let ts = format!(
            "{:02}:{:02}:{:02}",
            (app.uptime_secs / 3600) % 24,
            (app.uptime_secs % 3600) / 60,
            app.uptime_secs % 60
        );

        let tick = app.tick_count;

        match phase_tick {
            // Warmup: occasional info
            0..=29 => {
                if tick % 5 == 0 {
                    app.push_log(LogEntry {
                        timestamp: ts,
                        level: LogLevel::Info,
                        message: "Request deduped".to_string(),
                        fields: format!(
                            "key={:06x} session=user-{}",
                            (tick * 7) & 0xFFFFFF,
                            tick % 100
                        ),
                    });
                }
            }
            // Load ramp: more frequent info + occasional warn
            30..=44 => {
                if tick % 3 == 0 {
                    app.push_log(LogEntry {
                        timestamp: ts.clone(),
                        level: LogLevel::Info,
                        message: "Batch complete".to_string(),
                        fields: format!(
                            "prompts={} cost=${:.2}",
                            800 + tick % 100,
                            1.5 + (tick as f64 * 0.01)
                        ),
                    });
                }
                if phase_tick == 40 {
                    app.push_log(LogEntry {
                        timestamp: ts,
                        level: LogLevel::Warn,
                        message: "Channel pressure rising".to_string(),
                        fields: "queue=INF→PST depth=820/1024".to_string(),
                    });
                }
            }
            // Failure: errors + warns
            45..=59 => {
                if tick % 3 == 0 {
                    app.push_log(LogEntry {
                        timestamp: ts.clone(),
                        level: LogLevel::Error,
                        message: "Worker timeout".to_string(),
                        fields: format!("worker=llama.cpp attempt={}/3", 1 + (tick % 3)),
                    });
                }
                if phase_tick == 45 {
                    app.push_log(LogEntry {
                        timestamp: ts.clone(),
                        level: LogLevel::Warn,
                        message: "Circuit OPEN".to_string(),
                        fields: "worker=llama.cpp failures=5 threshold=5".to_string(),
                    });
                }
                if tick % 5 == 0 {
                    app.push_log(LogEntry {
                        timestamp: ts,
                        level: LogLevel::Warn,
                        message: "Stage latency spike".to_string(),
                        fields: format!(
                            "stage=INFER p99={:.0}ms threshold=400ms",
                            app.stage_latencies[2]
                        ),
                    });
                }
            }
            // HalfOpen: probe info
            60..=74 => {
                if phase_tick == 60 {
                    app.push_log(LogEntry {
                        timestamp: ts.clone(),
                        level: LogLevel::Info,
                        message: "Circuit HALF-OPEN".to_string(),
                        fields: "worker=llama.cpp probing=true".to_string(),
                    });
                }
                if tick % 5 == 0 {
                    app.push_log(LogEntry {
                        timestamp: ts,
                        level: LogLevel::Debug,
                        message: "Probe request sent".to_string(),
                        fields: format!("worker=llama.cpp latency={:.0}ms", app.stage_latencies[2]),
                    });
                }
            }
            // Recovery: success logs
            75..=89 => {
                if phase_tick == 75 {
                    app.push_log(LogEntry {
                        timestamp: ts.clone(),
                        level: LogLevel::Info,
                        message: "Circuit CLOSED".to_string(),
                        fields: "worker=llama.cpp recovered=true".to_string(),
                    });
                }
                if tick % 4 == 0 {
                    app.push_log(LogEntry {
                        timestamp: ts,
                        level: LogLevel::Info,
                        message: "Request deduped".to_string(),
                        fields: format!(
                            "key={:06x} savings=${:.3}",
                            (tick * 13) & 0xFFFFFF,
                            COST_PER_INFERENCE
                        ),
                    });
                }
            }
            // Steady: nominal info
            _ => {
                if tick % 4 == 0 {
                    let messages = [
                        (
                            "Request deduped",
                            format!(
                                "key={:06x} session=user-{}",
                                (tick * 7) & 0xFFFFFF,
                                tick % 500
                            ),
                        ),
                        (
                            "Batch complete",
                            format!(
                                "prompts={} duration={:.1}s",
                                800 + tick % 100,
                                10.0 + (tick as f64 * 0.3).sin() * 3.0
                            ),
                        ),
                        (
                            "Stage latency nominal",
                            format!("stage=RAG p99={:.1}ms", app.stage_latencies[0]),
                        ),
                    ];
                    let idx = (tick / 4) as usize % messages.len();
                    app.push_log(LogEntry {
                        timestamp: ts,
                        level: LogLevel::Info,
                        message: messages[idx].0.to_string(),
                        fields: messages[idx].1.clone(),
                    });
                }
            }
        }
    }

    /// Updates the active pipeline stage indicator.
    fn update_active_stage(&self, app: &mut App, t: f64) {
        let stage_idx = ((t * 0.5) as usize) % 5;
        app.active_stage = Some(stage_idx);
    }
}

/// Live metrics source that reads from a Prometheus endpoint.
///
/// Connects to the orchestrator's metrics server and parses Prometheus
/// text exposition format into app state.
#[derive(Debug)]
pub struct LiveMetrics {
    /// Base URL for the Prometheus metrics endpoint.
    metrics_url: String,
    /// Previous shed total for delta calculation.
    prev_shed_total: u64,
    /// Previous errors total for delta calculation.
    prev_errors_total: u64,
}

impl LiveMetrics {
    /// Creates a new live metrics source.
    ///
    /// # Arguments
    /// * `metrics_url` - Full URL to the Prometheus metrics endpoint.
    pub fn new(metrics_url: String) -> Self {
        Self {
            metrics_url,
            prev_shed_total: 0,
            prev_errors_total: 0,
        }
    }

    /// Fetches metrics from the Prometheus endpoint and updates app state.
    ///
    /// # Arguments
    /// * `app` - Mutable reference to the app state to update.
    ///
    /// # Returns
    /// `Ok(())` on success, or an error string on failure.
    pub async fn tick(&mut self, app: &mut App) -> Result<(), String> {
        let body = reqwest::get(&self.metrics_url)
            .await
            .map_err(|e| format!("HTTP error: {}", e))?
            .text()
            .await
            .map_err(|e| format!("Body read error: {}", e))?;

        self.parse_metrics(&body, app);
        app.tick_count += 1;
        app.uptime_secs += 1;
        Ok(())
    }

    /// Parses Prometheus text format into app state.
    pub fn parse_metrics(&mut self, body: &str, app: &mut App) {
        let ts = format!(
            "{:02}:{:02}:{:02}",
            (app.uptime_secs / 3600) % 24,
            (app.uptime_secs % 3600) / 60,
            app.uptime_secs % 60
        );

        for line in body.lines() {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }

            // Parse stage durations
            if let Some(rest) = line.strip_prefix("orchestrator_stage_duration_seconds{stage=\"") {
                self.parse_stage_duration(rest, app);
            }

            // Parse queue depths
            if let Some(rest) = line.strip_prefix("orchestrator_queue_depth{queue=\"") {
                self.parse_queue_depth(rest, app);
            }

            // Parse request totals
            if let Some(rest) = line.strip_prefix("orchestrator_requests_total ") {
                if let Ok(val) = rest.trim().parse::<f64>() {
                    app.requests_total = val as u64;
                }
            }

            // Parse inference totals
            if let Some(rest) = line.strip_prefix("orchestrator_inferences_total ") {
                if let Ok(val) = rest.trim().parse::<f64>() {
                    app.inferences_total = val as u64;
                }
            }

            // Parse shed total (delta-based log)
            if let Some(rest) = line.strip_prefix("orchestrator_requests_shed_total ") {
                if let Ok(val) = rest.trim().parse::<f64>() {
                    let current = val as u64;
                    if current > self.prev_shed_total {
                        let delta = current - self.prev_shed_total;
                        app.push_log(LogEntry {
                            timestamp: ts.clone(),
                            level: LogLevel::Warn,
                            message: "Requests shed".to_string(),
                            fields: format!("count={} total={}", delta, current),
                        });
                    }
                    self.prev_shed_total = current;
                }
            }

            // Parse errors total (delta-based log)
            if let Some(rest) = line.strip_prefix("orchestrator_errors_total ") {
                if let Ok(val) = rest.trim().parse::<f64>() {
                    let current = val as u64;
                    if current > self.prev_errors_total {
                        let delta = current - self.prev_errors_total;
                        app.push_log(LogEntry {
                            timestamp: ts.clone(),
                            level: LogLevel::Error,
                            message: "Errors detected".to_string(),
                            fields: format!("count={} total={}", delta, current),
                        });
                    }
                    self.prev_errors_total = current;
                }
            }
        }
    }

    /// Parses a stage duration metric line.
    fn parse_stage_duration(&self, rest: &str, app: &mut App) {
        let stage_map = [
            ("rag\"}", 0),
            ("assemble\"}", 1),
            ("inference\"}", 2),
            ("post_process\"}", 3),
            ("stream\"}", 4),
        ];
        for (suffix, idx) in &stage_map {
            if let Some(val_str) = rest.strip_prefix(suffix) {
                if let Ok(secs) = val_str.trim().parse::<f64>() {
                    app.stage_latencies[*idx] = secs * 1000.0; // convert to ms
                }
            }
        }
    }

    /// Parses a queue depth metric line.
    fn parse_queue_depth(&self, rest: &str, app: &mut App) {
        let queue_map = [
            ("rag_to_asm\"}", 0),
            ("asm_to_inf\"}", 1),
            ("inf_to_pst\"}", 2),
            ("pst_to_str\"}", 3),
        ];
        for (suffix, idx) in &queue_map {
            if let Some(val_str) = rest.strip_prefix(suffix) {
                if let Ok(depth) = val_str.trim().parse::<f64>() {
                    app.channel_depths[*idx].current = depth as usize;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_mock_metrics_tick_increments_counters() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));
        mock.tick(&mut app);

        assert_eq!(app.tick_count, 1);
        assert_eq!(app.uptime_secs, 1);
        assert!(app.requests_total > 0);
        assert!(app.inferences_total > 0);
    }

    #[test]
    fn test_mock_metrics_latencies_positive() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));
        for _ in 0..100 {
            mock.tick(&mut app);
        }
        for lat in &app.stage_latencies {
            assert!(*lat > 0.0, "Latency must be positive, got {}", lat);
        }
    }

    #[test]
    fn test_mock_metrics_story_phase_warmup() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));
        // Tick into warmup phase (ticks 1-30, phase_tick 0-29)
        for _ in 0..20 {
            mock.tick(&mut app);
        }
        // INFER should be relatively low in warmup
        assert!(
            app.stage_latencies[2] < 300.0,
            "INFER should be low in warmup, got {}",
            app.stage_latencies[2]
        );
        // All circuits should be closed
        for cb in &app.circuit_breakers {
            assert_eq!(
                cb.state,
                CircuitState::Closed,
                "All CBs should be closed in warmup"
            );
        }
    }

    #[test]
    fn test_mock_metrics_story_phase_failure() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));
        // Tick to failure phase (ticks 46-60, phase_tick 45-59)
        for _ in 0..50 {
            mock.tick(&mut app);
        }
        // llama.cpp should be OPEN
        assert_eq!(
            app.circuit_breakers[2].state,
            CircuitState::Open,
            "llama.cpp should be OPEN in failure phase"
        );
        // INFER should be high
        assert!(
            app.stage_latencies[2] > 350.0,
            "INFER should be high in failure, got {}",
            app.stage_latencies[2]
        );
    }

    #[test]
    fn test_mock_metrics_story_phase_halfopen() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));
        // Tick to half-open phase (ticks 61-75, phase_tick 60-74)
        for _ in 0..65 {
            mock.tick(&mut app);
        }
        // llama.cpp should be HALF-OPEN
        assert_eq!(
            app.circuit_breakers[2].state,
            CircuitState::HalfOpen,
            "llama.cpp should be HALF-OPEN"
        );
    }

    #[test]
    fn test_mock_metrics_story_loops() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));
        // Tick through one full cycle + into second warmup
        for _ in 0..125 {
            mock.tick(&mut app);
        }
        // Should be back in warmup phase (phase_tick = 124 % 120 = 4)
        // All circuits closed
        for cb in &app.circuit_breakers {
            assert_eq!(
                cb.state,
                CircuitState::Closed,
                "All CBs should be closed in second warmup"
            );
        }
    }

    #[test]
    fn test_mock_metrics_infer_latency_range() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));
        let mut max_infer = 0.0_f64;
        for _ in 0..200 {
            mock.tick(&mut app);
            if app.stage_latencies[2] > max_infer {
                max_infer = app.stage_latencies[2];
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
    fn test_mock_metrics_channel_depths_bounded() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));
        for _ in 0..200 {
            mock.tick(&mut app);
            for ch in &app.channel_depths {
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
    fn test_mock_metrics_dedup_ratio_positive() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));
        for _ in 0..100 {
            mock.tick(&mut app);
        }
        let savings = app.dedup_savings_percent();
        assert!(savings > 30.0, "Expected >30% savings, got {:.1}%", savings);
    }

    #[test]
    fn test_mock_metrics_throughput_populated() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));
        for _ in 0..10 {
            mock.tick(&mut app);
        }
        assert_eq!(app.throughput_history.len(), 10);
        for &val in &app.throughput_history {
            assert!(val > 0, "Throughput should be positive");
        }
    }

    #[test]
    fn test_mock_metrics_health_ranges() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));
        for _ in 0..200 {
            mock.tick(&mut app);
            assert!(
                app.cpu_percent >= 0.0 && app.cpu_percent <= 100.0,
                "CPU out of range: {}",
                app.cpu_percent
            );
            assert!(
                app.mem_percent >= 0.0 && app.mem_percent <= 100.0,
                "MEM out of range: {}",
                app.mem_percent
            );
            assert!(app.active_tasks > 0, "Active tasks should be > 0");
        }
    }

    #[test]
    fn test_mock_metrics_log_entries_generated() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));
        for _ in 0..20 {
            mock.tick(&mut app);
        }
        assert!(
            !app.log_entries.is_empty(),
            "Should have generated log entries"
        );
    }

    #[test]
    fn test_mock_metrics_active_stage_set() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));
        mock.tick(&mut app);
        assert!(app.active_stage.is_some());
        let idx = app.active_stage.map(|i| i < 5);
        assert_eq!(idx, Some(true));
    }

    #[test]
    fn test_live_metrics_parse_stage_duration() {
        let mut live = LiveMetrics::new("http://localhost:9090/metrics".into());
        let mut app = App::new(Duration::from_secs(1));

        let body = r#"# HELP orchestrator_stage_duration_seconds Stage duration
orchestrator_stage_duration_seconds{stage="rag"} 0.0042
orchestrator_stage_duration_seconds{stage="inference"} 0.287
"#;
        live.parse_metrics(body, &mut app);

        assert!((app.stage_latencies[0] - 4.2).abs() < 0.1);
        assert!((app.stage_latencies[2] - 287.0).abs() < 0.1);
    }

    #[test]
    fn test_live_metrics_parse_queue_depth() {
        let mut live = LiveMetrics::new("http://localhost:9090/metrics".into());
        let mut app = App::new(Duration::from_secs(1));

        let body = "orchestrator_queue_depth{queue=\"rag_to_asm\"} 412\n\
                     orchestrator_queue_depth{queue=\"inf_to_pst\"} 891\n";
        live.parse_metrics(body, &mut app);

        assert_eq!(app.channel_depths[0].current, 412);
        assert_eq!(app.channel_depths[2].current, 891);
    }

    #[test]
    fn test_live_metrics_parse_request_totals() {
        let mut live = LiveMetrics::new("http://localhost:9090/metrics".into());
        let mut app = App::new(Duration::from_secs(1));

        let body = "orchestrator_requests_total 1847\norchestrator_inferences_total 312\n";
        live.parse_metrics(body, &mut app);

        assert_eq!(app.requests_total, 1847);
        assert_eq!(app.inferences_total, 312);
    }

    #[test]
    fn test_live_metrics_parse_shed_total() {
        let mut live = LiveMetrics::new("http://localhost:9090/metrics".into());
        let mut app = App::new(Duration::from_secs(1));

        // First parse: sets baseline
        live.parse_metrics("orchestrator_requests_shed_total 10\n", &mut app);
        assert!(app.log_entries.iter().any(|e| e.message == "Requests shed"));

        // Second parse with same value: no new log
        let log_count = app.log_entries.len();
        live.parse_metrics("orchestrator_requests_shed_total 10\n", &mut app);
        assert_eq!(app.log_entries.len(), log_count);

        // Third parse with increased value: new log
        live.parse_metrics("orchestrator_requests_shed_total 15\n", &mut app);
        let last = app.log_entries.back().expect("should have log");
        assert_eq!(last.message, "Requests shed");
        assert!(last.fields.contains("count=5"));
    }

    #[test]
    fn test_live_metrics_parse_errors_total() {
        let mut live = LiveMetrics::new("http://localhost:9090/metrics".into());
        let mut app = App::new(Duration::from_secs(1));

        live.parse_metrics("orchestrator_errors_total 3\n", &mut app);
        assert!(app
            .log_entries
            .iter()
            .any(|e| e.message == "Errors detected"));

        let log_count = app.log_entries.len();
        live.parse_metrics("orchestrator_errors_total 3\n", &mut app);
        assert_eq!(app.log_entries.len(), log_count);

        live.parse_metrics("orchestrator_errors_total 7\n", &mut app);
        let last = app.log_entries.back().expect("should have log");
        assert_eq!(last.message, "Errors detected");
        assert!(last.fields.contains("count=4"));
    }

    #[test]
    fn test_live_metrics_parse_empty_body() {
        let mut live = LiveMetrics::new("http://localhost:9090/metrics".into());
        let mut app = App::new(Duration::from_secs(1));
        live.parse_metrics("", &mut app);
        assert_eq!(app.requests_total, 0);
    }

    #[test]
    fn test_live_metrics_parse_comments_only() {
        let mut live = LiveMetrics::new("http://localhost:9090/metrics".into());
        let mut app = App::new(Duration::from_secs(1));
        live.parse_metrics("# HELP something\n# TYPE something gauge\n", &mut app);
        assert_eq!(app.requests_total, 0);
    }

    #[test]
    fn test_live_metrics_parse_malformed_value() {
        let mut live = LiveMetrics::new("http://localhost:9090/metrics".into());
        let mut app = App::new(Duration::from_secs(1));
        live.parse_metrics("orchestrator_requests_total not_a_number\n", &mut app);
        assert_eq!(app.requests_total, 0);
    }

    #[test]
    fn test_mock_metrics_default() {
        let mock = MockMetrics::default();
        let mut app = App::new(Duration::from_secs(1));
        mock.tick(&mut app);
        assert_eq!(app.tick_count, 1);
    }

    #[test]
    fn test_mock_metrics_no_panic_over_long_run() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));
        for _ in 0..600 {
            mock.tick(&mut app);
        }
        assert_eq!(app.tick_count, 600);
        assert!(app.throughput_history.len() <= 60);
    }

    #[test]
    fn test_mock_metrics_openai_always_closed() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));
        for _ in 0..200 {
            mock.tick(&mut app);
            assert_eq!(
                app.circuit_breakers[0].state,
                CircuitState::Closed,
                "openai should always be Closed"
            );
        }
    }

    #[test]
    fn test_mock_metrics_anthropic_always_closed() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));
        for _ in 0..200 {
            mock.tick(&mut app);
            assert_eq!(
                app.circuit_breakers[1].state,
                CircuitState::Closed,
                "anthropic should always be Closed in story mode"
            );
        }
    }

    #[test]
    fn test_mock_metrics_llama_cycles() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));

        let mut saw_open = false;
        let mut saw_half = false;
        for _ in 0..120 {
            mock.tick(&mut app);
            match app.circuit_breakers[2].state {
                CircuitState::Open => saw_open = true,
                CircuitState::HalfOpen => saw_half = true,
                CircuitState::Closed => {}
            }
        }
        assert!(saw_open, "llama.cpp CB never opened");
        assert!(saw_half, "llama.cpp CB never went half-open");
    }
}
