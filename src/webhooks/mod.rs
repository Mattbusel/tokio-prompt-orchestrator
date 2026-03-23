#![allow(dead_code)]
//! # Module: Webhook Notifications
//!
//! ## Responsibility
//! Delivers structured JSON event notifications to one or more configured
//! webhook URLs when important pipeline events occur:
//!
//! | Event | Trigger |
//! |-------|---------|
//! | [`WebhookEvent::CircuitBreakerStateChange`] | Circuit breaker transitions CLOSED→OPEN, OPEN→HALF-OPEN, etc. |
//! | [`WebhookEvent::BudgetThresholdExceeded`] | Spend crosses a configured threshold |
//! | [`WebhookEvent::AnomalyDetected`] | Anomaly detector raises an alert |
//! | [`WebhookEvent::ThroughputDrop`] | Pipeline throughput drops >50% compared to recent baseline |
//!
//! Each event is delivered with a context JSON payload via HTTP POST.
//!
//! ## Delivery guarantees
//! - At-most-once: no retry on failure (logged as warning)
//! - Non-blocking: delivery is async; the caller never blocks
//! - Circuit-breaker aware: a per-webhook soft-disable prevents thundering
//!   herd when a webhook URL is consistently unreachable (≥5 consecutive
//!   failures causes a 60-second backoff)
//!
//! ## Usage
//!
//! ```ignore
//! let dispatcher = WebhookDispatcher::new(vec![
//!     WebhookConfig { url: "https://hooks.example.com/pipeline".into(), ..Default::default() },
//! ])?;
//!
//! dispatcher.dispatch(WebhookEvent::CircuitBreakerStateChange {
//!     worker: "openai".into(),
//!     from: CircuitBreakerState::Closed,
//!     to: CircuitBreakerState::Open,
//!     failure_count: 5,
//! }).await;
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Errors produced by the webhook dispatcher.
#[derive(Debug, Error)]
pub enum WebhookError {
    /// A webhook URL is syntactically invalid.
    #[error("invalid webhook URL: {0}")]
    InvalidUrl(String),
    /// An HTTP delivery failed.
    #[error("delivery failed to {url}: {reason}")]
    DeliveryFailed { url: String, reason: String },
    /// JSON serialisation of the payload failed.
    #[error("serialisation error: {0}")]
    Serialisation(String),
    /// Internal mutex was poisoned.
    #[error("internal lock poisoned")]
    LockPoisoned,
}

// ─── Circuit breaker state ───────────────────────────────────────────────────

/// State of a circuit breaker (mirrored here to avoid importing enhanced::).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CircuitBreakerState {
    /// Requests flowing normally.
    Closed,
    /// Testing recovery with probe requests.
    HalfOpen,
    /// Fast-failing all requests.
    Open,
}

impl std::fmt::Display for CircuitBreakerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CircuitBreakerState::Closed => write!(f, "CLOSED"),
            CircuitBreakerState::HalfOpen => write!(f, "HALF-OPEN"),
            CircuitBreakerState::Open => write!(f, "OPEN"),
        }
    }
}

// ─── Events ───────────────────────────────────────────────────────────────────

/// A pipeline event that triggers webhook delivery.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum WebhookEvent {
    /// A circuit breaker changed state.
    CircuitBreakerStateChange {
        /// Worker or service whose circuit breaker changed.
        worker: String,
        /// Previous state.
        from: CircuitBreakerState,
        /// New state.
        to: CircuitBreakerState,
        /// Number of consecutive failures that triggered the transition.
        failure_count: u32,
    },
    /// Spend crossed a configured budget threshold.
    BudgetThresholdExceeded {
        /// Budget name or identifier.
        budget_name: String,
        /// Threshold that was crossed in USD.
        threshold_usd: f64,
        /// Current spend in USD.
        current_spend_usd: f64,
        /// Percentage of budget consumed.
        percent_used: f64,
    },
    /// An anomaly detector raised an alert.
    AnomalyDetected {
        /// Name of the metric where the anomaly was detected.
        metric: String,
        /// The anomalous value.
        value: f64,
        /// Z-score or CUSUM magnitude.
        severity: f64,
        /// Human-readable description.
        description: String,
    },
    /// Pipeline throughput dropped by more than `drop_percent`%.
    ThroughputDrop {
        /// Requests per second over the last sampling window.
        current_rps: f64,
        /// Baseline requests per second (rolling average).
        baseline_rps: f64,
        /// Percentage drop (0–100).
        drop_percent: f64,
    },
}

impl WebhookEvent {
    /// Short human-readable event name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            WebhookEvent::CircuitBreakerStateChange { .. } => "circuit_breaker_state_change",
            WebhookEvent::BudgetThresholdExceeded { .. } => "budget_threshold_exceeded",
            WebhookEvent::AnomalyDetected { .. } => "anomaly_detected",
            WebhookEvent::ThroughputDrop { .. } => "throughput_drop",
        }
    }
}

// ─── Webhook payload ─────────────────────────────────────────────────────────

/// The JSON body delivered to webhook endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookPayload {
    /// ISO-8601 timestamp of the event.
    pub timestamp: String,
    /// Service name.
    pub service: String,
    /// The event itself (serialised as a nested object).
    pub event: WebhookEvent,
    /// Arbitrary context metadata.
    pub context: HashMap<String, serde_json::Value>,
}

impl WebhookPayload {
    /// Build a payload for the given event.
    #[must_use]
    pub fn new(event: WebhookEvent, service: &str) -> Self {
        Self {
            timestamp: chrono::Utc::now().to_rfc3339(),
            service: service.to_string(),
            event,
            context: HashMap::new(),
        }
    }

    /// Add a context key.
    #[must_use]
    pub fn with_context(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.context.insert(key.into(), value);
        self
    }
}

// ─── Webhook configuration ───────────────────────────────────────────────────

/// Configuration for a single webhook endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// HTTP URL to POST events to.
    pub url: String,
    /// Optional Bearer token added to the `Authorization` header.
    pub bearer_token: Option<String>,
    /// Optional additional headers.
    pub extra_headers: HashMap<String, String>,
    /// Delivery timeout.
    pub timeout: Duration,
    /// Which event types to deliver (empty = all).
    pub filter_events: Vec<String>,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            bearer_token: None,
            extra_headers: HashMap::new(),
            timeout: Duration::from_secs(5),
            filter_events: vec![],
        }
    }
}

// ─── Per-webhook backoff state ────────────────────────────────────────────────

#[derive(Debug)]
struct WebhookState {
    config: WebhookConfig,
    consecutive_failures: u32,
    backoff_until: Option<Instant>,
}

impl WebhookState {
    fn new(config: WebhookConfig) -> Self {
        Self {
            config,
            consecutive_failures: 0,
            backoff_until: None,
        }
    }

    fn is_backing_off(&self) -> bool {
        match self.backoff_until {
            Some(until) => Instant::now() < until,
            None => false,
        }
    }

    fn record_failure(&mut self) {
        self.consecutive_failures += 1;
        if self.consecutive_failures >= 5 {
            let backoff = Duration::from_secs(60);
            self.backoff_until = Some(Instant::now() + backoff);
            warn!(
                url = %self.config.url,
                failures = self.consecutive_failures,
                "webhook backing off for 60s after consecutive failures"
            );
        }
    }

    fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.backoff_until = None;
    }

    fn passes_filter(&self, event: &WebhookEvent) -> bool {
        if self.config.filter_events.is_empty() {
            return true;
        }
        self.config.filter_events.iter().any(|f| f == event.name())
    }
}

// ─── Dispatcher ───────────────────────────────────────────────────────────────

/// Sends webhook events to all configured endpoints.
pub struct WebhookDispatcher {
    service_name: String,
    states: Arc<Mutex<Vec<WebhookState>>>,
    http_client: reqwest::Client,
}

impl WebhookDispatcher {
    /// Create a new dispatcher.
    ///
    /// # Errors
    /// Returns [`WebhookError::InvalidUrl`] if any configured URL is empty.
    pub fn new(configs: Vec<WebhookConfig>) -> Result<Arc<Self>, WebhookError> {
        Self::with_service_name(configs, "tokio-prompt-orchestrator")
    }

    /// Create with a custom service name.
    ///
    /// # Errors
    /// Returns [`WebhookError::InvalidUrl`] if any configured URL is empty.
    pub fn with_service_name(
        configs: Vec<WebhookConfig>,
        service_name: &str,
    ) -> Result<Arc<Self>, WebhookError> {
        for cfg in &configs {
            if cfg.url.is_empty() {
                return Err(WebhookError::InvalidUrl("URL must not be empty".to_string()));
            }
        }

        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| WebhookError::InvalidUrl(e.to_string()))?;

        let states = configs
            .into_iter()
            .map(WebhookState::new)
            .collect::<Vec<_>>();

        Ok(Arc::new(Self {
            service_name: service_name.to_string(),
            states: Arc::new(Mutex::new(states)),
            http_client,
        }))
    }

    /// Dispatch an event to all configured (non-backed-off) webhooks.
    ///
    /// Delivery is attempted for each webhook sequentially. Failures are
    /// logged but do not propagate to the caller.
    pub async fn dispatch(&self, event: WebhookEvent) {
        let payload = WebhookPayload::new(event, &self.service_name);
        let body = match serde_json::to_string(&payload) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "failed to serialise webhook payload; skipping dispatch");
                return;
            }
        };

        // Collect the configs we need to deliver to (under lock, then release)
        let targets: Vec<(usize, String, Option<String>, HashMap<String, String>, Duration)> = {
            let Ok(states) = self.states.lock() else {
                warn!("webhook dispatcher lock poisoned");
                return;
            };
            states
                .iter()
                .enumerate()
                .filter(|(_, s)| !s.is_backing_off() && s.passes_filter(&payload.event))
                .map(|(i, s)| {
                    (
                        i,
                        s.config.url.clone(),
                        s.config.bearer_token.clone(),
                        s.config.extra_headers.clone(),
                        s.config.timeout,
                    )
                })
                .collect()
        };

        for (idx, url, bearer, extra_headers, timeout) in targets {
            let result = self
                .deliver(&url, &bearer, &extra_headers, timeout, &body)
                .await;
            let Ok(mut states) = self.states.lock() else {
                continue;
            };
            match result {
                Ok(()) => {
                    debug!(url = %url, event = %payload.event.name(), "webhook delivered");
                    if let Some(s) = states.get_mut(idx) {
                        s.record_success();
                    }
                }
                Err(e) => {
                    warn!(url = %url, error = %e, "webhook delivery failed");
                    if let Some(s) = states.get_mut(idx) {
                        s.record_failure();
                    }
                }
            }
        }

        info!(
            event = %payload.event.name(),
            service = %self.service_name,
            "webhook dispatch complete"
        );
    }

    async fn deliver(
        &self,
        url: &str,
        bearer: &Option<String>,
        extra_headers: &HashMap<String, String>,
        timeout: Duration,
        body: &str,
    ) -> Result<(), WebhookError> {
        let mut req = self
            .http_client
            .post(url)
            .timeout(timeout)
            .header("Content-Type", "application/json")
            .header("User-Agent", "tokio-prompt-orchestrator/1.0")
            .body(body.to_string());

        if let Some(token) = bearer {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        for (k, v) in extra_headers {
            req = req.header(k.as_str(), v.as_str());
        }

        let resp = req.send().await.map_err(|e| WebhookError::DeliveryFailed {
            url: url.to_string(),
            reason: e.to_string(),
        })?;

        if !resp.status().is_success() {
            return Err(WebhookError::DeliveryFailed {
                url: url.to_string(),
                reason: format!("HTTP {}", resp.status()),
            });
        }
        Ok(())
    }

    /// Return the number of configured webhooks currently in backoff.
    pub fn backed_off_count(&self) -> usize {
        let Ok(states) = self.states.lock() else {
            return 0;
        };
        states.iter().filter(|s| s.is_backing_off()).count()
    }

    /// Return the total number of configured webhooks.
    pub fn webhook_count(&self) -> usize {
        let Ok(states) = self.states.lock() else {
            return 0;
        };
        states.len()
    }
}

// ─── Convenience constructors for common events ────────────────────────────────

/// Build a [`WebhookEvent::CircuitBreakerStateChange`] event.
#[must_use]
pub fn circuit_breaker_event(
    worker: impl Into<String>,
    from: CircuitBreakerState,
    to: CircuitBreakerState,
    failure_count: u32,
) -> WebhookEvent {
    WebhookEvent::CircuitBreakerStateChange {
        worker: worker.into(),
        from,
        to,
        failure_count,
    }
}

/// Build a [`WebhookEvent::BudgetThresholdExceeded`] event.
#[must_use]
pub fn budget_exceeded_event(
    budget_name: impl Into<String>,
    threshold_usd: f64,
    current_spend_usd: f64,
) -> WebhookEvent {
    let percent_used = if threshold_usd > 0.0 {
        (current_spend_usd / threshold_usd * 100.0).min(999.9)
    } else {
        0.0
    };
    WebhookEvent::BudgetThresholdExceeded {
        budget_name: budget_name.into(),
        threshold_usd,
        current_spend_usd,
        percent_used,
    }
}

/// Build a [`WebhookEvent::ThroughputDrop`] event.
///
/// Returns `None` if the drop is less than `min_drop_percent`.
#[must_use]
pub fn throughput_drop_event(
    current_rps: f64,
    baseline_rps: f64,
    min_drop_percent: f64,
) -> Option<WebhookEvent> {
    if baseline_rps <= 0.0 {
        return None;
    }
    let drop_percent = ((baseline_rps - current_rps) / baseline_rps * 100.0).max(0.0);
    if drop_percent < min_drop_percent {
        return None;
    }
    Some(WebhookEvent::ThroughputDrop {
        current_rps,
        baseline_rps,
        drop_percent,
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_payload_serialises() {
        let event = circuit_breaker_event(
            "openai",
            CircuitBreakerState::Closed,
            CircuitBreakerState::Open,
            5,
        );
        let payload = WebhookPayload::new(event, "test-service");
        let json = serde_json::to_string(&payload).expect("should serialise");
        assert!(json.contains("circuit_breaker_state_change"));
        assert!(json.contains("openai"));
    }

    #[test]
    fn event_name_helpers() {
        assert_eq!(
            circuit_breaker_event(
                "w",
                CircuitBreakerState::Open,
                CircuitBreakerState::HalfOpen,
                3
            )
            .name(),
            "circuit_breaker_state_change"
        );
        assert_eq!(
            budget_exceeded_event("monthly", 100.0, 110.0).name(),
            "budget_threshold_exceeded"
        );
    }

    #[test]
    fn throughput_drop_event_threshold() {
        assert!(throughput_drop_event(100.0, 100.0, 50.0).is_none());
        assert!(throughput_drop_event(40.0, 100.0, 50.0).is_some());
        assert!(throughput_drop_event(49.0, 100.0, 50.0).is_none());
        let evt = throughput_drop_event(20.0, 100.0, 50.0).unwrap();
        if let WebhookEvent::ThroughputDrop { drop_percent, .. } = evt {
            assert!((drop_percent - 80.0).abs() < 0.01);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn dispatcher_invalid_url_rejected() {
        let result = WebhookDispatcher::new(vec![WebhookConfig {
            url: String::new(),
            ..Default::default()
        }]);
        assert!(result.is_err());
    }

    #[test]
    fn dispatcher_no_webhooks() {
        let dispatcher = WebhookDispatcher::new(vec![]).expect("empty ok");
        assert_eq!(dispatcher.webhook_count(), 0);
        assert_eq!(dispatcher.backed_off_count(), 0);
    }

    #[test]
    fn webhook_state_backoff_after_5_failures() {
        let mut state = WebhookState::new(WebhookConfig {
            url: "http://example.com".into(),
            ..Default::default()
        });
        assert!(!state.is_backing_off());
        for _ in 0..5 {
            state.record_failure();
        }
        assert!(state.is_backing_off());
        state.record_success();
        assert!(!state.is_backing_off());
    }

    #[test]
    fn webhook_filter_passes_matching() {
        let state = WebhookState::new(WebhookConfig {
            url: "http://example.com".into(),
            filter_events: vec!["anomaly_detected".into()],
            ..Default::default()
        });
        let anomaly_event = WebhookEvent::AnomalyDetected {
            metric: "latency".into(),
            value: 99.9,
            severity: 4.5,
            description: "high latency".into(),
        };
        assert!(state.passes_filter(&anomaly_event));

        let cb_event = circuit_breaker_event(
            "w",
            CircuitBreakerState::Closed,
            CircuitBreakerState::Open,
            1,
        );
        assert!(!state.passes_filter(&cb_event));
    }

    #[test]
    fn budget_exceeded_percent_computed() {
        let evt = budget_exceeded_event("monthly", 100.0, 75.0);
        if let WebhookEvent::BudgetThresholdExceeded { percent_used, .. } = evt {
            assert!((percent_used - 75.0).abs() < 0.01);
        } else {
            panic!("wrong variant");
        }
    }
}
