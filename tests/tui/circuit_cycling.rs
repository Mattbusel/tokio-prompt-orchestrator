//! Integration tests for circuit breaker state cycling.
//!
//! Verifies that the mock data generator cycles circuit breakers through
//! all states (Closed → Open → HalfOpen → Closed) within the expected timeframes.

use std::time::Duration;
use tokio_prompt_orchestrator::tui::app::{App, CircuitState};
use tokio_prompt_orchestrator::tui::metrics::MockMetrics;

#[test]
fn test_anthropic_full_cycle_within_ninety_seconds() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    let mut transitions = Vec::new();
    let mut last_state = CircuitState::Closed;

    for _ in 0..95 {
        mock.tick(&mut app);
        let current = app.circuit_breakers[1].state;
        if current != last_state {
            transitions.push((app.tick_count, last_state, current));
            last_state = current;
        }
    }

    // Should have at least 2 transitions (Closed→Open, Open→HalfOpen)
    assert!(
        transitions.len() >= 2,
        "Expected at least 2 transitions, got {}: {:?}",
        transitions.len(),
        transitions
    );
}

#[test]
fn test_anthropic_all_states_observed() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    let mut saw_closed = false;
    let mut saw_open = false;
    let mut saw_half_open = false;

    for _ in 0..100 {
        mock.tick(&mut app);
        match app.circuit_breakers[1].state {
            CircuitState::Closed => saw_closed = true,
            CircuitState::Open => saw_open = true,
            CircuitState::HalfOpen => saw_half_open = true,
        }
    }

    assert!(saw_closed, "Should observe Closed state");
    assert!(saw_open, "Should observe Open state");
    assert!(saw_half_open, "Should observe HalfOpen state");
}

#[test]
fn test_openai_always_healthy() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..300 {
        mock.tick(&mut app);
        assert_eq!(
            app.circuit_breakers[0].state,
            CircuitState::Closed,
            "openai should always be Closed"
        );
    }
}

#[test]
fn test_llama_cycles_with_offset() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    let mut saw_open = false;
    let mut saw_half_open = false;

    for _ in 0..150 {
        mock.tick(&mut app);
        match app.circuit_breakers[2].state {
            CircuitState::Open => saw_open = true,
            CircuitState::HalfOpen => saw_half_open = true,
            CircuitState::Closed => {}
        }
    }

    assert!(saw_open, "llama.cpp should enter Open state");
    assert!(saw_half_open, "llama.cpp should enter HalfOpen state");
}

#[test]
fn test_circuit_detail_string_populated() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..100 {
        mock.tick(&mut app);
        for cb in &app.circuit_breakers {
            assert!(
                !cb.detail.is_empty(),
                "CB {} detail should not be empty",
                cb.name
            );
        }
    }
}

#[test]
fn test_circuit_breaker_count_stable() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..100 {
        mock.tick(&mut app);
        assert_eq!(
            app.circuit_breakers.len(),
            3,
            "Should always have exactly 3 circuit breakers"
        );
    }
}
