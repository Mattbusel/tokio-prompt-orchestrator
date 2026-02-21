//! Integration tests for App state transitions and update logic.

use std::time::Duration;
use tokio_prompt_orchestrator::tui::app::{
    App, CircuitState, LOG_ENTRIES_CAP, STAGE_BUDGETS_MS, STAGE_NAMES, THROUGHPUT_HISTORY_CAP,
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
    assert!(savings > 60.0, "Expected >60% savings, got {:.1}%", savings);
    assert!(savings < 95.0, "Expected <95% savings, got {:.1}%", savings);
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

#[test]
fn test_scroll_up_and_down_integration() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..30 {
        mock.tick(&mut app);
    }

    assert_eq!(app.log_scroll_offset, 0);
    apply_event(&mut app, InputEvent::ScrollUp);
    assert!(app.log_scroll_offset > 0);
    let offset = app.log_scroll_offset;
    apply_event(&mut app, InputEvent::ScrollDown);
    assert_eq!(app.log_scroll_offset, offset - 1);
}

#[test]
fn test_scroll_offset_bounded_by_log_size() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..10 {
        mock.tick(&mut app);
    }

    // Try scrolling way past the log size
    for _ in 0..200 {
        apply_event(&mut app, InputEvent::ScrollUp);
    }

    assert!(
        app.log_scroll_offset < 200,
        "Scroll offset should be bounded by log size, got {}",
        app.log_scroll_offset
    );
}

#[test]
fn test_story_warmup_phase_characteristics() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..20 {
        mock.tick(&mut app);
    }

    // All circuits closed during warmup
    for cb in &app.circuit_breakers {
        assert_eq!(
            cb.state,
            CircuitState::Closed,
            "CB {} should be closed in warmup",
            cb.name
        );
    }

    // INFER latency should be moderate
    assert!(
        app.stage_latencies[2] < 300.0,
        "INFER should be low in warmup"
    );
}

#[test]
fn test_story_failure_phase_high_latency() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    // Advance to failure phase (tick 46-60)
    for _ in 0..50 {
        mock.tick(&mut app);
    }

    // INFER should be high
    assert!(
        app.stage_latencies[2] > 350.0,
        "INFER should spike during failure phase, got {}",
        app.stage_latencies[2]
    );
}

#[test]
fn test_story_recovery_clears_circuit() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    // Advance to recovery phase (tick 76-90)
    for _ in 0..80 {
        mock.tick(&mut app);
    }

    // llama.cpp should be back to closed
    assert_eq!(
        app.circuit_breakers[2].state,
        CircuitState::Closed,
        "llama.cpp should be Closed during recovery"
    );
}

#[test]
fn test_story_steady_state_moderate_metrics() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    // Advance to steady state (tick 91-120)
    for _ in 0..100 {
        mock.tick(&mut app);
    }

    // All circuits closed
    for cb in &app.circuit_breakers {
        assert_eq!(
            cb.state,
            CircuitState::Closed,
            "CB {} should be closed in steady state",
            cb.name
        );
    }

    // CPU should be moderate
    assert!(
        app.cpu_percent < 80.0,
        "CPU should be moderate in steady state"
    );
}

#[test]
fn test_reset_clears_scroll_offset_integration() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..20 {
        mock.tick(&mut app);
    }
    apply_event(&mut app, InputEvent::ScrollUp);
    apply_event(&mut app, InputEvent::ScrollUp);
    assert!(app.log_scroll_offset > 0);

    apply_event(&mut app, InputEvent::Reset);
    assert_eq!(app.log_scroll_offset, 0);
}
