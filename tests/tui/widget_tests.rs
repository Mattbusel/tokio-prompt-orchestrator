//! Additional widget-level tests for ratio compliance.
//!
//! Tests fill bar calculations, circuit state displays, log level colors,
//! and dedup number formatting at edge cases.

use std::time::Duration;
use tokio_prompt_orchestrator::tui::app::{
    App, ChannelDepth, CircuitState, LogEntry, LogLevel, STAGE_BUDGETS_MS, STAGE_NAMES,
};
use tokio_prompt_orchestrator::tui::events::{apply_event, InputEvent};
use tokio_prompt_orchestrator::tui::metrics::MockMetrics;

// ── Fill bar calculation tests ──────────────────────────────────────────

#[test]
fn test_channel_fill_ratio_at_zero_percent() {
    let ch = ChannelDepth {
        name: "test",
        current: 0,
        capacity: 512,
    };
    assert_eq!(ch.fill_ratio(), 0.0);
}

#[test]
fn test_channel_fill_ratio_at_fifty_percent() {
    let ch = ChannelDepth {
        name: "test",
        current: 256,
        capacity: 512,
    };
    let ratio = ch.fill_ratio();
    assert!((ratio - 0.5).abs() < 0.001, "Expected 0.5, got {}", ratio);
}

#[test]
fn test_channel_fill_ratio_at_eighty_five_percent() {
    let ch = ChannelDepth {
        name: "test",
        current: 435,
        capacity: 512,
    };
    let ratio = ch.fill_ratio();
    let expected = 435.0 / 512.0;
    assert!(
        (ratio - expected).abs() < 0.001,
        "Expected {}, got {}",
        expected,
        ratio
    );
}

#[test]
fn test_channel_fill_ratio_at_hundred_percent() {
    let ch = ChannelDepth {
        name: "test",
        current: 512,
        capacity: 512,
    };
    assert_eq!(ch.fill_ratio(), 1.0);
}

#[test]
fn test_channel_fill_ratio_over_capacity_clamped() {
    let ch = ChannelDepth {
        name: "test",
        current: 600,
        capacity: 512,
    };
    assert_eq!(ch.fill_ratio(), 1.0);
}

#[test]
fn test_channel_fill_ratio_zero_capacity() {
    let ch = ChannelDepth {
        name: "test",
        current: 100,
        capacity: 0,
    };
    assert_eq!(ch.fill_ratio(), 0.0);
}

#[test]
fn test_channel_fill_ratio_both_zero() {
    let ch = ChannelDepth {
        name: "test",
        current: 0,
        capacity: 0,
    };
    assert_eq!(ch.fill_ratio(), 0.0);
}

#[test]
fn test_channel_fill_ratio_one_of_one() {
    let ch = ChannelDepth {
        name: "test",
        current: 1,
        capacity: 1,
    };
    assert_eq!(ch.fill_ratio(), 1.0);
}

// ── Circuit state display tests ─────────────────────────────────────────

#[test]
fn test_circuit_state_closed_symbol_is_filled_circle() {
    let sym = CircuitState::Closed.symbol();
    assert!(!sym.is_empty());
    assert_eq!(sym, "\u{25cf}"); // ●
}

#[test]
fn test_circuit_state_half_open_symbol_is_half_circle() {
    let sym = CircuitState::HalfOpen.symbol();
    assert!(!sym.is_empty());
    assert_eq!(sym, "\u{25d0}"); // ◐
}

#[test]
fn test_circuit_state_open_symbol_is_empty_circle() {
    let sym = CircuitState::Open.symbol();
    assert!(!sym.is_empty());
    assert_eq!(sym, "\u{25cb}"); // ○
}

#[test]
fn test_circuit_state_all_symbols_distinct() {
    let symbols: Vec<&str> = vec![
        CircuitState::Closed.symbol(),
        CircuitState::HalfOpen.symbol(),
        CircuitState::Open.symbol(),
    ];
    assert_ne!(symbols[0], symbols[1]);
    assert_ne!(symbols[1], symbols[2]);
    assert_ne!(symbols[0], symbols[2]);
}

#[test]
fn test_circuit_state_all_labels_distinct() {
    let labels: Vec<&str> = vec![
        CircuitState::Closed.label(),
        CircuitState::HalfOpen.label(),
        CircuitState::Open.label(),
    ];
    assert_ne!(labels[0], labels[1]);
    assert_ne!(labels[1], labels[2]);
    assert_ne!(labels[0], labels[2]);
}

#[test]
fn test_circuit_state_closed_label() {
    assert_eq!(CircuitState::Closed.label(), "CLOSED");
}

#[test]
fn test_circuit_state_half_open_label() {
    assert_eq!(CircuitState::HalfOpen.label(), "HALF-OPEN");
}

#[test]
fn test_circuit_state_open_label() {
    assert_eq!(CircuitState::Open.label(), "OPEN");
}

#[test]
fn test_circuit_state_equality() {
    assert_eq!(CircuitState::Closed, CircuitState::Closed);
    assert_eq!(CircuitState::Open, CircuitState::Open);
    assert_eq!(CircuitState::HalfOpen, CircuitState::HalfOpen);
    assert_ne!(CircuitState::Closed, CircuitState::Open);
}

// ── Log level color mapping tests ───────────────────────────────────────

#[test]
fn test_log_level_info_label_is_fixed_width() {
    assert_eq!(LogLevel::Info.label().len(), 5);
}

#[test]
fn test_log_level_warn_label_is_fixed_width() {
    assert_eq!(LogLevel::Warn.label().len(), 5);
}

#[test]
fn test_log_level_error_label_is_fixed_width() {
    assert_eq!(LogLevel::Error.label().len(), 5);
}

#[test]
fn test_log_level_debug_label_is_fixed_width() {
    assert_eq!(LogLevel::Debug.label().len(), 5);
}

#[test]
fn test_log_level_all_labels_fixed_width() {
    let levels = [
        LogLevel::Info,
        LogLevel::Warn,
        LogLevel::Error,
        LogLevel::Debug,
    ];
    for level in &levels {
        assert_eq!(
            level.label().len(),
            5,
            "{:?} label should be 5 chars, got {}",
            level,
            level.label().len()
        );
    }
}

// ── Stage budget ratio tests ────────────────────────────────────────────

#[test]
fn test_stage_budget_ratio_all_stages_zero_latency() {
    let app = App::new(Duration::from_secs(1));
    for i in 0..5 {
        assert_eq!(app.stage_budget_ratio(i), 0.0);
    }
}

#[test]
fn test_stage_budget_ratio_rag_at_budget() {
    let mut app = App::new(Duration::from_secs(1));
    app.stage_latencies[0] = STAGE_BUDGETS_MS[0]; // RAG at exact budget
    let ratio = app.stage_budget_ratio(0);
    assert!((ratio - 1.0).abs() < 0.001);
}

#[test]
fn test_stage_budget_ratio_infer_double_budget() {
    let mut app = App::new(Duration::from_secs(1));
    app.stage_latencies[2] = STAGE_BUDGETS_MS[2] * 2.0; // INFER at 2x budget
    let ratio = app.stage_budget_ratio(2);
    assert!((ratio - 2.0).abs() < 0.001);
}

#[test]
fn test_stage_budget_ratio_out_of_range_returns_zero() {
    let app = App::new(Duration::from_secs(1));
    assert_eq!(app.stage_budget_ratio(5), 0.0);
    assert_eq!(app.stage_budget_ratio(100), 0.0);
    assert_eq!(app.stage_budget_ratio(usize::MAX), 0.0);
}

// ── Uptime display edge cases ───────────────────────────────────────────

#[test]
fn test_uptime_display_zero_seconds() {
    let app = App::new(Duration::from_secs(1));
    assert_eq!(app.uptime_display(), "0m");
}

#[test]
fn test_uptime_display_59_seconds() {
    let mut app = App::new(Duration::from_secs(1));
    app.uptime_secs = 59;
    assert_eq!(app.uptime_display(), "0m");
}

#[test]
fn test_uptime_display_60_seconds() {
    let mut app = App::new(Duration::from_secs(1));
    app.uptime_secs = 60;
    assert_eq!(app.uptime_display(), "1m");
}

#[test]
fn test_uptime_display_3599_seconds() {
    let mut app = App::new(Duration::from_secs(1));
    app.uptime_secs = 3599;
    assert_eq!(app.uptime_display(), "59m");
}

#[test]
fn test_uptime_display_3600_seconds() {
    let mut app = App::new(Duration::from_secs(1));
    app.uptime_secs = 3600;
    assert_eq!(app.uptime_display(), "1h 0m");
}

#[test]
fn test_uptime_display_one_day() {
    let mut app = App::new(Duration::from_secs(1));
    app.uptime_secs = 86400;
    assert_eq!(app.uptime_display(), "1d 0h 0m");
}

#[test]
fn test_uptime_display_max_realistic() {
    let mut app = App::new(Duration::from_secs(1));
    app.uptime_secs = 30 * 86400; // 30 days
    assert_eq!(app.uptime_display(), "30d 0h 0m");
}

// ── Dedup savings edge cases ────────────────────────────────────────────

#[test]
fn test_dedup_savings_zero_requests() {
    let app = App::new(Duration::from_secs(1));
    assert_eq!(app.dedup_savings_percent(), 0.0);
}

#[test]
fn test_dedup_savings_equal_requests_and_inferences() {
    let mut app = App::new(Duration::from_secs(1));
    app.requests_total = 100;
    app.inferences_total = 100;
    assert_eq!(app.dedup_savings_percent(), 0.0);
}

#[test]
fn test_dedup_savings_all_deduplicated() {
    let mut app = App::new(Duration::from_secs(1));
    app.requests_total = 100;
    app.inferences_total = 0;
    assert_eq!(app.dedup_savings_percent(), 100.0);
}

#[test]
fn test_dedup_savings_half_deduplicated() {
    let mut app = App::new(Duration::from_secs(1));
    app.requests_total = 100;
    app.inferences_total = 50;
    assert!((app.dedup_savings_percent() - 50.0).abs() < 0.01);
}

#[test]
fn test_dedup_savings_inferences_greater_than_requests() {
    let mut app = App::new(Duration::from_secs(1));
    app.requests_total = 50;
    app.inferences_total = 100;
    // saturating_sub means 0 deduped
    assert_eq!(app.dedup_savings_percent(), 0.0);
}

// ── App initialization tests ────────────────────────────────────────────

#[test]
fn test_app_initial_state_all_defaults() {
    let app = App::new(Duration::from_millis(500));
    assert!(!app.should_quit);
    assert!(!app.paused);
    assert!(!app.show_help);
    assert_eq!(app.tick_count, 0);
    assert_eq!(app.stage_latencies, [0.0; 5]);
    assert_eq!(app.active_stage, None);
    assert_eq!(app.channel_depths.len(), 4);
    assert_eq!(app.circuit_breakers.len(), 3);
    assert_eq!(app.requests_total, 0);
    assert_eq!(app.inferences_total, 0);
    assert_eq!(app.cost_saved_usd, 0.0);
    assert!(app.throughput_history.is_empty());
    assert_eq!(app.cpu_percent, 0.0);
    assert_eq!(app.mem_percent, 0.0);
    assert_eq!(app.active_tasks, 0);
    assert_eq!(app.uptime_secs, 0);
    assert!(app.log_entries.is_empty());
    assert_eq!(app.tick_rate, Duration::from_millis(500));
}

#[test]
fn test_app_channel_names() {
    let app = App::new(Duration::from_secs(1));
    assert!(app.channel_depths[0].name.contains("RAG"));
    assert!(app.channel_depths[1].name.contains("ASM"));
    assert!(app.channel_depths[2].name.contains("INF"));
    assert!(app.channel_depths[3].name.contains("PST"));
}

#[test]
fn test_app_circuit_breaker_names() {
    let app = App::new(Duration::from_secs(1));
    assert_eq!(app.circuit_breakers[0].name, "openai");
    assert_eq!(app.circuit_breakers[1].name, "anthropic");
    assert_eq!(app.circuit_breakers[2].name, "llama.cpp");
}

#[test]
fn test_app_all_circuit_breakers_start_closed() {
    let app = App::new(Duration::from_secs(1));
    for cb in &app.circuit_breakers {
        assert_eq!(
            cb.state,
            CircuitState::Closed,
            "CB {} should start Closed",
            cb.name
        );
    }
}

// ── Event application compound tests ────────────────────────────────────

#[test]
fn test_multiple_resets_are_idempotent() {
    let mut app = App::new(Duration::from_secs(1));
    let mock = MockMetrics::new();
    for _ in 0..10 {
        mock.tick(&mut app);
    }
    apply_event(&mut app, InputEvent::Reset);
    apply_event(&mut app, InputEvent::Reset);
    apply_event(&mut app, InputEvent::Reset);
    assert_eq!(app.requests_total, 0);
    assert_eq!(app.tick_count, 0);
}

#[test]
fn test_quit_after_pause_still_quits() {
    let mut app = App::new(Duration::from_secs(1));
    apply_event(&mut app, InputEvent::Pause);
    assert!(app.paused);
    apply_event(&mut app, InputEvent::Quit);
    assert!(app.should_quit);
}

#[test]
fn test_help_during_pause() {
    let mut app = App::new(Duration::from_secs(1));
    apply_event(&mut app, InputEvent::Pause);
    apply_event(&mut app, InputEvent::Help);
    assert!(app.paused);
    assert!(app.show_help);
}

#[test]
fn test_resize_event_does_not_affect_state() {
    let mut app = App::new(Duration::from_secs(1));
    let mock = MockMetrics::new();
    mock.tick(&mut app);
    let requests_before = app.requests_total;
    apply_event(&mut app, InputEvent::Resize(200, 60));
    assert_eq!(app.requests_total, requests_before);
    assert!(!app.should_quit);
    assert!(!app.paused);
}

// ── Log entry construction tests ────────────────────────────────────────

#[test]
fn test_log_entry_equality() {
    let e1 = LogEntry {
        timestamp: "00:00:00".into(),
        level: LogLevel::Info,
        message: "test".into(),
        fields: "key=val".into(),
    };
    let e2 = LogEntry {
        timestamp: "00:00:00".into(),
        level: LogLevel::Info,
        message: "test".into(),
        fields: "key=val".into(),
    };
    assert_eq!(e1, e2);
}

#[test]
fn test_log_entry_inequality_level() {
    let e1 = LogEntry {
        timestamp: "00:00:00".into(),
        level: LogLevel::Info,
        message: "test".into(),
        fields: "".into(),
    };
    let e2 = LogEntry {
        timestamp: "00:00:00".into(),
        level: LogLevel::Error,
        message: "test".into(),
        fields: "".into(),
    };
    assert_ne!(e1, e2);
}

#[test]
fn test_log_entry_inequality_message() {
    let e1 = LogEntry {
        timestamp: "00:00:00".into(),
        level: LogLevel::Info,
        message: "msg1".into(),
        fields: "".into(),
    };
    let e2 = LogEntry {
        timestamp: "00:00:00".into(),
        level: LogLevel::Info,
        message: "msg2".into(),
        fields: "".into(),
    };
    assert_ne!(e1, e2);
}

// ── Mock data determinism tests ─────────────────────────────────────────

#[test]
fn test_mock_produces_same_results_for_same_tick_count() {
    let mock = MockMetrics::new();
    let mut app1 = App::new(Duration::from_secs(1));
    let mut app2 = App::new(Duration::from_secs(1));

    for _ in 0..50 {
        mock.tick(&mut app1);
    }
    for _ in 0..50 {
        mock.tick(&mut app2);
    }

    assert_eq!(app1.tick_count, app2.tick_count);
    assert_eq!(app1.requests_total, app2.requests_total);
    assert_eq!(app1.inferences_total, app2.inferences_total);

    for i in 0..5 {
        assert!(
            (app1.stage_latencies[i] - app2.stage_latencies[i]).abs() < 0.001,
            "Stage {} latency mismatch: {} vs {}",
            i,
            app1.stage_latencies[i],
            app2.stage_latencies[i]
        );
    }
}

#[test]
fn test_mock_channel_depths_vary_over_time() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    let mut depths_over_time: Vec<[usize; 4]> = Vec::new();
    for _ in 0..60 {
        mock.tick(&mut app);
        let depths = [
            app.channel_depths[0].current,
            app.channel_depths[1].current,
            app.channel_depths[2].current,
            app.channel_depths[3].current,
        ];
        depths_over_time.push(depths);
    }

    // Each channel should have some variation
    for ch_idx in 0..4 {
        let min_depth = depths_over_time
            .iter()
            .map(|d| d[ch_idx])
            .min()
            .unwrap_or(0);
        let max_depth = depths_over_time
            .iter()
            .map(|d| d[ch_idx])
            .max()
            .unwrap_or(0);
        assert!(
            max_depth > min_depth,
            "Channel {} should vary: min={}, max={}",
            ch_idx,
            min_depth,
            max_depth
        );
    }
}

#[test]
fn test_mock_cost_accumulates_monotonically() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    let mut prev_cost = 0.0;
    for i in 0..200 {
        mock.tick(&mut app);
        assert!(
            app.cost_saved_usd >= prev_cost,
            "Cost should never decrease: tick {} prev={} now={}",
            i,
            prev_cost,
            app.cost_saved_usd
        );
        prev_cost = app.cost_saved_usd;
    }
    assert!(prev_cost > 0.0, "Cost should be positive after 200 ticks");
}

#[test]
fn test_mock_uptime_matches_tick_count() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..42 {
        mock.tick(&mut app);
    }

    assert_eq!(app.uptime_secs, 42);
    assert_eq!(app.tick_count, 42);
}

// ── Stage name and budget consistency ───────────────────────────────────

#[test]
fn test_stage_names_are_uppercase() {
    for name in &STAGE_NAMES {
        assert_eq!(
            *name,
            name.to_uppercase(),
            "Stage name '{}' should be uppercase",
            name
        );
    }
}

#[test]
fn test_stage_names_are_nonempty() {
    for name in &STAGE_NAMES {
        assert!(!name.is_empty());
    }
}

#[test]
fn test_stage_budgets_order() {
    // INFER should have the highest budget
    let max_budget = STAGE_BUDGETS_MS.iter().cloned().fold(0.0_f64, f64::max);
    assert_eq!(
        max_budget, STAGE_BUDGETS_MS[2],
        "INFER should have highest budget"
    );
}

// ── Channel depth app accessor tests ────────────────────────────────────

#[test]
fn test_app_channel_fill_ratio_all_channels() {
    let mut app = App::new(Duration::from_secs(1));
    app.channel_depths[0].current = 256; // 50%
    app.channel_depths[1].current = 512; // 100%
    app.channel_depths[2].current = 0; // 0%
    app.channel_depths[3].current = 128; // 25%

    assert!((app.channel_fill_ratio(0) - 0.5).abs() < 0.01);
    assert_eq!(app.channel_fill_ratio(1), 1.0);
    assert_eq!(app.channel_fill_ratio(2), 0.0);
    assert!((app.channel_fill_ratio(3) - 0.25).abs() < 0.01);
}

#[test]
fn test_app_channel_capacities_match_spec() {
    let app = App::new(Duration::from_secs(1));
    assert_eq!(app.channel_depths[0].capacity, 512);
    assert_eq!(app.channel_depths[1].capacity, 512);
    assert_eq!(app.channel_depths[2].capacity, 1024);
    assert_eq!(app.channel_depths[3].capacity, 512);
}
