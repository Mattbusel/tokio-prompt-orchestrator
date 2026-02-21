//! # Module: TUI Dashboard
//!
//! ## Responsibility
//! Provides a production-grade ASCII terminal dashboard for the tokio-prompt-orchestrator
//! using Ratatui. This is the primary demo and monitoring interface, rendering pipeline
//! flow, channel depths, circuit breaker states, deduplication stats, throughput
//! sparklines, system health, and a log tail in real-time.
//!
//! ## Guarantees
//! - No panics in any rendering or update path
//! - Clean terminal restore on exit, including on panic
//! - Graceful resize handling down to 100x40 minimum
//! - 10fps rendering with 1hz data updates
//!
//! ## NOT Responsible For
//! - Orchestrator pipeline execution (read-only monitoring)
//! - Persistent storage of metrics (ephemeral display only)
//! - Network protocol implementation (delegates to metrics module)

pub mod app;
pub mod events;
pub mod metrics;
pub mod ui;
pub mod widgets;
