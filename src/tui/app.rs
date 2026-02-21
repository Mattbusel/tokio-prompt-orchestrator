//! # Module: TUI App State
//!
//! ## Responsibility
//! Owns all dashboard state and provides update/tick logic. The `App` struct is the
//! single source of truth for every widget's data. State transitions are deterministic
//! and testable in isolation.
//!
//! ## Guarantees
//! - All numeric fields are bounded to valid display ranges
//! - `VecDeque` collections are bounded and never grow unbounded
//! - `update()` and `on_tick()` never panic

use std::collections::VecDeque;
use std::time::Duration;

/// Maximum number of throughput history data points (one per second, 60s window).
pub const THROUGHPUT_HISTORY_CAP: usize = 60;

/// Maximum number of log entries retained for display.
pub const LOG_ENTRIES_CAP: usize = 50;

/// Minimum terminal width for the dashboard to render.
pub const MIN_COLS: u16 = 100;

/// Minimum terminal height for the dashboard to render.
pub const MIN_ROWS: u16 = 40;

/// Pipeline stage names in order.
pub const STAGE_NAMES: [&str; 5] = ["RAG", "ASSEMBLE", "INFER", "POST", "STREAM"];

/// Pipeline stage latency budgets in milliseconds (P99).
pub const STAGE_BUDGETS_MS: [f64; 5] = [20.0, 10.0, 400.0, 15.0, 10.0];

/// Primary application state for the TUI dashboard.
#[derive(Debug)]
pub struct App {
    /// Whether the application should exit.
    pub should_quit: bool,
    /// Whether display updates are paused (data still collected).
    pub paused: bool,
    /// Whether the help overlay is visible.
    pub show_help: bool,
    /// Monotonic tick counter (incremented each data tick).
    pub tick_count: u64,

    /// Pipeline stage latencies in milliseconds (rolling average).
    /// Order: RAG, ASSEMBLE, INFER, POST, STREAM.
    pub stage_latencies: [f64; 5],
    /// Index of the currently-processing stage, if any.
    pub active_stage: Option<usize>,

    /// Channel depths between pipeline stages.
    pub channel_depths: [ChannelDepth; 4],

    /// Circuit breaker states for each worker backend.
    pub circuit_breakers: Vec<CircuitBreakerStatus>,

    /// Total requests received (for dedup calculations).
    pub requests_total: u64,
    /// Total inferences actually executed (post-dedup).
    pub inferences_total: u64,
    /// Estimated cost saved via deduplication in USD.
    pub cost_saved_usd: f64,

    /// Throughput history: requests per second for the last 60 seconds.
    pub throughput_history: VecDeque<u64>,

    /// Current CPU usage percentage (0.0 - 100.0).
    pub cpu_percent: f64,
    /// Current memory usage percentage (0.0 - 100.0).
    pub mem_percent: f64,
    /// Number of active Tokio tasks.
    pub active_tasks: usize,
    /// Uptime in seconds.
    pub uptime_secs: u64,

    /// Rolling log entries, newest at the back.
    pub log_entries: VecDeque<LogEntry>,

    /// Data update interval.
    pub tick_rate: Duration,
}

/// Depth of a bounded channel between two pipeline stages.
#[derive(Debug, Clone)]
pub struct ChannelDepth {
    /// Human-readable label, e.g. "RAG \u{2192} ASM".
    pub name: &'static str,
    /// Current queue depth.
    pub current: usize,
    /// Maximum capacity.
    pub capacity: usize,
}

/// Status of a single circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreakerStatus {
    /// Worker/service name.
    pub name: String,
    /// Current circuit state.
    pub state: CircuitState,
    /// Human-readable detail string.
    pub detail: String,
}

/// Circuit breaker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Circuit is closed (healthy, requests flow through).
    Closed,
    /// Circuit is half-open (probing with test requests).
    HalfOpen,
    /// Circuit is open (requests fast-fail).
    Open,
}

/// A single log entry for the log tail widget.
#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    /// Formatted timestamp string, e.g. "14:32:01".
    pub timestamp: String,
    /// Severity level.
    pub level: LogLevel,
    /// Primary log message.
    pub message: String,
    /// Structured fields as a formatted string.
    pub fields: String,
}

/// Log severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Informational message.
    Info,
    /// Warning condition.
    Warn,
    /// Error condition.
    Error,
    /// Debug-level message.
    Debug,
}

impl App {
    /// Creates a new `App` with default/empty state.
    ///
    /// # Returns
    /// A fresh `App` instance ready for the event loop.
    pub fn new(tick_rate: Duration) -> Self {
        Self {
            should_quit: false,
            paused: false,
            show_help: false,
            tick_count: 0,

            stage_latencies: [0.0; 5],
            active_stage: None,

            channel_depths: [
                ChannelDepth {
                    name: "RAG  \u{2192} ASM",
                    current: 0,
                    capacity: 512,
                },
                ChannelDepth {
                    name: "ASM  \u{2192} INF",
                    current: 0,
                    capacity: 512,
                },
                ChannelDepth {
                    name: "INF  \u{2192} PST",
                    current: 0,
                    capacity: 1024,
                },
                ChannelDepth {
                    name: "PST  \u{2192} STR",
                    current: 0,
                    capacity: 512,
                },
            ],

            circuit_breakers: vec![
                CircuitBreakerStatus {
                    name: "openai".into(),
                    state: CircuitState::Closed,
                    detail: "(0 ok)".into(),
                },
                CircuitBreakerStatus {
                    name: "anthropic".into(),
                    state: CircuitState::Closed,
                    detail: "(0 ok)".into(),
                },
                CircuitBreakerStatus {
                    name: "llama.cpp".into(),
                    state: CircuitState::Closed,
                    detail: "(0 ok)".into(),
                },
            ],

            requests_total: 0,
            inferences_total: 0,
            cost_saved_usd: 0.0,

            throughput_history: VecDeque::with_capacity(THROUGHPUT_HISTORY_CAP),

            cpu_percent: 0.0,
            mem_percent: 0.0,
            active_tasks: 0,
            uptime_secs: 0,

            log_entries: VecDeque::with_capacity(LOG_ENTRIES_CAP),

            tick_rate,
        }
    }

    /// Resets all counters and statistics to zero.
    pub fn reset_stats(&mut self) {
        self.requests_total = 0;
        self.inferences_total = 0;
        self.cost_saved_usd = 0.0;
        self.throughput_history.clear();
        self.log_entries.clear();
        self.tick_count = 0;
    }

    /// Pushes a log entry, evicting the oldest if at capacity.
    pub fn push_log(&mut self, entry: LogEntry) {
        if self.log_entries.len() >= LOG_ENTRIES_CAP {
            self.log_entries.pop_front();
        }
        self.log_entries.push_back(entry);
    }

    /// Pushes a throughput sample, evicting the oldest if at capacity.
    pub fn push_throughput(&mut self, value: u64) {
        if self.throughput_history.len() >= THROUGHPUT_HISTORY_CAP {
            self.throughput_history.pop_front();
        }
        self.throughput_history.push_back(value);
    }

    /// Computes the deduplication savings percentage.
    ///
    /// # Returns
    /// Percentage of requests that were deduplicated (0.0 - 100.0).
    /// Returns 0.0 if no requests have been received.
    pub fn dedup_savings_percent(&self) -> f64 {
        if self.requests_total == 0 {
            return 0.0;
        }
        let deduped = self.requests_total.saturating_sub(self.inferences_total);
        (deduped as f64 / self.requests_total as f64) * 100.0
    }

    /// Returns human-readable uptime string.
    ///
    /// # Returns
    /// Formatted string like "2d 14h 32m".
    pub fn uptime_display(&self) -> String {
        let days = self.uptime_secs / 86400;
        let hours = (self.uptime_secs % 86400) / 3600;
        let mins = (self.uptime_secs % 3600) / 60;
        if days > 0 {
            format!("{}d {}h {}m", days, hours, mins)
        } else if hours > 0 {
            format!("{}h {}m", hours, mins)
        } else {
            format!("{}m", mins)
        }
    }

    /// Returns the fill ratio for a channel depth (0.0 - 1.0).
    ///
    /// # Arguments
    /// * `index` - Channel index (0-3).
    ///
    /// # Returns
    /// Fill ratio clamped to [0.0, 1.0], or 0.0 if index is out of range.
    pub fn channel_fill_ratio(&self, index: usize) -> f64 {
        if index >= self.channel_depths.len() {
            return 0.0;
        }
        let ch = &self.channel_depths[index];
        if ch.capacity == 0 {
            return 0.0;
        }
        (ch.current as f64 / ch.capacity as f64).min(1.0)
    }

    /// Returns the latency ratio for a pipeline stage relative to its P99 budget.
    ///
    /// # Arguments
    /// * `index` - Stage index (0-4).
    ///
    /// # Returns
    /// Ratio of current latency to budget. Values >1.0 indicate budget exceeded.
    /// Returns 0.0 if index is out of range or budget is zero.
    pub fn stage_budget_ratio(&self, index: usize) -> f64 {
        if index >= self.stage_latencies.len() || index >= STAGE_BUDGETS_MS.len() {
            return 0.0;
        }
        let budget = STAGE_BUDGETS_MS[index];
        if budget <= 0.0 {
            return 0.0;
        }
        self.stage_latencies[index] / budget
    }
}

impl ChannelDepth {
    /// Returns the fill ratio (0.0 - 1.0).
    pub fn fill_ratio(&self) -> f64 {
        if self.capacity == 0 {
            return 0.0;
        }
        (self.current as f64 / self.capacity as f64).min(1.0)
    }
}

impl CircuitState {
    /// Returns the display symbol for this circuit state.
    pub fn symbol(&self) -> &'static str {
        match self {
            Self::Closed => "\u{25cf}",   // ●
            Self::HalfOpen => "\u{25d0}", // ◐
            Self::Open => "\u{25cb}",     // ○
        }
    }

    /// Returns the display label for this circuit state.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Closed => "CLOSED",
            Self::HalfOpen => "HALF-OPEN",
            Self::Open => "OPEN",
        }
    }
}

impl LogLevel {
    /// Returns the display label for this log level.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Info => "INFO ",
            Self::Warn => "WARN ",
            Self::Error => "ERROR",
            Self::Debug => "DEBUG",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_new_initializes_with_defaults() {
        let app = App::new(Duration::from_secs(1));
        assert!(!app.should_quit);
        assert!(!app.paused);
        assert!(!app.show_help);
        assert_eq!(app.tick_count, 0);
        assert_eq!(app.requests_total, 0);
        assert_eq!(app.inferences_total, 0);
        assert_eq!(app.throughput_history.len(), 0);
        assert_eq!(app.log_entries.len(), 0);
        assert_eq!(app.circuit_breakers.len(), 3);
        assert_eq!(app.channel_depths.len(), 4);
    }

    #[test]
    fn test_app_reset_stats_clears_counters() {
        let mut app = App::new(Duration::from_secs(1));
        app.requests_total = 1000;
        app.inferences_total = 200;
        app.cost_saved_usd = 50.0;
        app.push_throughput(42);
        app.push_log(LogEntry {
            timestamp: "00:00:00".into(),
            level: LogLevel::Info,
            message: "test".into(),
            fields: "".into(),
        });
        app.tick_count = 99;

        app.reset_stats();

        assert_eq!(app.requests_total, 0);
        assert_eq!(app.inferences_total, 0);
        assert_eq!(app.cost_saved_usd, 0.0);
        assert_eq!(app.throughput_history.len(), 0);
        assert_eq!(app.log_entries.len(), 0);
        assert_eq!(app.tick_count, 0);
    }

    #[test]
    fn test_dedup_savings_percent_zero_requests() {
        let app = App::new(Duration::from_secs(1));
        assert_eq!(app.dedup_savings_percent(), 0.0);
    }

    #[test]
    fn test_dedup_savings_percent_eighty_percent() {
        let mut app = App::new(Duration::from_secs(1));
        app.requests_total = 1000;
        app.inferences_total = 200;
        let savings = app.dedup_savings_percent();
        assert!((savings - 80.0).abs() < 0.01);
    }

    #[test]
    fn test_dedup_savings_percent_no_dedup() {
        let mut app = App::new(Duration::from_secs(1));
        app.requests_total = 500;
        app.inferences_total = 500;
        assert_eq!(app.dedup_savings_percent(), 0.0);
    }

    #[test]
    fn test_dedup_savings_percent_all_deduped() {
        let mut app = App::new(Duration::from_secs(1));
        app.requests_total = 500;
        app.inferences_total = 0;
        assert_eq!(app.dedup_savings_percent(), 100.0);
    }

    #[test]
    fn test_uptime_display_minutes_only() {
        let mut app = App::new(Duration::from_secs(1));
        app.uptime_secs = 120;
        assert_eq!(app.uptime_display(), "2m");
    }

    #[test]
    fn test_uptime_display_hours_and_minutes() {
        let mut app = App::new(Duration::from_secs(1));
        app.uptime_secs = 3661;
        assert_eq!(app.uptime_display(), "1h 1m");
    }

    #[test]
    fn test_uptime_display_days_hours_minutes() {
        let mut app = App::new(Duration::from_secs(1));
        app.uptime_secs = 2 * 86400 + 14 * 3600 + 32 * 60;
        assert_eq!(app.uptime_display(), "2d 14h 32m");
    }

    #[test]
    fn test_uptime_display_zero() {
        let app = App::new(Duration::from_secs(1));
        assert_eq!(app.uptime_display(), "0m");
    }

    #[test]
    fn test_channel_fill_ratio_empty() {
        let app = App::new(Duration::from_secs(1));
        assert_eq!(app.channel_fill_ratio(0), 0.0);
    }

    #[test]
    fn test_channel_fill_ratio_half() {
        let mut app = App::new(Duration::from_secs(1));
        app.channel_depths[0].current = 256;
        assert!((app.channel_fill_ratio(0) - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_channel_fill_ratio_full() {
        let mut app = App::new(Duration::from_secs(1));
        app.channel_depths[0].current = 512;
        assert_eq!(app.channel_fill_ratio(0), 1.0);
    }

    #[test]
    fn test_channel_fill_ratio_overflow_clamped() {
        let mut app = App::new(Duration::from_secs(1));
        app.channel_depths[0].current = 1000;
        assert_eq!(app.channel_fill_ratio(0), 1.0);
    }

    #[test]
    fn test_channel_fill_ratio_out_of_range() {
        let app = App::new(Duration::from_secs(1));
        assert_eq!(app.channel_fill_ratio(99), 0.0);
    }

    #[test]
    fn test_channel_fill_ratio_zero_capacity() {
        let mut app = App::new(Duration::from_secs(1));
        app.channel_depths[0].capacity = 0;
        assert_eq!(app.channel_fill_ratio(0), 0.0);
    }

    #[test]
    fn test_stage_budget_ratio_within_budget() {
        let mut app = App::new(Duration::from_secs(1));
        app.stage_latencies[0] = 10.0; // RAG budget is 20ms
        assert!((app.stage_budget_ratio(0) - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_stage_budget_ratio_over_budget() {
        let mut app = App::new(Duration::from_secs(1));
        app.stage_latencies[2] = 500.0; // INFER budget is 400ms
        assert!(app.stage_budget_ratio(2) > 1.0);
    }

    #[test]
    fn test_stage_budget_ratio_out_of_range() {
        let app = App::new(Duration::from_secs(1));
        assert_eq!(app.stage_budget_ratio(99), 0.0);
    }

    #[test]
    fn test_push_log_bounded() {
        let mut app = App::new(Duration::from_secs(1));
        for i in 0..(LOG_ENTRIES_CAP + 10) {
            app.push_log(LogEntry {
                timestamp: format!("{:05}", i),
                level: LogLevel::Info,
                message: format!("msg {}", i),
                fields: String::new(),
            });
        }
        assert_eq!(app.log_entries.len(), LOG_ENTRIES_CAP);
        // Newest entry should be the last one pushed
        let last = app.log_entries.back();
        assert!(last.is_some());
    }

    #[test]
    fn test_push_throughput_bounded() {
        let mut app = App::new(Duration::from_secs(1));
        for i in 0..(THROUGHPUT_HISTORY_CAP + 20) {
            app.push_throughput(i as u64);
        }
        assert_eq!(app.throughput_history.len(), THROUGHPUT_HISTORY_CAP);
    }

    #[test]
    fn test_circuit_state_symbol_closed() {
        assert_eq!(CircuitState::Closed.symbol(), "\u{25cf}");
    }

    #[test]
    fn test_circuit_state_symbol_half_open() {
        assert_eq!(CircuitState::HalfOpen.symbol(), "\u{25d0}");
    }

    #[test]
    fn test_circuit_state_symbol_open() {
        assert_eq!(CircuitState::Open.symbol(), "\u{25cb}");
    }

    #[test]
    fn test_circuit_state_label_closed() {
        assert_eq!(CircuitState::Closed.label(), "CLOSED");
    }

    #[test]
    fn test_circuit_state_label_half_open() {
        assert_eq!(CircuitState::HalfOpen.label(), "HALF-OPEN");
    }

    #[test]
    fn test_circuit_state_label_open() {
        assert_eq!(CircuitState::Open.label(), "OPEN");
    }

    #[test]
    fn test_log_level_label_info() {
        assert_eq!(LogLevel::Info.label(), "INFO ");
    }

    #[test]
    fn test_log_level_label_warn() {
        assert_eq!(LogLevel::Warn.label(), "WARN ");
    }

    #[test]
    fn test_log_level_label_error() {
        assert_eq!(LogLevel::Error.label(), "ERROR");
    }

    #[test]
    fn test_log_level_label_debug() {
        assert_eq!(LogLevel::Debug.label(), "DEBUG");
    }

    #[test]
    fn test_channel_depth_fill_ratio() {
        let ch = ChannelDepth {
            name: "test",
            current: 256,
            capacity: 512,
        };
        assert!((ch.fill_ratio() - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_channel_depth_fill_ratio_zero_cap() {
        let ch = ChannelDepth {
            name: "test",
            current: 10,
            capacity: 0,
        };
        assert_eq!(ch.fill_ratio(), 0.0);
    }

    #[test]
    fn test_channel_depth_fill_ratio_clamped() {
        let ch = ChannelDepth {
            name: "test",
            current: 1000,
            capacity: 512,
        };
        assert_eq!(ch.fill_ratio(), 1.0);
    }

    #[test]
    fn test_push_log_newest_at_back() {
        let mut app = App::new(Duration::from_secs(1));
        app.push_log(LogEntry {
            timestamp: "first".into(),
            level: LogLevel::Info,
            message: "first".into(),
            fields: String::new(),
        });
        app.push_log(LogEntry {
            timestamp: "second".into(),
            level: LogLevel::Warn,
            message: "second".into(),
            fields: String::new(),
        });
        assert_eq!(app.log_entries.len(), 2);
        assert_eq!(
            app.log_entries.back().map(|e| e.message.as_str()),
            Some("second")
        );
        assert_eq!(
            app.log_entries.front().map(|e| e.message.as_str()),
            Some("first")
        );
    }
}
