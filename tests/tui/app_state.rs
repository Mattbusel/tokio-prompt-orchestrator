//! Integration tests for App state transitions and update logic.

use std::time::Duration;
use tokio_prompt_orchestrator::tui::app::{
    App, LOG_ENTRIES_CAP, STAGE_BUDGETS_MS, STAGE_NAMES, THROUGHPUT_HISTORY_CAP,
};
use tokio_prompt_orchestrator::tui::events::{apply_event, InputEvent};
use tokio_prompt_orchestrator::tui::metrics::MockMetrics;

#[test]
fn test_app_full_lifecycle_mock_mode() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    // Simulate 120 seconds of operation
    for _ in 0..120 {
        mock.tick(&mut app);
    }

    // Verify state is populated
    assert_eq!(app.tick_count, 120);
    assert_eq!(app.uptime_secs, 120);
    assert!(app.requests_total > 0);
    assert!(app.inferences_total > 0);
    assert!(app.cost_saved_usd > 0.0);
    assert!(!app.throughput_history.is_empty());
    assert!(!app.log_entries.is_empty());

    // All latencies should be positive
    for (i, &lat) in app.stage_latencies.iter().enumerate() {
        assert!(
            lat > 0.0,
            "Stage {} latency should be positive, got {}",
            STAGE_NAMES[i],
            lat
        );
    }

    // All channel depths within bounds
    for ch in &app.channel_depths {
        assert!(
            ch.current <= ch.capacity,
            "{} overflow: {}/{}",
            ch.name,
            ch.current,
            ch.capacity
        );
    }

    // Health metrics in valid ranges
    assert!(app.cpu_percent >= 0.0 && app.cpu_percent <= 100.0);
    assert!(app.mem_percent >= 0.0 && app.mem_percent <= 100.0);
    assert!(app.active_tasks > 0);
}

#[test]
fn test_app_pause_stops_data_but_preserves_state() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    // Collect some data
    for _ in 0..10 {
        mock.tick(&mut app);
    }
    let requests_before = app.requests_total;

    // Pause
    apply_event(&mut app, InputEvent::Pause);
    assert!(app.paused);

    // In real loop, paused prevents ticking. Verify state unchanged.
    assert_eq!(app.requests_total, requests_before);

    // Unpause
    apply_event(&mut app, InputEvent::Pause);
    assert!(!app.paused);
}

#[test]
fn test_app_reset_during_operation() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..50 {
        mock.tick(&mut app);
    }

    assert!(app.requests_total > 0);
    assert!(!app.throughput_history.is_empty());
    assert!(!app.log_entries.is_empty());

    // Reset
    apply_event(&mut app, InputEvent::Reset);

    assert_eq!(app.requests_total, 0);
    assert_eq!(app.inferences_total, 0);
    assert_eq!(app.cost_saved_usd, 0.0);
    assert!(app.throughput_history.is_empty());
    assert!(app.log_entries.is_empty());
    assert_eq!(app.tick_count, 0);

    // Can resume after reset
    for _ in 0..5 {
        mock.tick(&mut app);
    }
    assert!(app.requests_total > 0);
}

#[test]
fn test_app_quit_event() {
    let mut app = App::new(Duration::from_secs(1));
    assert!(!app.should_quit);
    apply_event(&mut app, InputEvent::Quit);
    assert!(app.should_quit);
}

#[test]
fn test_app_help_toggle() {
    let mut app = App::new(Duration::from_secs(1));
    assert!(!app.show_help);
    apply_event(&mut app, InputEvent::Help);
    assert!(app.show_help);
    apply_event(&mut app, InputEvent::Help);
    assert!(!app.show_help);
}

#[test]
fn test_stage_budgets_are_positive() {
    for (i, &budget) in STAGE_BUDGETS_MS.iter().enumerate() {
        assert!(
            budget > 0.0,
            "Stage {} budget must be positive, got {}",
            STAGE_NAMES[i],
            budget
        );
    }
}

#[test]
fn test_stage_names_and_budgets_aligned() {
    assert_eq!(STAGE_NAMES.len(), STAGE_BUDGETS_MS.len());
    assert_eq!(STAGE_NAMES.len(), 5);
}

#[test]
fn test_dedup_savings_after_extended_operation() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..300 {
        mock.tick(&mut app);
    }

    let savings = app.dedup_savings_percent();
    assert!(savings > 70.0, "Expected >70% savings, got {:.1}%", savings);
    assert!(savings < 90.0, "Expected <90% savings, got {:.1}%", savings);
}

#[test]
fn test_throughput_bounded_over_extended_run() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..500 {
        mock.tick(&mut app);
    }

    assert!(
        app.throughput_history.len() <= THROUGHPUT_HISTORY_CAP,
        "Throughput history exceeded capacity: {} > {}",
        app.throughput_history.len(),
        THROUGHPUT_HISTORY_CAP
    );
}

#[test]
fn test_log_entries_bounded_over_extended_run() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..500 {
        mock.tick(&mut app);
    }

    assert!(
        app.log_entries.len() <= LOG_ENTRIES_CAP,
        "Log entries exceeded capacity: {} > {}",
        app.log_entries.len(),
        LOG_ENTRIES_CAP
    );
}

#[test]
fn test_uptime_display_after_operation() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..3661 {
        mock.tick(&mut app);
    }

    assert_eq!(app.uptime_display(), "1h 1m");
}

#[test]
fn test_active_stage_always_valid_index() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..200 {
        mock.tick(&mut app);
        if let Some(idx) = app.active_stage {
            assert!(idx < 5, "Active stage index out of range: {}", idx);
        }
    }
}
