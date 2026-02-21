//! # Module: TUI Metrics Ingestion
//!
//! ## Responsibility
//! Provides data sources for the TUI dashboard. Two modes:
//! - **Mock mode**: Generates realistic synthetic data that tells a compelling story.
//!   Circuit breakers cycle, throughput follows organic patterns, latencies spike naturally.
//! - **Live mode**: Connects to the orchestrator's Prometheus endpoint and parses metrics.
//!
//! ## Guarantees
//! - Mock data stays within valid numeric ranges at all times
//! - No panics on network errors in live mode (graceful degradation)
//! - Circuit breaker cycling is deterministic and visually interesting

use super::app::{App, CircuitState, LogEntry, LogLevel};

/// Cost per inference call in USD (used for savings estimate).
const COST_PER_INFERENCE: f64 = 0.012;

/// Mock data generator that produces realistic synthetic dashboard data.
///
/// Uses the app's tick_count as a time base to produce deterministic,
/// visually interesting patterns including circuit breaker cycling,
/// organic throughput curves, and natural latency fluctuations.
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
    ///
    /// This produces:
    /// - Pipeline latencies with natural fluctuation and occasional INFER spikes
    /// - Channel depths that pulse under simulated load bursts
    /// - Circuit breaker cycling through states every ~90 seconds
    /// - ~80% dedup hit rate with realistic counter increments
    /// - Organic throughput sparkline with smooth curves
    /// - Varied log entries at realistic intervals
    pub fn tick(&self, app: &mut App) {
        app.tick_count += 1;
        app.uptime_secs += 1;

        let t = app.tick_count as f64;

        self.update_latencies(app, t);
        self.update_channels(app, t);
        self.update_circuit_breakers(app, t);
        self.update_dedup(app, t);
        self.update_throughput(app, t);
        self.update_health(app, t);
        self.update_logs(app, t);
        self.update_active_stage(app, t);
    }

    /// Updates pipeline stage latencies with natural fluctuation.
    fn update_latencies(&self, app: &mut App, t: f64) {
        // RAG: 3-6ms with slow oscillation
        app.stage_latencies[0] = 4.2 + 1.5 * (t * 0.07).sin() + 0.3 * (t * 0.31).cos();

        // ASSEMBLE: 1-3ms, stable
        app.stage_latencies[1] = 1.8 + 0.6 * (t * 0.11).sin() + 0.2 * (t * 0.47).cos();

        // INFER: 250-350ms base, occasional spikes to 400ms+
        let spike = if (t % 45.0) < 5.0 {
            100.0 * ((t % 45.0) * 0.6).sin().abs()
        } else {
            0.0
        };
        app.stage_latencies[2] = 287.0 + 40.0 * (t * 0.05).sin() + spike + 15.0 * (t * 0.23).cos();

        // POST: 1.5-3ms
        app.stage_latencies[3] = 2.1 + 0.7 * (t * 0.09).sin() + 0.3 * (t * 0.19).cos();

        // STREAM: 0.5-1.5ms
        app.stage_latencies[4] = 0.9 + 0.4 * (t * 0.13).sin() + 0.1 * (t * 0.37).cos();

        // Clamp all to non-negative
        for lat in &mut app.stage_latencies {
            if *lat < 0.1 {
                *lat = 0.1;
            }
        }
    }

    /// Updates channel depths with pulsing load patterns.
    fn update_channels(&self, app: &mut App, t: f64) {
        // RAG → ASM: moderate fill with slow pulse
        let cap0 = app.channel_depths[0].capacity as f64;
        app.channel_depths[0].current =
            ((cap0 * 0.6 + cap0 * 0.2 * (t * 0.08).sin() + cap0 * 0.1 * (t * 0.29).cos()).max(0.0)
                as usize)
                .min(app.channel_depths[0].capacity);

        // ASM → INF: low fill (inference is fast enough to keep up)
        let cap1 = app.channel_depths[1].capacity as f64;
        app.channel_depths[1].current =
            ((cap1 * 0.15 + cap1 * 0.08 * (t * 0.12).sin() + cap1 * 0.05 * (t * 0.41).cos())
                .max(0.0) as usize)
                .min(app.channel_depths[1].capacity);

        // INF → PST: high fill during inference spikes
        let cap2 = app.channel_depths[2].capacity as f64;
        let inf_pressure = if (t % 45.0) < 5.0 { 0.25 } else { 0.0 };
        app.channel_depths[2].current =
            ((cap2 * (0.65 + inf_pressure) + cap2 * 0.15 * (t * 0.06).sin()).max(0.0) as usize)
                .min(app.channel_depths[2].capacity);

        // PST → STR: low fill
        let cap3 = app.channel_depths[3].capacity as f64;
        app.channel_depths[3].current =
            ((cap3 * 0.08 + cap3 * 0.05 * (t * 0.15).sin() + cap3 * 0.03 * (t * 0.33).cos())
                .max(0.0) as usize)
                .min(app.channel_depths[3].capacity);
    }

    /// Cycles circuit breakers through states on a ~90 second period.
    fn update_circuit_breakers(&self, app: &mut App, t: f64) {
        // openai: always closed, accumulating successes
        let ok_count = app.tick_count * 2 + 47;
        if let Some(cb) = app.circuit_breakers.get_mut(0) {
            cb.state = CircuitState::Closed;
            cb.detail = format!("({} ok)", ok_count);
        }

        // anthropic: cycles through states every ~90s
        // 0-60s: Closed, 60-75s: Open, 75-85s: HalfOpen, 85-90s: Closed
        let cycle_pos = (t % 90.0) as u64;
        if let Some(cb) = app.circuit_breakers.get_mut(1) {
            if cycle_pos < 60 {
                cb.state = CircuitState::Closed;
                let count = (t * 1.3) as u64 + 200;
                cb.detail = format!("({} ok)", count);
            } else if cycle_pos < 75 {
                cb.state = CircuitState::Open;
                let ago = cycle_pos - 60;
                cb.detail = format!("(opened {}s ago)", ago);
            } else if cycle_pos < 85 {
                cb.state = CircuitState::HalfOpen;
                let probe_in = 85 - cycle_pos;
                cb.detail = format!("(probe in {}s)", probe_in);
            } else {
                cb.state = CircuitState::Closed;
                cb.detail = "(recovering)".into();
            }
        }

        // llama.cpp: independent cycle, offset by 30s
        let llama_cycle = ((t + 30.0) % 120.0) as u64;
        if let Some(cb) = app.circuit_breakers.get_mut(2) {
            if llama_cycle < 80 {
                cb.state = CircuitState::Closed;
                let count = (t * 0.8) as u64 + 100;
                cb.detail = format!("({} ok)", count);
            } else if llama_cycle < 100 {
                cb.state = CircuitState::Open;
                let ago = llama_cycle - 80;
                cb.detail = format!("(opened {}s ago)", ago);
            } else if llama_cycle < 115 {
                cb.state = CircuitState::HalfOpen;
                let probe_in = 115 - llama_cycle;
                cb.detail = format!("(probe in {}s)", probe_in);
            } else {
                cb.state = CircuitState::Closed;
                cb.detail = "(recovering)".into();
            }
        }
    }

    /// Updates dedup statistics with ~80% hit rate.
    fn update_dedup(&self, app: &mut App, _t: f64) {
        // ~5 new requests per tick, ~1 actually needs inference
        app.requests_total += 5;
        app.inferences_total += 1;
        app.cost_saved_usd += COST_PER_INFERENCE * 4.0;
    }

    /// Generates organic throughput pattern with smooth curves.
    fn update_throughput(&self, app: &mut App, t: f64) {
        // Multi-frequency sine wave for organic-looking load
        let load = self.base_throughput
            + 8.0 * (t * 0.04).sin()
            + 4.0 * (t * 0.11).sin()
            + 3.0 * (t * 0.19).cos()
            + 2.0 * (t * 0.31).sin();

        // Occasional burst
        let burst = if (t % 30.0) < 3.0 { 10.0 } else { 0.0 };

        let value = (load + burst).max(1.0) as u64;
        app.push_throughput(value);
    }

    /// Updates system health metrics.
    fn update_health(&self, app: &mut App, t: f64) {
        // CPU: 50-75% with load correlation
        app.cpu_percent = 62.0 + 10.0 * (t * 0.05).sin() + 3.0 * (t * 0.17).cos();
        app.cpu_percent = app.cpu_percent.clamp(0.0, 100.0);

        // MEM: slow climb with sawtooth (GC-like) pattern
        let base_mem = 38.0 + (t % 120.0) * 0.1;
        app.mem_percent = base_mem + 3.0 * (t * 0.03).sin();
        app.mem_percent = app.mem_percent.clamp(0.0, 100.0);

        // Active tasks: correlated with throughput
        app.active_tasks = (200.0 + 50.0 * (t * 0.06).sin() + 20.0 * (t * 0.15).cos()) as usize;
    }

    /// Generates realistic log entries at varied intervals.
    fn update_logs(&self, app: &mut App, t: f64) {
        let tick = app.tick_count;
        let ts = format!(
            "{:02}:{:02}:{:02}",
            (app.uptime_secs / 3600) % 24,
            (app.uptime_secs % 3600) / 60,
            app.uptime_secs % 60
        );

        // Info logs every 2-3 ticks
        if tick % 3 == 0 {
            let messages = [
                (
                    "Request deduped",
                    format!(
                        "key={:06x} session=user-{}",
                        (t * 7.3) as u32 & 0xFFFFFF,
                        (t * 1.1) as u32 % 500
                    ),
                ),
                (
                    "Batch complete",
                    format!(
                        "prompts={} duration={:.1}s cost=${:.2}",
                        800 + (t * 0.7) as u64 % 100,
                        10.0 + (t * 0.3).sin() * 3.0,
                        1.5 + (t * 0.1).sin() * 0.8
                    ),
                ),
                (
                    "Stage latency nominal",
                    format!("stage=RAG p99={:.1}ms", app.stage_latencies[0]),
                ),
            ];
            let idx = (tick / 3) as usize % messages.len();
            app.push_log(LogEntry {
                timestamp: ts.clone(),
                level: LogLevel::Info,
                message: messages[idx].0.to_string(),
                fields: messages[idx].1.clone(),
            });
        }

        // Warn logs on circuit breaker state changes
        if tick % 15 == 0 {
            let cycle_pos = (t % 90.0) as u64;
            if (60..=62).contains(&cycle_pos) || (75..=77).contains(&cycle_pos) {
                app.push_log(LogEntry {
                    timestamp: ts.clone(),
                    level: LogLevel::Warn,
                    message: "Circuit state change".to_string(),
                    fields: "worker=anthropic".to_string(),
                });
            }
        }

        // Error logs during inference spikes
        if (t % 45.0) < 2.0 && tick % 5 == 0 {
            app.push_log(LogEntry {
                timestamp: ts.clone(),
                level: LogLevel::Error,
                message: "Worker timeout".to_string(),
                fields: format!("worker=llama.cpp attempt={}/3", 1 + (tick % 3)),
            });
        }

        // Latency spike warnings
        if app.stage_latencies[2] > 380.0 && tick % 7 == 0 {
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

    /// Updates the active pipeline stage indicator.
    fn update_active_stage(&self, app: &mut App, t: f64) {
        // Cycle through stages to show animation
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
}

impl LiveMetrics {
    /// Creates a new live metrics source.
    ///
    /// # Arguments
    /// * `metrics_url` - Full URL to the Prometheus metrics endpoint.
    pub fn new(metrics_url: String) -> Self {
        Self { metrics_url }
    }

    /// Fetches metrics from the Prometheus endpoint and updates app state.
    ///
    /// # Arguments
    /// * `app` - Mutable reference to the app state to update.
    ///
    /// # Returns
    /// `Ok(())` on success, or an error string on failure.
    /// On failure, app state is left unchanged (no partial updates).
    pub async fn tick(&self, app: &mut App) -> Result<(), String> {
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
    fn parse_metrics(&self, body: &str, app: &mut App) {
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
        // INFER should spike above 350ms at some point
        assert!(
            max_infer > 300.0,
            "Expected INFER spikes, max was {}",
            max_infer
        );
        // But should never be astronomically high
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
    fn test_mock_metrics_circuit_breaker_cycles() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));

        let mut saw_closed = false;
        let mut saw_open = false;
        let mut saw_half_open = false;

        // Run for 100 ticks to see a full cycle
        for _ in 0..100 {
            mock.tick(&mut app);
            if let Some(cb) = app.circuit_breakers.get(1) {
                match cb.state {
                    CircuitState::Closed => saw_closed = true,
                    CircuitState::Open => saw_open = true,
                    CircuitState::HalfOpen => saw_half_open = true,
                }
            }
        }

        assert!(saw_closed, "anthropic CB never entered Closed state");
        assert!(saw_open, "anthropic CB never entered Open state");
        assert!(saw_half_open, "anthropic CB never entered HalfOpen state");
    }

    #[test]
    fn test_mock_metrics_dedup_ratio_approximately_eighty_percent() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));
        for _ in 0..100 {
            mock.tick(&mut app);
        }
        let savings = app.dedup_savings_percent();
        assert!(
            savings > 70.0 && savings < 90.0,
            "Expected ~80% savings, got {:.1}%",
            savings
        );
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
        let live = LiveMetrics::new("http://localhost:9090/metrics".into());
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
        let live = LiveMetrics::new("http://localhost:9090/metrics".into());
        let mut app = App::new(Duration::from_secs(1));

        let body = "orchestrator_queue_depth{queue=\"rag_to_asm\"} 412\n\
                     orchestrator_queue_depth{queue=\"inf_to_pst\"} 891\n";
        live.parse_metrics(body, &mut app);

        assert_eq!(app.channel_depths[0].current, 412);
        assert_eq!(app.channel_depths[2].current, 891);
    }

    #[test]
    fn test_live_metrics_parse_request_totals() {
        let live = LiveMetrics::new("http://localhost:9090/metrics".into());
        let mut app = App::new(Duration::from_secs(1));

        let body = "orchestrator_requests_total 1847\norchestrator_inferences_total 312\n";
        live.parse_metrics(body, &mut app);

        assert_eq!(app.requests_total, 1847);
        assert_eq!(app.inferences_total, 312);
    }

    #[test]
    fn test_live_metrics_parse_empty_body() {
        let live = LiveMetrics::new("http://localhost:9090/metrics".into());
        let mut app = App::new(Duration::from_secs(1));
        live.parse_metrics("", &mut app);
        // Should not crash, all defaults intact
        assert_eq!(app.requests_total, 0);
    }

    #[test]
    fn test_live_metrics_parse_comments_only() {
        let live = LiveMetrics::new("http://localhost:9090/metrics".into());
        let mut app = App::new(Duration::from_secs(1));
        live.parse_metrics("# HELP something\n# TYPE something gauge\n", &mut app);
        assert_eq!(app.requests_total, 0);
    }

    #[test]
    fn test_live_metrics_parse_malformed_value() {
        let live = LiveMetrics::new("http://localhost:9090/metrics".into());
        let mut app = App::new(Duration::from_secs(1));
        live.parse_metrics("orchestrator_requests_total not_a_number\n", &mut app);
        // Should not crash, value unchanged
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
        // Simulate 10 minutes of ticks
        for _ in 0..600 {
            mock.tick(&mut app);
        }
        assert_eq!(app.tick_count, 600);
        assert!(app.throughput_history.len() <= 60); // bounded
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
    fn test_mock_metrics_llama_cycles() {
        let mock = MockMetrics::new();
        let mut app = App::new(Duration::from_secs(1));

        let mut saw_open = false;
        let mut saw_half = false;
        for _ in 0..150 {
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
