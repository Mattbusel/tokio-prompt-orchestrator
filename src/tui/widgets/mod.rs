//! # Module: TUI Widgets
//!
//! ## Responsibility
//! Individual rendering widgets for each dashboard section. Each widget is a pure
//! function that takes app state and a layout rect, and renders into a frame.
//!
//! ## Guarantees
//! - All widgets handle zero-data gracefully (empty state rendering)
//! - No widget panics on any input range
//! - Color-coded thresholds are consistent across all bar-style widgets

pub mod channels;
pub mod circuit;
pub mod dedup;
pub mod health;
pub mod log;
pub mod pipeline;
pub mod sparkline;
