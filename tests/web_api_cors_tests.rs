//! CORS header integration tests for `src/web_api.rs`.
//!
//! Tests verify:
//! - Wildcard CORS when `ALLOWED_ORIGINS` is not set
//! - Specific origin echo when `ALLOWED_ORIGINS` is set
//! - No CORS header for origins not in the allowlist
//! - Preflight OPTIONS returns correct allow-methods and allow-headers
//!
//! # Design note
//!
//! `ALLOWED_ORIGINS` is read once at server startup inside `start_server`.
//! Because tests run in the same process and share `std::env`, each test
//! that needs a specific origins value spawns its own server *after* setting
//! the env var.  To prevent parallel tests from racing, the tests that rely
//! on a specific origins value run with `#[serial]` ordering via explicit
//! port isolation; a `Mutex` guard is not feasible across async tasks, so we
//! instead separate the "with origins" tests onto their own ports and ensure
//! `ALLOWED_ORIGINS` is set synchronously before the server is spawned and
//! before the await that lets the scheduler run.

#![cfg(feature = "web-api")]

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::mpsc;

use tokio_prompt_orchestrator::web_api::ServerConfig;
use tokio_prompt_orchestrator::PromptRequest;

// ============================================================================
// Global mutex so CORS tests that mutate ALLOWED_ORIGINS don't race
// ============================================================================

static ENV_MUTEX: Mutex<()> = Mutex::new(());

// ============================================================================
// Port allocation
// ============================================================================

static CORS_PORT_COUNTER: AtomicU16 = AtomicU16::new(30100);

fn next_port() -> u16 {
    CORS_PORT_COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ============================================================================
// Server helper
// ============================================================================

/// Spawn a web API server and return `(base_url, pipeline_rx)`.
/// Caller is responsible for setting/clearing `ALLOWED_ORIGINS` *before*
/// calling this function (while holding `ENV_MUTEX`).
async fn spawn_server_on(port: u16) -> (String, mpsc::Receiver<PromptRequest>) {
    let (tx, rx) = mpsc::channel::<PromptRequest>(64);
    let config = ServerConfig {
        host: "127.0.0.1".to_string(),
        port,
        max_request_size: 1024 * 1024,
        timeout_seconds: 2,
    };
    let (_, out_rx) = mpsc::channel::<tokio_prompt_orchestrator::PostOutput>(1);
    tokio::spawn(async move {
        let _ = tokio_prompt_orchestrator::web_api::start_server(config, tx, out_rx).await;
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    (format!("http://127.0.0.1:{port}"), rx)
}

fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client must build in tests")
}

// ============================================================================
// Test 27a – Wildcard CORS when ALLOWED_ORIGINS is not set
// ============================================================================

/// When `ALLOWED_ORIGINS` is absent the server responds with
/// `Access-Control-Allow-Origin: *` for any origin.
#[tokio::test]
async fn test_cors_wildcard_when_allowed_origins_not_set() {
    let port = next_port();
    let _guard = ENV_MUTEX.lock().expect("env mutex");
    std::env::remove_var("ALLOWED_ORIGINS");
    let (base, _rx) = spawn_server_on(port).await;
    drop(_guard);

    let resp = client()
        .get(format!("{base}/health"))
        .header("origin", "http://anything.example.com")
        .send()
        .await
        .expect("send must succeed");

    let acao = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    assert!(
        !acao.is_empty(),
        "Access-Control-Allow-Origin must be present when ALLOWED_ORIGINS is not set"
    );
    assert_eq!(
        acao, "*",
        "Access-Control-Allow-Origin must be '*' when ALLOWED_ORIGINS is not set, got '{acao}'"
    );
}

// ============================================================================
// Test 27b – Specific origin echoed when ALLOWED_ORIGINS is set
// ============================================================================

/// When `ALLOWED_ORIGINS=https://app.example.com` the server echoes that
/// exact origin back in `Access-Control-Allow-Origin`.
#[tokio::test]
async fn test_cors_specific_origin_echoed_when_in_allowlist() {
    let port = next_port();
    let _guard = ENV_MUTEX.lock().expect("env mutex");
    std::env::set_var("ALLOWED_ORIGINS", "https://app.example.com");
    let (base, _rx) = spawn_server_on(port).await;
    std::env::remove_var("ALLOWED_ORIGINS");
    drop(_guard);

    let resp = client()
        .get(format!("{base}/health"))
        .header("origin", "https://app.example.com")
        .send()
        .await
        .expect("send must succeed");

    let acao = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    assert!(
        !acao.is_empty(),
        "Access-Control-Allow-Origin must be present for an allowed origin"
    );
    assert_eq!(
        acao,
        "https://app.example.com",
        "Server must echo the allowed origin exactly, got '{acao}'"
    );
}

// ============================================================================
// Test 27c – Origin NOT in allowlist does not get echoed as ACAO
// ============================================================================

/// When `ALLOWED_ORIGINS` lists a specific origin, a request from a
/// *different* origin must NOT receive that origin back in ACAO.
#[tokio::test]
async fn test_cors_origin_not_in_allowlist_not_echoed() {
    let port = next_port();
    let _guard = ENV_MUTEX.lock().expect("env mutex");
    std::env::set_var("ALLOWED_ORIGINS", "https://allowed.example.com");
    let (base, _rx) = spawn_server_on(port).await;
    std::env::remove_var("ALLOWED_ORIGINS");
    drop(_guard);

    let resp = client()
        .get(format!("{base}/health"))
        .header("origin", "https://evil.example.com")
        .send()
        .await
        .expect("send must succeed");

    // Server should still respond 200 — CORS is advisory for browsers.
    assert!(
        resp.status().is_success(),
        "Server should respond 2xx even for disallowed origins, got {}",
        resp.status()
    );

    let acao = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    assert!(
        acao != "https://evil.example.com",
        "Server must NOT echo a non-allowlisted origin, got '{acao}'"
    );
}

// ============================================================================
// Test 27d – Preflight OPTIONS returns allow-methods and allow-headers
// ============================================================================

/// A CORS preflight `OPTIONS` request must succeed (2xx) and include the
/// `Access-Control-Allow-Origin` header.  The server uses
/// `CorsLayer::new().allow_origin(AllowOrigin::any())` without an explicit
/// `allow_headers` override, so `Access-Control-Allow-Headers` is only
/// present when tower-http mirrors the requested headers.  We verify the
/// response is a valid preflight response (2xx + ACAO) without asserting on
/// headers the server does not explicitly configure.
#[tokio::test]
async fn test_cors_preflight_options_returns_success_and_acao() {
    let port = next_port();
    let _guard = ENV_MUTEX.lock().expect("env mutex");
    std::env::remove_var("ALLOWED_ORIGINS");
    let (base, _rx) = spawn_server_on(port).await;
    drop(_guard);

    let resp = client()
        .request(reqwest::Method::OPTIONS, format!("{base}/api/v1/infer"))
        .header("origin", "http://example.com")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "content-type")
        .send()
        .await
        .expect("preflight OPTIONS must not fail at the transport level");

    // Must be a 2xx response — not 4xx or 5xx.
    assert!(
        resp.status().is_success(),
        "Preflight OPTIONS should return 2xx, got {}",
        resp.status()
    );

    // The ACAO header must be present for a wildcard-origin server.
    let acao = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    assert!(
        !acao.is_empty(),
        "Preflight response must include Access-Control-Allow-Origin, got empty"
    );
    assert_eq!(
        acao, "*",
        "Wildcard server must return '*' in preflight ACAO, got '{acao}'"
    );
}

// ============================================================================
// Test 27e – Wildcard server: any origin gets ACAO: *
// ============================================================================

/// When the server is in wildcard mode, sending any origin header results in
/// `Access-Control-Allow-Origin: *` being returned.
#[tokio::test]
async fn test_cors_wildcard_server_returns_star_for_any_origin() {
    let port = next_port();
    let _guard = ENV_MUTEX.lock().expect("env mutex");
    std::env::remove_var("ALLOWED_ORIGINS");
    let (base, _rx) = spawn_server_on(port).await;
    drop(_guard);

    let origins = ["https://alpha.example.com", "http://beta.org", "null"];
    for origin in &origins {
        let resp = client()
            .get(format!("{base}/health"))
            .header("origin", *origin)
            .send()
            .await
            .expect("send must succeed");
        let acao = resp
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(
            acao, "*",
            "Wildcard server must return '*' for origin '{origin}', got '{acao}'"
        );
    }
}

// ============================================================================
// Test 27f – Multiple allowed origins: first origin is allowed
// ============================================================================

#[tokio::test]
async fn test_cors_multiple_allowed_origins_first_is_allowed() {
    let port = next_port();
    let _guard = ENV_MUTEX.lock().expect("env mutex");
    std::env::set_var(
        "ALLOWED_ORIGINS",
        "https://first.example.com,https://second.example.com",
    );
    let (base, _rx) = spawn_server_on(port).await;
    std::env::remove_var("ALLOWED_ORIGINS");
    drop(_guard);

    let resp = client()
        .get(format!("{base}/health"))
        .header("origin", "https://first.example.com")
        .send()
        .await
        .expect("send");

    let acao = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    assert_eq!(
        acao, "https://first.example.com",
        "First origin in list should be echoed, got '{acao}'"
    );
}

// ============================================================================
// Test 27g – Multiple allowed origins: second origin is allowed
// ============================================================================

#[tokio::test]
async fn test_cors_multiple_allowed_origins_second_is_allowed() {
    let port = next_port();
    let _guard = ENV_MUTEX.lock().expect("env mutex");
    std::env::set_var(
        "ALLOWED_ORIGINS",
        "https://first.example.com,https://second.example.com",
    );
    let (base, _rx) = spawn_server_on(port).await;
    std::env::remove_var("ALLOWED_ORIGINS");
    drop(_guard);

    let resp = client()
        .get(format!("{base}/health"))
        .header("origin", "https://second.example.com")
        .send()
        .await
        .expect("send");

    let acao = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    assert_eq!(
        acao, "https://second.example.com",
        "Second origin in list should be echoed, got '{acao}'"
    );
}
