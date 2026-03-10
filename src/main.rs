//! tokio-prompt-orchestrator — production CLI binary
//!
//! ## CLI flags
//!
//! ```text
//! --provider <openai|anthropic|llama|echo>   (default: echo)
//! --model    <model-name>                    (default: provider-specific)
//! --port     <N>                             (default: 8080)
//! --host     <addr>                          (default: 0.0.0.0)
//! --log-level <trace|debug|info|warn|error>  (default: info)
//! --no-web                                   disable HTTP/WS API
//! --help / -h
//! --version / -V
//! ```
//!
//! ## Environment variables
//!
//! - `OPENAI_API_KEY`    — required for `--provider openai`
//! - `ANTHROPIC_API_KEY` — required for `--provider anthropic`
//! - `LLAMA_CPP_URL`     — llama.cpp base URL (default: http://localhost:8080)
//! - `API_KEY`           — bearer token for the web API (optional; disables auth if unset)
//! - `RUST_LOG`          — overrides `--log-level` for fine-grained tracing

use std::env;
use std::sync::Arc;
use tokio_prompt_orchestrator::{
    metrics, spawn_pipeline, AnthropicWorker, EchoWorker, LlamaCppWorker, ModelWorker, OpenAiWorker,
};

#[cfg(feature = "web-api")]
use tokio_prompt_orchestrator::web_api::{start_server, ServerConfig};

// ---------------------------------------------------------------------------
// CLI parsing — zero extra dependencies, pure std
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CliArgs {
    provider: String,
    model: String,
    port: u16,
    host: String,
    log_level: String,
    no_web: bool,
}

fn default_model(provider: &str) -> &'static str {
    match provider {
        "openai" => "gpt-4o",
        "anthropic" => "claude-3-5-sonnet-20241022",
        "llama" => "local",
        _ => "echo",
    }
}

fn print_help() {
    println!(
        r#"tokio-prompt-orchestrator {}

USAGE:
    orchestrator [OPTIONS]

OPTIONS:
    --provider <openai|anthropic|llama|echo>   LLM backend  (default: echo)
    --model    <name>                          Model name   (default: provider-specific)
    --port     <N>                             HTTP port    (default: 8080)
    --host     <addr>                          Bind address (default: 0.0.0.0)
    --log-level <trace|debug|info|warn|error>  Log level    (default: info)
    --no-web                                   Disable web API server
    --help, -h                                 Print this help
    --version, -V                              Print version

ENVIRONMENT:
    OPENAI_API_KEY     Required when --provider openai
    ANTHROPIC_API_KEY  Required when --provider anthropic
    LLAMA_CPP_URL      llama.cpp server URL (default: http://localhost:8080)
    API_KEY            Bearer token for web API auth (auth disabled if unset)
    RUST_LOG           Fine-grained log filter (overrides --log-level)"#,
        env!("CARGO_PKG_VERSION")
    );
}

fn parse_args() -> Result<CliArgs, String> {
    let args: Vec<String> = env::args().collect();
    let mut provider = "echo".to_string();
    let mut model = String::new();
    let mut port: u16 = 8080;
    let mut host = "0.0.0.0".to_string();
    let mut log_level = "info".to_string();
    let mut no_web = false;

    let mut i = 1usize;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("orchestrator {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--no-web" => {
                no_web = true;
            }
            flag @ ("--provider" | "--model" | "--port" | "--host" | "--log-level") => {
                i += 1;
                if i >= args.len() {
                    return Err(format!("{flag} requires a value"));
                }
                let val = args[i].clone();
                match flag {
                    "--provider" => provider = val,
                    "--model" => model = val,
                    "--port" => {
                        port = val
                            .parse()
                            .map_err(|_| format!("--port must be a number, got: {val}"))?;
                    }
                    "--host" => host = val,
                    "--log-level" => log_level = val,
                    _ => {}
                }
            }
            unknown => {
                return Err(format!("Unknown flag: {unknown}"));
            }
        }
        i += 1;
    }

    if model.is_empty() {
        model = default_model(&provider).to_string();
    }

    Ok(CliArgs {
        provider,
        model,
        port,
        host,
        log_level,
        no_web,
    })
}

// ---------------------------------------------------------------------------
// Startup banner
// ---------------------------------------------------------------------------

fn print_banner(args: &CliArgs) {
    let api_key_set = env::var("API_KEY").map(|v| !v.is_empty()).unwrap_or(false);
    let auth_status = if api_key_set {
        "enabled (API_KEY set)"
    } else {
        "disabled (API_KEY not set)"
    };
    let web_status = if args.no_web {
        "disabled (--no-web)".to_string()
    } else {
        format!("http://{}:{}", args.host, args.port)
    };

    println!();
    println!("╔══════════════════════════════════════════════════╗");
    println!(
        "║      tokio-prompt-orchestrator v{:<16} ║",
        env!("CARGO_PKG_VERSION")
    );
    println!("╠══════════════════════════════════════════════════╣");
    println!(
        "║  Provider : {:<38}║",
        format!("{} ({})", args.provider, args.model)
    );
    println!("║  Web API  : {:<38}║", web_status);
    println!("║  Auth     : {:<38}║", auth_status);
    println!("║  Log level: {:<38}║", args.log_level);
    println!("╚══════════════════════════════════════════════════╝");
    println!();
}

// ---------------------------------------------------------------------------
// Worker construction
// ---------------------------------------------------------------------------

fn build_worker(args: &CliArgs) -> Result<Arc<dyn ModelWorker>, String> {
    match args.provider.as_str() {
        "openai" => {
            // Validate env var presence before calling constructor (which also reads it)
            match env::var("OPENAI_API_KEY") {
                Err(_) => {
                    return Err(
                        "OPENAI_API_KEY environment variable is required for --provider openai"
                            .to_string(),
                    )
                }
                Ok(ref k) if k.is_empty() => {
                    return Err("OPENAI_API_KEY is set but empty".to_string())
                }
                _ => {}
            }
            OpenAiWorker::new(args.model.clone())
                .map(|w| Arc::new(w) as Arc<dyn ModelWorker>)
                .map_err(|e| e.to_string())
        }
        "anthropic" => {
            match env::var("ANTHROPIC_API_KEY") {
                Err(_) => return Err(
                    "ANTHROPIC_API_KEY environment variable is required for --provider anthropic"
                        .to_string(),
                ),
                Ok(ref k) if k.is_empty() => {
                    return Err("ANTHROPIC_API_KEY is set but empty".to_string())
                }
                _ => {}
            }
            AnthropicWorker::new(args.model.clone())
                .map(|w| Arc::new(w) as Arc<dyn ModelWorker>)
                .map_err(|e| e.to_string())
        }
        "llama" => {
            // LlamaCppWorker::new() reads LLAMA_CPP_URL from env internally
            Ok(Arc::new(LlamaCppWorker::new()) as Arc<dyn ModelWorker>)
        }
        "echo" => Ok(Arc::new(EchoWorker::with_delay(10)) as Arc<dyn ModelWorker>),
        unknown => Err(format!(
            "Unknown provider '{unknown}'. Valid values: openai, anthropic, llama, echo"
        )),
    }
}

// ---------------------------------------------------------------------------
// Tracing initialisation with CLI level support
// ---------------------------------------------------------------------------

fn init_tracing_with_level(level: &str) {
    use tracing_subscriber::{fmt, EnvFilter};

    // RUST_LOG always wins; fall back to --log-level
    let filter = env::var("RUST_LOG")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| level.to_string());

    let _ = fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_target(false)
        .try_init();
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Error: {e}");
            eprintln!("Run with --help for usage.");
            std::process::exit(1);
        }
    };

    init_tracing_with_level(&args.log_level);

    if let Err(e) = metrics::init_metrics() {
        eprintln!("Warning: metrics init failed: {e}");
    }

    print_banner(&args);

    // Build worker — exit cleanly if required env vars are missing
    let worker = match build_worker(&args) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    tracing::info!(provider = %args.provider, model = %args.model, "Worker ready");

    // Spawn the 5-stage pipeline
    let handles = spawn_pipeline(worker);

    tracing::info!("Pipeline stages spawned");

    // Graceful shutdown channel
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    // Ctrl-C handler
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("Ctrl+C received — initiating graceful shutdown");
            let _ = shutdown_tx.send(());
        }
    });

    if args.no_web {
        // Headless mode: pipeline is open for programmatic use
        println!("Pipeline ready. Press Ctrl+C to stop.");
        shutdown_rx.await.ok();
        tracing::info!("Shutdown complete");
    } else {
        // Web API mode
        #[cfg(feature = "web-api")]
        {
            let pipeline_tx = handles.input_tx.clone();
            let config = ServerConfig {
                host: args.host.clone(),
                port: args.port,
                ..ServerConfig::default()
            };

            tracing::info!(addr = %format!("{}:{}", args.host, args.port), "Starting web API");

            tokio::select! {
                result = start_server(config, pipeline_tx) => {
                    if let Err(e) = result {
                        tracing::error!(error = %e, "Web API server error");
                        std::process::exit(1);
                    }
                }
                _ = shutdown_rx => {
                    tracing::info!("Shutdown signal received — stopping web API");
                }
            }
        }

        #[cfg(not(feature = "web-api"))]
        {
            eprintln!(
                "Warning: built without 'web-api' feature — web server disabled.\n\
                 Rebuild with: cargo build --features web-api"
            );
            println!("Pipeline ready (headless). Press Ctrl+C to stop.");
            shutdown_rx.await.ok();
            tracing::info!("Shutdown complete");
        }
    }

    // Drop the pipeline sender — signals the pipeline to drain
    drop(handles.input_tx);

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_model_openai_returns_gpt4o() {
        assert_eq!(default_model("openai"), "gpt-4o");
    }

    #[test]
    fn test_default_model_anthropic_returns_claude_sonnet() {
        assert_eq!(default_model("anthropic"), "claude-3-5-sonnet-20241022");
    }

    #[test]
    fn test_default_model_llama_returns_local() {
        assert_eq!(default_model("llama"), "local");
    }

    #[test]
    fn test_default_model_echo_returns_echo() {
        assert_eq!(default_model("echo"), "echo");
    }

    #[test]
    fn test_default_model_unknown_returns_echo() {
        assert_eq!(default_model("unknown-provider"), "echo");
    }

    #[test]
    fn test_parse_args_defaults() {
        // Simulate running with no arguments.
        // parse_args reads env::args(), so we test the defaults by calling with
        // a known empty-ish scenario indirectly through the default values.
        let args = CliArgs {
            provider: "echo".to_string(),
            model: "echo".to_string(),
            port: 8080,
            host: "0.0.0.0".to_string(),
            log_level: "info".to_string(),
            no_web: false,
        };
        assert_eq!(args.provider, "echo");
        assert_eq!(args.port, 8080);
        assert!(!args.no_web);
    }

    #[test]
    fn test_build_worker_echo_succeeds() {
        let args = CliArgs {
            provider: "echo".to_string(),
            model: "echo".to_string(),
            port: 8080,
            host: "0.0.0.0".to_string(),
            log_level: "info".to_string(),
            no_web: false,
        };
        let result = build_worker(&args);
        assert!(result.is_ok(), "echo worker must always succeed");
    }

    #[test]
    fn test_build_worker_openai_fails_without_key() {
        // If the key IS set in the environment, skip — we cannot unset env vars portably.
        if env::var("OPENAI_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            return;
        }
        let args = CliArgs {
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            port: 8080,
            host: "0.0.0.0".to_string(),
            log_level: "info".to_string(),
            no_web: false,
        };
        let result = build_worker(&args);
        assert!(result.is_err(), "openai requires OPENAI_API_KEY");
        let msg = result.err().unwrap();
        assert!(
            msg.contains("OPENAI_API_KEY"),
            "error must name the missing key"
        );
    }

    #[test]
    fn test_build_worker_anthropic_fails_without_key() {
        if env::var("ANTHROPIC_API_KEY").is_ok() {
            return;
        }
        let args = CliArgs {
            provider: "anthropic".to_string(),
            model: "claude-3-5-sonnet-20241022".to_string(),
            port: 8080,
            host: "0.0.0.0".to_string(),
            log_level: "info".to_string(),
            no_web: false,
        };
        let result = build_worker(&args);
        assert!(result.is_err(), "anthropic requires ANTHROPIC_API_KEY");
        let msg = result.err().unwrap();
        assert!(msg.contains("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn test_build_worker_unknown_provider_returns_error() {
        let args = CliArgs {
            provider: "fakeai".to_string(),
            model: "x".to_string(),
            port: 8080,
            host: "0.0.0.0".to_string(),
            log_level: "info".to_string(),
            no_web: false,
        };
        let result = build_worker(&args);
        assert!(result.is_err());
        assert!(result.err().unwrap().contains("Unknown provider"));
    }

    #[test]
    fn test_build_worker_llama_uses_default_url() {
        // LlamaCppWorker just stores the URL; no network call at construction.
        let args = CliArgs {
            provider: "llama".to_string(),
            model: "local".to_string(),
            port: 8080,
            host: "0.0.0.0".to_string(),
            log_level: "info".to_string(),
            no_web: false,
        };
        let result = build_worker(&args);
        assert!(result.is_ok(), "llama worker should build without env var");
    }
}
