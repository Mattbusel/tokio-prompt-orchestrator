//! Integration tests for the mock data generator.
//!
//! Verifies that mock metrics produce valid ranges, realistic patterns,
//! and never cause panics over extended operation.

use std::time::Duration;
use tokio_prompt_orchestrator::tui::app::App;
use tokio_prompt_orchestrator::tui::metrics::MockMetrics;

#[test]
fn test_mock_produces_valid_ranges_extended() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for tick in 0..600 {
        mock.tick(&mut app);

        // All latencies positive
        for (i, &lat) in app.stage_latencies.iter().enumerate() {
            assert!(
                lat >= 0.1,
                "Tick {}: stage {} latency below minimum: {}",
                tick,
                i,
                lat
            );
        }

        // Channel depths within capacity
        for ch in &app.channel_depths {
            assert!(
                ch.current <= ch.capacity,
                "Tick {}: channel {} overflow: {}/{}",
                tick,
                ch.name,
                ch.current,
                ch.capacity
            );
        }

        // Health in range
        assert!(
            app.cpu_percent >= 0.0 && app.cpu_percent <= 100.0,
            "Tick {}: CPU out of range: {}",
            tick,
            app.cpu_percent
        );
        assert!(
            app.mem_percent >= 0.0 && app.mem_percent <= 100.0,
            "Tick {}: MEM out of range: {}",
            tick,
            app.mem_percent
        );
    }
}

#[test]
fn test_mock_infer_spikes_occur() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    let mut had_spike = false;
    for _ in 0..200 {
        mock.tick(&mut app);
        if app.stage_latencies[2] > 350.0 {
            had_spike = true;
        }
    }

    assert!(
        had_spike,
        "INFER stage should produce latency spikes above 350ms"
    );
}

#[test]
fn test_mock_throughput_organic_pattern() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..60 {
        mock.tick(&mut app);
    }

    let values: Vec<u64> = app.throughput_history.iter().copied().collect();
    assert_eq!(values.len(), 60);

    // Check that values vary (not constant)
    let min = values.iter().copied().min().unwrap_or(0);
    let max = values.iter().copied().max().unwrap_or(0);
    assert!(
        max > min + 3,
        "Throughput should vary: min={}, max={}",
        min,
        max
    );

    // Check that all values are reasonable
    for &v in &values {
        assert!(v >= 1, "Throughput should be >= 1, got {}", v);
        assert!(v < 100, "Throughput unreasonably high: {}", v);
    }
}

#[test]
fn test_mock_log_variety() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..100 {
        mock.tick(&mut app);
    }

    let mut has_info = false;
    let mut has_warn = false;
    let mut has_error = false;

    for entry in &app.log_entries {
        match entry.level {
            tokio_prompt_orchestrator::tui::app::LogLevel::Info => has_info = true,
            tokio_prompt_orchestrator::tui::app::LogLevel::Warn => has_warn = true,
            tokio_prompt_orchestrator::tui::app::LogLevel::Error => has_error = true,
            tokio_prompt_orchestrator::tui::app::LogLevel::Debug => {}
        }
    }

    assert!(has_info, "Should have INFO log entries");
    assert!(
        has_warn || has_error,
        "Should have at least WARN or ERROR log entries"
    );
}

#[test]
fn test_mock_cost_accumulates() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    mock.tick(&mut app);
    let cost_1 = app.cost_saved_usd;

    for _ in 0..9 {
        mock.tick(&mut app);
    }
    let cost_10 = app.cost_saved_usd;

    assert!(cost_10 > cost_1, "Cost should accumulate over time");
    assert!(cost_10 > 0.0, "Cost should be positive");
}

#[test]
fn test_mock_default_new_equivalent() {
    let mock1 = MockMetrics::new();
    let mock2 = MockMetrics::default();

    let mut app1 = App::new(Duration::from_secs(1));
    let mut app2 = App::new(Duration::from_secs(1));

    mock1.tick(&mut app1);
    mock2.tick(&mut app2);

    // Both should produce identical results
    assert_eq!(app1.tick_count, app2.tick_count);
    assert_eq!(app1.requests_total, app2.requests_total);
}
