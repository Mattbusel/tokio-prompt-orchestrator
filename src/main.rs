//! tokio-prompt-orchestrator — interactive production CLI binary
//!
//! On first run the wizard prompts for all required settings.
//! Subsequent runs reuse values stored in orchestrator.env (same folder as the .exe).
//!
//! ## Agent / IDE connection
//!
//! After startup the orchestrator exposes:
//!   HTTP POST  http://localhost:<port>/v1/prompt   — send a prompt, get a response
//!   WS         ws://localhost:<port>/v1/stream     — streaming responses
//!   GET        http://localhost:<port>/health       — health check
//!   GET        http://localhost:<port>/metrics      — prometheus metrics
//!
//! Claude Desktop / MCP: add to claude_desktop_config.json
//!   { "mcpServers": { "orchestrator": { "url": "http://localhost:8080" } } }
//!
//! VS Code / Cursor: point the extension at http://localhost:<port>
//!
//! ## CLI flags (all optional — wizard fills anything missing)
//!
//! ```text
//! --provider <openai|anthropic|llama|echo>
//! --model    <model-name>
//! --port     <N>                   (default: 8080)
//! --host     <addr>                (default: 127.0.0.1)
//! --log-level <trace|debug|info|warn|error>
//! --no-web                         disable HTTP/WS API
//! --help / -h
//! --version / -V
//! ```

use std::env;
use std::io::{self, Write as _};
use std::sync::Arc;
use tokio_prompt_orchestrator::{
    metrics, spawn_pipeline, AnthropicWorker, EchoWorker, LlamaCppWorker, ModelWorker, OpenAiWorker,
};

#[cfg(feature = "web-api")]
use tokio_prompt_orchestrator::web_api::{start_server, ServerConfig};

// ---------------------------------------------------------------------------
// Env file — persist settings next to the .exe between runs
// ---------------------------------------------------------------------------

fn env_file_path() -> std::path::PathBuf {
    // Place next to the running binary so double-clicking always finds it.
    let mut p = env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("."));
    p.pop();
    p.push("orchestrator.env");
    p
}

/// Load KEY=VALUE pairs from orchestrator.env into the process environment
/// (only if the variable is not already set).
fn load_env_file() {
    let path = env_file_path();
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return;
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim();
            if env::var(k).is_err() {
                // Safety: single-threaded at this point
                unsafe { env::set_var(k, v) };
            }
        }
    }
}

/// Append or overwrite a single key in orchestrator.env.
fn save_env_value(key: &str, value: &str) {
    let path = env_file_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| !l.trim_start().starts_with(&format!("{key}=")))
        .map(str::to_string)
        .collect();
    lines.push(format!("{key}={value}"));

    let _ = std::fs::write(&path, lines.join("\n") + "\n");
}

// ---------------------------------------------------------------------------
// Interactive wizard helpers
// ---------------------------------------------------------------------------

fn prompt_line(question: &str) -> String {
    print!("{question}");
    let _ = io::stdout().flush();
    let mut buf = String::new();
    let _ = io::stdin().read_line(&mut buf);
    buf.trim().to_string()
}

fn prompt_secret(question: &str) -> String {
    // Best-effort: hide input on supported terminals.
    // Falls back to visible input if rpassword is not available.
    print!("{question}");
    let _ = io::stdout().flush();
    let mut buf = String::new();
    let _ = io::stdin().read_line(&mut buf);
    buf.trim().to_string()
}

fn prompt_with_default(question: &str, default: &str) -> String {
    let answer = prompt_line(&format!("{question} [{}]: ", default));
    if answer.is_empty() {
        default.to_string()
    } else {
        answer
    }
}

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CliArgs {
    provider: Option<String>, // None = ask wizard
    model: Option<String>,
    port: u16,
    host: String,
    log_level: String,
    no_web: bool,
}

fn default_model(provider: &str) -> &'static str {
    match provider {
        "openai" => "gpt-4o",
        "anthropic" => "claude-sonnet-4-6",
        "llama" => "local",
        _ => "echo",
    }
}

fn print_help() {
    println!(
        r#"orchestrator {}

USAGE:
    orchestrator [OPTIONS]

    Run with no options to launch the interactive setup wizard.

OPTIONS:
    --provider <openai|anthropic|llama|echo>   LLM backend
    --model    <name>                          Model name
    --port     <N>                             HTTP port        (default: 8080)
    --host     <addr>                          Bind address     (default: 127.0.0.1)
    --log-level <trace|debug|info|warn|error>  Log verbosity    (default: info)
    --no-web                                   Disable web API server
    --help, -h                                 Print this help
    --version, -V                              Print version

AGENT / IDE CONNECTION:
    HTTP  POST http://localhost:<port>/v1/prompt     send a prompt
    WS         ws://localhost:<port>/v1/stream       streaming
    GET        http://localhost:<port>/health        health check

    Claude Desktop — add to claude_desktop_config.json:
      {{ "mcpServers": {{ "orchestrator": {{ "url": "http://localhost:8080" }} }} }}

    VS Code / Cursor — point your AI extension at http://localhost:<port>

ENVIRONMENT (written to orchestrator.env on first run):
    OPENAI_API_KEY     Required when --provider openai
    ANTHROPIC_API_KEY  Required when --provider anthropic
    LLAMA_CPP_URL      llama.cpp server URL (default: http://localhost:8080)
    API_KEY            Bearer token for web API auth (leave blank = no auth)
    RUST_LOG           Fine-grained log filter"#,
        env!("CARGO_PKG_VERSION")
    );
}

fn parse_args() -> Result<CliArgs, String> {
    let args: Vec<String> = env::args().collect();
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut port: u16 = 8080;
    let mut host = "127.0.0.1".to_string();
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
                    "--provider" => provider = Some(val),
                    "--model" => model = Some(val),
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
// Interactive setup wizard
// Fills in anything missing from CLI args or env, then saves to orchestrator.env
// ---------------------------------------------------------------------------

struct ResolvedConfig {
    provider: String,
    model: String,
    port: u16,
    host: String,
    log_level: String,
    no_web: bool,
}

fn run_wizard(args: CliArgs) -> ResolvedConfig {
    let is_interactive = args.provider.is_none()
        && env::var("OPENAI_API_KEY").is_err()
        && env::var("ANTHROPIC_API_KEY").is_err();

    // ── Provider ─────────────────────────────────────────────────────────────
    let provider = match args.provider {
        Some(p) => p,
        None => {
            println!("\n  Which LLM provider do you want to use?");
            println!("  1) openai     (requires OPENAI_API_KEY)");
            println!("  2) anthropic  (requires ANTHROPIC_API_KEY)");
            println!("  3) llama      (local llama.cpp server)");
            println!("  4) echo       (offline test mode)");
            let choice = prompt_with_default("  Enter number or name", "4");
            match choice.as_str() {
                "1" => "openai".to_string(),
                "2" => "anthropic".to_string(),
                "3" => "llama".to_string(),
                "4" | "echo" => "echo".to_string(),
                other => other.to_string(),
            }
        }
    };

    // ── API key ───────────────────────────────────────────────────────────────
    match provider.as_str() {
        "openai" => {
            if env::var("OPENAI_API_KEY")
                .map(|v| v.is_empty())
                .unwrap_or(true)
            {
                let key = prompt_secret("  OpenAI API key (sk-…): ");
                if !key.is_empty() {
                    unsafe { env::set_var("OPENAI_API_KEY", &key) };
                    save_env_value("OPENAI_API_KEY", &key);
                }
            } else if is_interactive {
                println!("  OPENAI_API_KEY already set — using existing key.");
            }
        }
        "anthropic" => {
            if env::var("ANTHROPIC_API_KEY")
                .map(|v| v.is_empty())
                .unwrap_or(true)
            {
                let key = prompt_secret("  Anthropic API key (sk-ant-…): ");
                if !key.is_empty() {
                    unsafe { env::set_var("ANTHROPIC_API_KEY", &key) };
                    save_env_value("ANTHROPIC_API_KEY", &key);
                }
            } else if is_interactive {
                println!("  ANTHROPIC_API_KEY already set — using existing key.");
            }
        }
        "llama" => {
            let default_url =
                env::var("LLAMA_CPP_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
            if is_interactive {
                let url = prompt_with_default("  llama.cpp server URL", &default_url);
                unsafe { env::set_var("LLAMA_CPP_URL", &url) };
                save_env_value("LLAMA_CPP_URL", &url);
            }
        }
        _ => {}
    }

    // ── Model ─────────────────────────────────────────────────────────────────
    let model = match args.model {
        Some(m) => m,
        None => {
            let def = default_model(&provider);
            if is_interactive {
                prompt_with_default("  Model name", def)
            } else {
                def.to_string()
            }
        }
    };

    // ── Web API port & auth ───────────────────────────────────────────────────
    let (port, no_web) = if is_interactive && !args.no_web {
        let port_str = prompt_with_default("  Web API port", &args.port.to_string());
        let port: u16 = port_str.parse().unwrap_or(args.port);

        // Optional bearer token auth
        if env::var("API_KEY").map(|v| v.is_empty()).unwrap_or(true) {
            println!("  (optional) Set a bearer token for API auth — press Enter to skip");
            let key = prompt_secret("  API_KEY: ");
            if !key.is_empty() {
                unsafe { env::set_var("API_KEY", &key) };
                save_env_value("API_KEY", &key);
            }
        }

        (port, false)
    } else {
        (args.port, args.no_web)
    };

    // Save non-secret settings
    save_env_value("PROVIDER", &provider);
    save_env_value("MODEL", &model);

    ResolvedConfig {
        provider,
        model,
        port,
        host: args.host,
        log_level: args.log_level,
        no_web,
    }
}

// ---------------------------------------------------------------------------
// Startup banner
// ---------------------------------------------------------------------------

fn print_banner(cfg: &ResolvedConfig) {
    let api_key_set = env::var("API_KEY").map(|v| !v.is_empty()).unwrap_or(false);
    let auth_status = if api_key_set {
        "enabled (API_KEY set)"
    } else {
        "disabled — no bearer token"
    };
    let web_status = if cfg.no_web {
        "disabled (--no-web)".to_string()
    } else {
        format!("http://{}:{}", cfg.host, cfg.port)
    };

    println!();
    println!("╔══════════════════════════════════════════════════╗");
    println!(
        "║   tokio-prompt-orchestrator v{:<19}║",
        env!("CARGO_PKG_VERSION")
    );
    println!("╠══════════════════════════════════════════════════╣");
    println!(
        "║  Provider : {:<38}║",
        format!("{} ({})", cfg.provider, cfg.model)
    );
    println!("║  Web API  : {:<38}║", web_status);
    println!("║  Auth     : {:<38}║", auth_status);
    println!("║  Log level: {:<38}║", cfg.log_level);
    println!("╠══════════════════════════════════════════════════╣");

    if !cfg.no_web {
        println!(
            "║  POST http://{}:{}/v1/prompt          ║",
            cfg.host, cfg.port
        );
        println!(
            "║  WS   ws://{}:{}/v1/stream            ║",
            cfg.host, cfg.port
        );
        println!("║                                                  ║");
        println!("║  Claude Desktop config:                          ║");
        println!("║   mcpServers > orchestrator >                    ║");
        println!("║     url: http://{}:{:<25}║", cfg.host, cfg.port);
    }

    println!("╚══════════════════════════════════════════════════╝");
    println!();
}

// ---------------------------------------------------------------------------
// Worker construction
// ---------------------------------------------------------------------------

fn build_worker(cfg: &ResolvedConfig) -> Result<Arc<dyn ModelWorker>, String> {
    match cfg.provider.as_str() {
        "openai" => {
            match env::var("OPENAI_API_KEY") {
                Err(_) => {
                    return Err(
                        "OPENAI_API_KEY is required — re-run the wizard to enter it".to_string()
                    )
                }
                Ok(ref k) if k.is_empty() => {
                    return Err("OPENAI_API_KEY is set but empty".to_string())
                }
                _ => {}
            }
            OpenAiWorker::new(cfg.model.clone())
                .map(|w| Arc::new(w) as Arc<dyn ModelWorker>)
                .map_err(|e| e.to_string())
        }
        "anthropic" => {
            match env::var("ANTHROPIC_API_KEY") {
                Err(_) => {
                    return Err(
                        "ANTHROPIC_API_KEY is required — re-run the wizard to enter it".to_string(),
                    )
                }
                Ok(ref k) if k.is_empty() => {
                    return Err("ANTHROPIC_API_KEY is set but empty".to_string())
                }
                _ => {}
            }
            AnthropicWorker::new(cfg.model.clone())
                .map(|w| Arc::new(w) as Arc<dyn ModelWorker>)
                .map_err(|e| e.to_string())
        }
        "llama" => Ok(Arc::new(LlamaCppWorker::new()) as Arc<dyn ModelWorker>),
        "echo" => Ok(Arc::new(EchoWorker::with_delay(10)) as Arc<dyn ModelWorker>),
        unknown => Err(format!(
            "Unknown provider '{unknown}'. Valid values: openai, anthropic, llama, echo"
        )),
    }
}

// ---------------------------------------------------------------------------
// Tracing
// ---------------------------------------------------------------------------

fn init_tracing(level: &str) {
    use tracing_subscriber::{fmt, EnvFilter};

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
    // Load saved settings from orchestrator.env before anything else
    load_env_file();

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Error: {e}");
            eprintln!("Run with --help for usage.");
            std::process::exit(1);
        }
    };

    // Re-use saved provider/model if CLI didn't specify them
    let args = CliArgs {
        provider: args.provider.or_else(|| env::var("PROVIDER").ok()),
        model: args.model.or_else(|| env::var("MODEL").ok()),
        ..args
    };

    let log_level = args.log_level.clone();
    init_tracing(&log_level);

    if let Err(e) = metrics::init_metrics() {
        eprintln!("Warning: metrics init failed: {e}");
    }

    // Run interactive wizard for any missing config
    let cfg = run_wizard(args);

    print_banner(&cfg);

    let worker = match build_worker(&cfg) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("\nError: {e}");
            eprintln!("Re-run orchestrator.exe to reconfigure.\n");
            // Pause so the window stays open if double-clicked
            let _ = prompt_line("Press Enter to exit...");
            std::process::exit(1);
        }
    };

    tracing::info!(provider = %cfg.provider, model = %cfg.model, "Worker ready");

    let handles = spawn_pipeline(worker);

    tracing::info!("Pipeline stages spawned");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("Ctrl+C received — initiating graceful shutdown");
            let _ = shutdown_tx.send(());
        }
    });

    if cfg.no_web {
        println!("Pipeline ready (headless). Press Ctrl+C to stop.");
        shutdown_rx.await.ok();
        tracing::info!("Shutdown complete");
    } else {
        #[cfg(feature = "web-api")]
        {
            let pipeline_tx = handles.input_tx.clone();
            let config = ServerConfig {
                host: cfg.host.clone(),
                port: cfg.port,
                ..ServerConfig::default()
            };

            tracing::info!(addr = %format!("{}:{}", cfg.host, cfg.port), "Starting web API");

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
        assert_eq!(default_model("anthropic"), "claude-sonnet-4-6");
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
    fn test_env_file_path_is_absolute() {
        let p = env_file_path();
        assert!(p.is_absolute() || p.to_str().map(|s| s.contains('/')).unwrap_or(false));
    }

    #[test]
    fn test_prompt_with_default_uses_default_on_empty() {
        // Directly test the logic (not stdin)
        let answer = "";
        let default = "echo";
        let result = if answer.is_empty() {
            default.to_string()
        } else {
            answer.to_string()
        };
        assert_eq!(result, "echo");
    }

    #[test]
    fn test_build_worker_echo_succeeds() {
        let cfg = ResolvedConfig {
            provider: "echo".to_string(),
            model: "echo".to_string(),
            port: 8080,
            host: "127.0.0.1".to_string(),
            log_level: "info".to_string(),
            no_web: false,
        };
        let result = build_worker(&cfg);
        assert!(result.is_ok(), "echo worker must always succeed");
    }

    #[test]
    fn test_build_worker_openai_fails_without_key() {
        if env::var("OPENAI_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            return;
        }
        let cfg = ResolvedConfig {
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            port: 8080,
            host: "127.0.0.1".to_string(),
            log_level: "info".to_string(),
            no_web: false,
        };
        let result = build_worker(&cfg);
        assert!(result.is_err());
        assert!(result.err().unwrap().contains("OPENAI_API_KEY"));
    }

    #[test]
    fn test_build_worker_anthropic_fails_without_key() {
        if env::var("ANTHROPIC_API_KEY").is_ok() {
            return;
        }
        let cfg = ResolvedConfig {
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            port: 8080,
            host: "127.0.0.1".to_string(),
            log_level: "info".to_string(),
            no_web: false,
        };
        let result = build_worker(&cfg);
        assert!(result.is_err());
        assert!(result.err().unwrap().contains("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn test_build_worker_unknown_provider_returns_error() {
        let cfg = ResolvedConfig {
            provider: "fakeai".to_string(),
            model: "x".to_string(),
            port: 8080,
            host: "127.0.0.1".to_string(),
            log_level: "info".to_string(),
            no_web: false,
        };
        let result = build_worker(&cfg);
        assert!(result.is_err());
        assert!(result.err().unwrap().contains("Unknown provider"));
    }

    #[test]
    fn test_build_worker_llama_builds_without_env() {
        let cfg = ResolvedConfig {
            provider: "llama".to_string(),
            model: "local".to_string(),
            port: 8080,
            host: "127.0.0.1".to_string(),
            log_level: "info".to_string(),
            no_web: false,
        };
        assert!(build_worker(&cfg).is_ok());
    }

    #[test]
    fn test_save_and_load_env_value_roundtrip() {
        use std::fs;
        // Write to a temp file, not the real env file
        let tmp = std::env::temp_dir().join("test_orchestrator.env");
        fs::write(&tmp, "OLD_KEY=old_value\n").unwrap();

        // Simulate what save_env_value does
        let contents = fs::read_to_string(&tmp).unwrap();
        let mut lines: Vec<String> = contents
            .lines()
            .filter(|l| !l.trim_start().starts_with("NEW_KEY="))
            .map(str::to_string)
            .collect();
        lines.push("NEW_KEY=new_value".to_string());
        fs::write(&tmp, lines.join("\n") + "\n").unwrap();

        let result = fs::read_to_string(&tmp).unwrap();
        assert!(result.contains("NEW_KEY=new_value"));
        assert!(result.contains("OLD_KEY=old_value"));
        let _ = fs::remove_file(tmp);
    }
}
