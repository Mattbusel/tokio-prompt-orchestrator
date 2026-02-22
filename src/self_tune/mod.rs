//! Self-tuning subsystem - telemetry, PID control, experiments, anomaly detection, cost, snapshots.
//! All items in this module are gated behind the self-tune feature flag.

pub mod actuator;
pub mod anomaly;
pub mod controller;
pub mod cost;
pub mod experiment;
pub mod helix_probe;
pub mod snapshot;
pub mod telemetry_bus;
#[cfg(feature = "self-tune")]
pub mod wiring;
