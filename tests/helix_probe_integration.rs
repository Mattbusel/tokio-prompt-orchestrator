//! Integration tests for [`HelixPressureProbe`] using a real mock HTTP server.
//!
//! These tests start a real Wiremock server that mimics HelixRouter's
//! `/api/stats` endpoint and verify that the probe correctly:
//!
//! - Extracts `pressure_score`, `dropped`, and `completed` from the JSON.
//! - Computes the combined pressure signal (`max(pressure_score, drop_rate)`).
//! - Feeds the signal into the `TelemetryBus` via `set_external_pressure`.
//! - Resets external pressure to `0.0` when the probe fails repeatedly.

#![cfg(feature = "self-tune")]

use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tokio_prompt_orchestrator::self_tune::{
    helix_probe::{HelixPressureProbe, HelixProbeConfig},
    telemetry_bus::{PipelineCounters, TelemetryBus, TelemetryBusConfig},
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_bus() -> TelemetryBus {
    let counters = PipelineCounters::new();
    TelemetryBus::new(TelemetryBusConfig::default(), counters)
}

fn make_probe_at(base_url: &str, bus: TelemetryBus) -> HelixPressureProbe {
    let cfg = HelixProbeConfig {
        base_url: base_url.to_string(),
        poll_interval: Duration::from_millis(50),
        connect_timeout: Duration::from_millis(500),
        request_timeout: Duration::from_secs(5),
        error_threshold: 3,
    };
    HelixPressureProbe::new(cfg, bus)
}

fn stats_json(pressure: f64, dropped: u64, completed: u64) -> String {
    // Minimal HelixRouter /api/stats body consumed by HelixStats.
    format!(
        r#"{{"pressure_score":{pressure},"dropped":{dropped},"completed":{completed}}}"#
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Probe fetches `/api/stats` and sets external pressure from `pressure_score`.
#[tokio::test]
async fn probe_fetch_pressure_reflects_pressure_score() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/stats"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(stats_json(0.75, 0, 100)),
        )
        .mount(&server)
        .await;

    let bus = make_bus();
    let probe = make_probe_at(&server.uri(), bus.clone());

    // fetch_pressure is the internal async method; call it via the public run path
    // by running the loop for a single tick with a very short interval.
    let bus_clone = bus.clone();
    let handle = tokio::spawn(async move { probe.run().await });

    // Allow at least one poll tick.
    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.abort();
    let _ = handle.await;

    // pressure_score = 0.75, drop_rate = 0/100 = 0.0 → combined = 0.75
    let pressure = bus_clone.external_pressure();
    assert!(
        (pressure - 0.75).abs() < 0.01,
        "expected ~0.75 but got {pressure}"
    );
}

/// When drop_rate > pressure_score, the combined signal uses drop_rate.
#[tokio::test]
async fn probe_combined_pressure_uses_drop_rate_when_higher() {
    let server = MockServer::start().await;
    // pressure_score=0.1, drop_rate=80/(80+20)=0.8 → combined=0.8
    Mock::given(method("GET"))
        .and(path("/api/stats"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(stats_json(0.1, 80, 20)),
        )
        .mount(&server)
        .await;

    let bus = make_bus();
    let probe = make_probe_at(&server.uri(), bus.clone());

    let bus_clone = bus.clone();
    let handle = tokio::spawn(async move { probe.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.abort();
    let _ = handle.await;

    let pressure = bus_clone.external_pressure();
    assert!(
        (pressure - 0.8).abs() < 0.02,
        "expected ~0.8 (drop_rate) but got {pressure}"
    );
}

/// When the server returns HTTP 500, the probe soft-logs and retries.
/// After `error_threshold` consecutive failures it resets external pressure to 0.0.
#[tokio::test]
async fn probe_resets_pressure_on_repeated_failures() {
    let server = MockServer::start().await;
    // Set initial high pressure...
    Mock::given(method("GET"))
        .and(path("/api/stats"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let bus = make_bus();
    // Pre-seed a stale high-pressure value.
    bus.set_external_pressure(0.9);

    let cfg = HelixProbeConfig {
        base_url: server.uri(),
        poll_interval: Duration::from_millis(30),
        connect_timeout: Duration::from_millis(500),
        request_timeout: Duration::from_secs(5),
        error_threshold: 2, // Reset after 2 consecutive failures.
    };
    let probe = HelixPressureProbe::new(cfg, bus.clone());

    let bus_clone = bus.clone();
    let handle = tokio::spawn(async move { probe.run().await });

    // Wait long enough for >= 2 failure ticks.
    tokio::time::sleep(Duration::from_millis(300)).await;
    handle.abort();
    let _ = handle.await;

    let pressure = bus_clone.external_pressure();
    assert_eq!(
        pressure, 0.0,
        "stale pressure should be cleared after repeated failures, got {pressure}"
    );
}

/// With zero completed and zero dropped, pressure_score is used unchanged.
#[tokio::test]
async fn probe_zero_jobs_uses_pressure_score_directly() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/stats"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(stats_json(0.45, 0, 0)),
        )
        .mount(&server)
        .await;

    let bus = make_bus();
    let probe = make_probe_at(&server.uri(), bus.clone());

    let bus_clone = bus.clone();
    let handle = tokio::spawn(async move { probe.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.abort();
    let _ = handle.await;

    let pressure = bus_clone.external_pressure();
    assert!(
        (pressure - 0.45).abs() < 0.01,
        "expected ~0.45 (no jobs, use pressure_score) but got {pressure}"
    );
}

/// Additional unknown fields in the stats JSON are tolerated (forward-compat).
#[tokio::test]
async fn probe_ignores_unknown_json_fields() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/stats"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                r#"{"pressure_score":0.3,"dropped":0,"completed":50,"future_field":42}"#,
            ),
        )
        .mount(&server)
        .await;

    let bus = make_bus();
    let probe = make_probe_at(&server.uri(), bus.clone());

    let bus_clone = bus.clone();
    let handle = tokio::spawn(async move { probe.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.abort();
    let _ = handle.await;

    let pressure = bus_clone.external_pressure();
    assert!(
        (pressure - 0.3).abs() < 0.01,
        "unexpected pressure {pressure} (should ignore extra fields)"
    );
}

/// Probe handles connection refused gracefully — no panic, just a warn log.
#[tokio::test]
async fn probe_connection_refused_does_not_panic() {
    // Port 19999 is unlikely to be in use.
    let bus = make_bus();
    let cfg = HelixProbeConfig {
        base_url: "http://127.0.0.1:19999".to_string(),
        poll_interval: Duration::from_millis(30),
        connect_timeout: Duration::from_millis(100),
        request_timeout: Duration::from_millis(200),
        error_threshold: 1,
    };
    let probe = HelixPressureProbe::new(cfg, bus.clone());

    let handle = tokio::spawn(async move { probe.run().await });
    tokio::time::sleep(Duration::from_millis(250)).await;
    handle.abort();
    let _ = handle.await;

    // No panic — the test itself passing is the assertion.
    // Pressure should have been cleared after threshold failures.
    assert_eq!(
        bus.external_pressure(),
        0.0,
        "pressure should be 0 after connection refused clears stale data"
    );
}
