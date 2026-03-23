//! Dead-Letter Queue Replay Binary
//!
//! Reads failed [`PromptRequest`] records from a dead-letter queue dump (NDJSON
//! format) and replays them through a running orchestrator's HTTP API.
//!
//! ## Usage
//!
//! ```text
//! # Replay a DLQ file (max 3 retries per request):
//! replay --queue-file dlq.ndjson --orchestrator-url http://localhost:8080
//!
//! # Read from stdin, increase retry budget:
//! cat dlq.ndjson | replay --max-retries 5
//!
//! # Dry-run: parse and validate without submitting:
//! replay --queue-file dlq.ndjson --dry-run
//! ```
//!
//! ## NDJSON Format
//!
//! Each line is a JSON object. Fields match the HTTP `POST /api/v1/infer` body:
//!
//! ```json
//! {"prompt":"Hello pipeline","session_id":"s1","metadata":{},"deadline_secs":null}
//! ```
//!
//! The replay tool also accepts the raw [`DroppedRequest`] format exported from
//! the `/api/v1/debug/dlq` endpoint:
//!
//! ```json
//! {"request_id":"req-1","session_id":"s1","reason":"backpressure","dropped_at":1711900000}
//! ```
//!
//! When a raw `DroppedRequest` is detected (no `prompt` field), the tool
//! reconstructs a minimal `PromptRequest` with `input` set to `"<replay>"` and
//! the original `request_id` preserved in metadata.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;

// ============================================================================
// CLI Arguments
// ============================================================================

/// Replay dead-letter queue entries into a running tokio-prompt-orchestrator.
#[derive(Debug, Parser)]
#[command(name = "replay", version, about, long_about = None)]
struct Args {
    /// Path to the NDJSON queue file.
    ///
    /// When omitted (or set to `-`), entries are read from stdin.
    #[arg(long, short = 'f', value_name = "FILE")]
    queue_file: Option<PathBuf>,

    /// Maximum number of retry attempts for each failed request.
    ///
    /// Requests that fail more than this many times are skipped and counted
    /// in the final error summary.  Default: 3.
    #[arg(long, default_value_t = 3, value_name = "N")]
    max_retries: u32,

    /// Base URL of the running orchestrator, e.g. `http://localhost:8080`.
    #[arg(
        long,
        default_value = "http://127.0.0.1:8080",
        value_name = "URL",
        env = "ORCHESTRATOR_URL"
    )]
    orchestrator_url: String,

    /// Optional API key sent as `Authorization: Bearer <key>`.
    #[arg(long, value_name = "KEY", env = "API_KEY")]
    api_key: Option<String>,

    /// Parse and validate the NDJSON without submitting any requests.
    #[arg(long, default_value_t = false)]
    dry_run: bool,

    /// Delay between submissions (milliseconds) to avoid overwhelming the pipeline.
    #[arg(long, default_value_t = 50, value_name = "MS")]
    delay_ms: u64,

    /// Timeout for each individual HTTP request (seconds).
    #[arg(long, default_value_t = 30, value_name = "SECS")]
    request_timeout_secs: u64,
}

// ============================================================================
// Serialisation types
// ============================================================================

/// A single infer request — matches the `POST /api/v1/infer` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReplayEntry {
    /// The prompt to submit.
    #[serde(default)]
    prompt: String,
    /// Optional session identifier.
    #[serde(default)]
    session_id: Option<String>,
    /// Arbitrary metadata forwarded through the pipeline.
    #[serde(default)]
    metadata: HashMap<String, String>,
    /// Optional deadline in seconds from now.
    #[serde(default)]
    deadline_secs: Option<u64>,
}

/// Raw `DroppedRequest` shape from the `/api/v1/debug/dlq` endpoint.
///
/// Detected when the `prompt` field is absent.
#[derive(Debug, Deserialize)]
struct DroppedRequestEntry {
    request_id: String,
    session_id: String,
    reason: String,
}

/// Response body from `POST /api/v1/infer`.
#[derive(Debug, Deserialize)]
struct InferResponse {
    request_id: String,
    status: String,
    #[serde(default)]
    error: Option<String>,
}

// ============================================================================
// Progress display
// ============================================================================

/// Simple terminal progress bar that prints to stderr.
///
/// Uses only the standard library — no external TUI dependency needed for this
/// lightweight binary.  For richer display, the `tui` feature binary provides
/// a full Ratatui dashboard.
struct ProgressBar {
    total: usize,
    current: usize,
    succeeded: usize,
    failed: usize,
    skipped: usize,
}

impl ProgressBar {
    fn new(total: usize) -> Self {
        Self {
            total,
            current: 0,
            succeeded: 0,
            failed: 0,
            skipped: 0,
        }
    }

    /// Advance by one item and print the updated bar.
    fn advance(&mut self, succeeded: bool, skipped: bool) {
        self.current += 1;
        if skipped {
            self.skipped += 1;
        } else if succeeded {
            self.succeeded += 1;
        } else {
            self.failed += 1;
        }
        self.render();
    }

    fn render(&self) {
        let bar_width: usize = 40;
        let filled = if self.total > 0 {
            (self.current * bar_width) / self.total
        } else {
            0
        };
        let empty = bar_width.saturating_sub(filled);
        let bar: String = format!(
            "[{}{}] {}/{} ok:{} fail:{} skip:{}",
            "#".repeat(filled),
            "-".repeat(empty),
            self.current,
            self.total,
            self.succeeded,
            self.failed,
            self.skipped,
        );
        // Use \r to overwrite the line on terminals; newline on the last item.
        if self.current >= self.total {
            eprintln!("\r{bar}");
        } else {
            eprint!("\r{bar}");
        }
    }

    fn print_summary(&self) {
        eprintln!(
            "\nReplay complete: {} submitted, {} succeeded, {} failed, {} skipped.",
            self.current, self.succeeded, self.failed, self.skipped
        );
    }
}

// ============================================================================
// Core replay logic
// ============================================================================

/// Attempt to submit `entry` to the orchestrator with up to `max_retries`
/// retry attempts using exponential back-off.
///
/// Returns `true` on success, `false` if all attempts are exhausted.
async fn submit_with_retry(
    client: &Client,
    endpoint: &str,
    entry: &ReplayEntry,
    api_key: Option<&str>,
    max_retries: u32,
) -> bool {
    let mut attempt = 0u32;
    loop {
        let mut req = client.post(endpoint).json(entry);
        if let Some(key) = api_key {
            req = req.bearer_auth(key);
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                // Optionally parse the response to log the request_id.
                if let Ok(body) = resp.json::<InferResponse>().await {
                    eprintln!(
                        "  -> accepted: request_id={} status={}",
                        body.request_id, body.status
                    );
                }
                return true;
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp
                    .text()
                    .await
                    .unwrap_or_else(|_| "<unreadable>".to_string());
                eprintln!(
                    "  attempt {}/{}: HTTP {} — {}",
                    attempt + 1,
                    max_retries + 1,
                    status,
                    body.chars().take(120).collect::<String>()
                );
            }
            Err(e) => {
                eprintln!(
                    "  attempt {}/{}: network error — {}",
                    attempt + 1,
                    max_retries + 1,
                    e
                );
            }
        }

        attempt += 1;
        if attempt > max_retries {
            return false;
        }
        // Exponential back-off: 100ms, 200ms, 400ms, …
        let delay = Duration::from_millis(100u64.saturating_mul(1u64 << attempt.min(10)));
        tokio::time::sleep(delay).await;
    }
}

/// Parse a single NDJSON line into a [`ReplayEntry`].
///
/// Handles both the full `InferRequest` shape (has `prompt`) and the raw
/// `DroppedRequest` shape (has `request_id` + `reason`).
fn parse_line(line: &str) -> Option<ReplayEntry> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None; // skip blank lines and comments
    }

    // Try full InferRequest shape first.
    if let Ok(entry) = serde_json::from_str::<ReplayEntry>(line) {
        if !entry.prompt.is_empty() {
            return Some(entry);
        }
    }

    // Fallback: parse as DroppedRequest and reconstruct a minimal entry.
    if let Ok(dropped) = serde_json::from_str::<DroppedRequestEntry>(line) {
        let mut metadata = HashMap::new();
        metadata.insert("original_request_id".to_string(), dropped.request_id);
        metadata.insert("dlq_reason".to_string(), dropped.reason);
        return Some(ReplayEntry {
            prompt: "<replay>".to_string(),
            session_id: Some(dropped.session_id),
            metadata,
            deadline_secs: None,
        });
    }

    eprintln!("  [warn] could not parse line, skipping: {}", &line[..line.len().min(80)]);
    None
}

// ============================================================================
// Entry point
// ============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialise a simple env-filter tracing subscriber for the replay binary.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    let args = Args::parse();

    let infer_endpoint = format!("{}/api/v1/infer", args.orchestrator_url.trim_end_matches('/'));

    eprintln!("tokio-prompt-orchestrator DLQ Replay");
    eprintln!("  endpoint   : {infer_endpoint}");
    eprintln!("  max-retries: {}", args.max_retries);
    eprintln!("  dry-run    : {}", args.dry_run);
    eprintln!("  delay-ms   : {}", args.delay_ms);

    // ---- Read all lines from file or stdin ----
    let lines: Vec<String> = match &args.queue_file {
        Some(path) if path.to_str() != Some("-") => {
            eprintln!("  source     : {}", path.display());
            let file = tokio::fs::File::open(path).await.map_err(|e| {
                format!("cannot open queue file '{}': {e}", path.display())
            })?;
            let mut reader = BufReader::new(file).lines();
            let mut v = Vec::new();
            while let Some(line) = reader.next_line().await? {
                v.push(line);
            }
            v
        }
        _ => {
            eprintln!("  source     : stdin");
            let stdin = tokio::io::stdin();
            let mut reader = BufReader::new(stdin).lines();
            let mut v = Vec::new();
            while let Some(line) = reader.next_line().await? {
                v.push(line);
            }
            v
        }
    };

    // ---- Parse entries ----
    let entries: Vec<ReplayEntry> = lines.iter().filter_map(|l| parse_line(l)).collect();
    eprintln!("  entries    : {} parsed from {} lines", entries.len(), lines.len());

    if entries.is_empty() {
        eprintln!("No valid entries to replay. Exiting.");
        return Ok(());
    }

    if args.dry_run {
        eprintln!("\nDry-run mode — no requests submitted.");
        for (i, entry) in entries.iter().enumerate() {
            eprintln!(
                "  [{:>4}] session={} prompt={:?}",
                i + 1,
                entry.session_id.as_deref().unwrap_or("-"),
                &entry.prompt.chars().take(60).collect::<String>()
            );
        }
        return Ok(());
    }

    // ---- Build HTTP client ----
    let client = Client::builder()
        .timeout(Duration::from_secs(args.request_timeout_secs))
        .build()?;

    let api_key = args.api_key.as_deref();
    let delay = Duration::from_millis(args.delay_ms);

    // ---- Replay loop ----
    let mut bar = ProgressBar::new(entries.len());

    for entry in &entries {
        let succeeded = submit_with_retry(
            &client,
            &infer_endpoint,
            entry,
            api_key,
            args.max_retries,
        )
        .await;

        bar.advance(succeeded, false);

        if delay.as_millis() > 0 {
            tokio::time::sleep(delay).await;
        }
    }

    bar.print_summary();

    // Exit with a non-zero code when any requests failed.
    if bar.failed > 0 {
        std::process::exit(1);
    }

    Ok(())
}
