//! Integration tests for circuit breaker state cycling.
//!
//! Verifies that the mock story cycles llama.cpp through all circuit breaker
//! states (Closed → Open → HalfOpen → Closed) within the 2-minute story.

use std::time::Duration;
use tokio_prompt_orchestrator::tui::app::{App, CircuitState};
use tokio_prompt_orchestrator::tui::metrics::MockMetrics;

#[test]
fn test_llama_full_cycle_within_story() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    let mut transitions = Vec::new();
    let mut last_state = CircuitState::Closed;

    for _ in 0..120 {
        mock.tick(&mut app);
        let current = app.circuit_breakers[2].state;
        if current != last_state {
            transitions.push((app.tick_count, last_state, current));
            last_state = current;
        }
    }

    // Should have at least 3 transitions (Closed→Open, Open→HalfOpen, HalfOpen→Closed)
    assert!(
        transitions.len() >= 3,
        "Expected at least 3 transitions, got {}: {:?}",
        transitions.len(),
        transitions
    );
}

#[test]
fn test_llama_all_states_observed() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    let mut saw_closed = false;
    let mut saw_open = false;
    let mut saw_half_open = false;

    for _ in 0..120 {
        mock.tick(&mut app);
        match app.circuit_breakers[2].state {
            CircuitState::Closed => saw_closed = true,
            CircuitState::Open => saw_open = true,
            CircuitState::HalfOpen => saw_half_open = true,
        }
    }

    assert!(saw_closed, "Should observe Closed state for llama.cpp");
    assert!(saw_open, "Should observe Open state for llama.cpp");
    assert!(saw_half_open, "Should observe HalfOpen state for llama.cpp");
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
fn test_anthropic_always_healthy() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    for _ in 0..300 {
        mock.tick(&mut app);
        assert_eq!(
            app.circuit_breakers[1].state,
            CircuitState::Closed,
            "anthropic should always be Closed in story mode"
        );
    }
}

#[test]
fn test_story_loops_after_120_seconds() {
    let mock = MockMetrics::new();
    let mut app = App::new(Duration::from_secs(1));

    // Run past one full cycle
    for _ in 0..130 {
        mock.tick(&mut app);
    }

    // Should be back in warmup: all circuits closed
    for cb in &app.circuit_breakers {
        assert_eq!(
            cb.state,
            CircuitState::Closed,
            "CB {} should be Closed after story loops",
            cb.name
        );
    }
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
