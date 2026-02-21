//! Integration tests for the TUI dashboard module.
//!
//! These tests verify cross-module interactions: app state updates from mock
//! metrics, circuit breaker cycling through all states, log rotation bounds,
//! and end-to-end data flow from metrics source to app state.

#[cfg(feature = "tui")]
mod app_state;
#[cfg(feature = "tui")]
mod circuit_cycling;
#[cfg(feature = "tui")]
mod log_rotation;
#[cfg(feature = "tui")]
mod mock_data;
#[cfg(feature = "tui")]
mod widget_tests;
