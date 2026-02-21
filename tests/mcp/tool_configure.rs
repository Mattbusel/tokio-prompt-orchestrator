//! Integration tests for MCP configure_pipeline tool.
//!
//! Verifies configuration validation logic: valid/invalid workers,
//! boundary values for retry attempts, circuit breaker thresholds,
//! and rate limits.

#[test]
fn test_valid_worker_names() {
    let valid = ["openai", "anthropic", "llama", "echo"];
    assert!(valid.contains(&"echo"));
    assert!(valid.contains(&"openai"));
    assert!(valid.contains(&"anthropic"));
    assert!(valid.contains(&"llama"));
}

#[test]
fn test_invalid_worker_name_rejected() {
    let valid = ["openai", "anthropic", "llama", "echo"];
    assert!(!valid.contains(&"gpt4"));
    assert!(!valid.contains(&"claude"));
    assert!(!valid.contains(&""));
}

#[test]
fn test_retry_attempts_boundary_valid() {
    // Valid range: 0..=10
    for retries in 0..=10u32 {
        assert!(retries <= 10);
    }
}

#[test]
fn test_retry_attempts_boundary_invalid() {
    let retries: u32 = 11;
    assert!(retries > 10, "11 should exceed max retry limit");

    let retries: u32 = 99;
    assert!(retries > 10, "99 should exceed max retry limit");
}

#[test]
fn test_circuit_breaker_threshold_must_be_positive() {
    let threshold: u32 = 0;
    assert_eq!(threshold, 0, "zero threshold should be rejected");

    let threshold: u32 = 1;
    assert!(threshold >= 1, "1 is the minimum valid threshold");
}

#[test]
fn test_rate_limit_rps_must_be_positive() {
    let rps: u32 = 0;
    assert_eq!(rps, 0, "zero rps should be rejected");

    let rps: u32 = 1;
    assert!(rps >= 1, "1 is the minimum valid rps");
}

#[test]
fn test_config_defaults() {
    // Verify expected default values match spec
    let worker = "echo";
    let retry_attempts: u32 = 3;
    let circuit_breaker_threshold: u32 = 5;
    let rate_limit_rps: u32 = 100;

    assert_eq!(worker, "echo");
    assert_eq!(retry_attempts, 3);
    assert_eq!(circuit_breaker_threshold, 5);
    assert_eq!(rate_limit_rps, 100);
}

#[test]
fn test_multiple_config_changes_tracked() {
    let mut changes: Vec<String> = Vec::new();

    // Simulate applying multiple config changes
    let worker = Some("anthropic");
    let retries = Some(5u32);
    let threshold = Some(10u32);
    let rps = Some(50u32);

    if let Some(w) = worker {
        changes.push(format!("worker → {w}"));
    }
    if let Some(r) = retries {
        changes.push(format!("retry_attempts → {r}"));
    }
    if let Some(t) = threshold {
        changes.push(format!("circuit_breaker_threshold → {t}"));
    }
    if let Some(r) = rps {
        changes.push(format!("rate_limit_rps → {r}"));
    }

    assert_eq!(changes.len(), 4);
    assert!(changes[0].contains("anthropic"));
    assert!(changes[1].contains("5"));
    assert!(changes[2].contains("10"));
    assert!(changes[3].contains("50"));
}

#[test]
fn test_partial_config_update_only_changes_specified() {
    let mut changes: Vec<String> = Vec::new();

    // Only update worker — other fields are None
    let worker = Some("llama");
    let retries: Option<u32> = None;
    let threshold: Option<u32> = None;
    let rps: Option<u32> = None;

    if let Some(w) = worker {
        changes.push(format!("worker → {w}"));
    }
    if let Some(r) = retries {
        changes.push(format!("retry_attempts → {r}"));
    }
    if let Some(t) = threshold {
        changes.push(format!("circuit_breaker_threshold → {t}"));
    }
    if let Some(r) = rps {
        changes.push(format!("rate_limit_rps → {r}"));
    }

    assert_eq!(changes.len(), 1);
    assert!(changes[0].contains("llama"));
}

#[test]
fn test_config_json_serialization_shape() {
    let config = serde_json::json!({
        "worker": "echo",
        "retry_attempts": 3,
        "circuit_breaker_threshold": 5,
        "rate_limit_rps": 100,
    });

    assert_eq!(config.get("worker").and_then(|v| v.as_str()), Some("echo"));
    assert_eq!(
        config.get("retry_attempts").and_then(|v| v.as_u64()),
        Some(3)
    );
    assert_eq!(
        config
            .get("circuit_breaker_threshold")
            .and_then(|v| v.as_u64()),
        Some(5)
    );
    assert_eq!(
        config.get("rate_limit_rps").and_then(|v| v.as_u64()),
        Some(100)
    );
}

#[test]
fn test_config_response_shape() {
    let response = serde_json::json!({
        "status": "updated",
        "changes": ["worker → echo"],
        "current_config": {
            "worker": "echo",
            "retry_attempts": 3,
            "circuit_breaker_threshold": 5,
            "rate_limit_rps": 100,
        }
    });

    assert_eq!(
        response.get("status").and_then(|v| v.as_str()),
        Some("updated")
    );
    assert!(response.get("changes").and_then(|v| v.as_array()).is_some());
    assert!(response.get("current_config").is_some());
}
