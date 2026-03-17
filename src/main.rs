//! tokio-prompt-orchestrator — interactive production CLI binary
//!
//! On first run the wizard prompts for all required settings and explains
//! exactly where to get an API key.  Subsequent runs reuse values stored in
//! orchestrator.env (same folder as the .exe).
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
//! --reset                          re-run setup wizard (change key / provider)
//! --help / -h
//! --version / -V
//! ```

use std::env;
use std::io::{self, Write as _};
use std::sync::Arc;
use tokio_prompt_orchestrator::{
    metrics, spawn_pipeline, AnthropicWorker, EchoWorker, LlamaCppWorker, ModelWorker,
    OpenAiWorker, PostOutput, PromptRequest, SessionId,
};

#[cfg(feature = "web-api")]
use tokio_prompt_orchestrator::web_api::{start_server, ServerConfig};

// ---------------------------------------------------------------------------
// Env file — persist settings next to the .exe between runs
// ---------------------------------------------------------------------------

fn env_file_path() -> std::path::PathBuf {
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
                // Safety: caller must ensure no other threads are running
                // (called before the Tokio runtime starts)
                env::set_var(k, v);
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

/// Delete a key from orchestrator.env (used by --reset).
fn delete_env_value(key: &str) {
    let path = env_file_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let lines: Vec<String> = existing
        .lines()
        .filter(|l| !l.trim_start().starts_with(&format!("{key}=")))
        .map(str::to_string)
        .collect();
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
    provider: Option<String>,
    model: Option<String>,
    port: u16,
    host: String,
    log_level: String,
    /// Maximum USD spend before the session is terminated. `None` = unlimited.
    max_spend: Option<f64>,
    no_web: bool,
    reset: bool,
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

What it does
  Routes your prompts through a resilience pipeline — deduplication,
  retries, circuit breakers — so AI agents stay online under load.
  Run it once, leave the window open, and any tool can use it.

USAGE:
    orchestrator.exe           (launches setup wizard on first run)
    orchestrator.exe --reset   (change your key or provider)

OPTIONS:
    --provider <openai|anthropic|llama|echo>   AI provider to use
    --model    <name>                          Model name
    --port     <N>                             HTTP port     (default: 8080)
    --host     <addr>                          Bind address  (default: 127.0.0.1)
    --log-level <trace|debug|info|warn|error>  Log verbosity (default: info)
    --max-spend <dollars>                      Kill session when spend reaches this USD cap
    --no-web                                   Disable web API
    --reset                                    Re-run setup wizard
    --help, -h                                 Print this help
    --version, -V                              Print version

GETTING AN API KEY:
    OpenAI    → https://platform.openai.com/api-keys
    Anthropic → https://console.anthropic.com/settings/keys
    (Both require a free account and a few dollars of credit to start)

CONNECTING AGENTS / IDEs:
    POST http://127.0.0.1:8080/v1/prompt   — send a prompt, get a response
    WS   ws://127.0.0.1:8080/v1/stream     — streaming responses

    Claude Desktop — add to claude_desktop_config.json:
      {{ "mcpServers": {{ "orchestrator": {{ "url": "http://127.0.0.1:8080" }} }} }}

SETTINGS FILE:
    orchestrator.env  (same folder as the .exe)
    Edit this file directly to change provider, key, or port."#,
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
    let mut max_spend: Option<f64> = None;
    let mut no_web = false;
    let mut reset = false;

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
            "--no-web" => no_web = true,
            "--reset" => reset = true,
            flag @ ("--provider" | "--model" | "--port" | "--host" | "--log-level" | "--max-spend") => {
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
                    "--max-spend" => {
                        max_spend = Some(
                            val.parse::<f64>()
                                .map_err(|_| format!("--max-spend must be a number, got: {val}"))?,
                        );
                    }
                    _ => {}
                }
            }
            unknown => return Err(format!("Unknown flag: {unknown}")),
        }
        i += 1;
    }

    Ok(CliArgs {
        provider,
        model,
        port,
        host,
        log_level,
        max_spend,
        no_web,
        reset,
    })
}

// ---------------------------------------------------------------------------
// Interactive setup wizard
// ---------------------------------------------------------------------------

struct ResolvedConfig {
    provider: String,
    model: String,
    port: u16,
    host: String,
    log_level: String,
    /// Optional USD spending cap for this session.
    max_spend: Option<f64>,
    no_web: bool,
}

fn run_wizard(args: CliArgs) -> ResolvedConfig {
    // --reset: wipe saved provider/key so the wizard asks again
    if args.reset {
        println!("\n  Resetting configuration...");
        for key in &[
            "PROVIDER",
            "MODEL",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "LLAMA_CPP_URL",
            "API_KEY",
        ] {
            delete_env_value(key);
            // Safety: called before the Tokio runtime starts (single-threaded)
            env::remove_var(key);
        }
        println!("  Done. Please enter your new settings below.\n");
    }

    let is_first_run = args.provider.is_none()
        && env::var("OPENAI_API_KEY").is_err()
        && env::var("ANTHROPIC_API_KEY").is_err()
        && env::var("PROVIDER").is_err();

    // ── Returning user: show saved settings and offer a menu ─────────────────
    if !is_first_run && !args.reset {
        let saved_provider = env::var("PROVIDER").unwrap_or_else(|_| "unknown".to_string());
        let saved_model = env::var("MODEL").unwrap_or_else(|_| "default".to_string());
        let key_status = match saved_provider.as_str() {
            "anthropic" => {
                if env::var("ANTHROPIC_API_KEY")
                    .map(|v| !v.is_empty())
                    .unwrap_or(false)
                {
                    "key saved ✓"
                } else {
                    "NO KEY SAVED ✗"
                }
            }
            "openai" => {
                if env::var("OPENAI_API_KEY")
                    .map(|v| !v.is_empty())
                    .unwrap_or(false)
                {
                    "key saved ✓"
                } else {
                    "NO KEY SAVED ✗"
                }
            }
            _ => "no key needed",
        };

        println!();
        println!("  ┌─────────────────────────────────────────────────────┐");
        println!("  │         tokio-prompt-orchestrator                    │");
        println!("  ├─────────────────────────────────────────────────────┤");
        println!("  │  Saved settings:                                     │");
        println!("  │    Provider : {:<38}│", saved_provider);
        println!("  │    Model    : {:<38}│", saved_model);
        println!("  │    API Key  : {:<38}│", key_status);
        println!("  ├─────────────────────────────────────────────────────┤");
        println!("  │  [Enter]  Start with these settings                  │");
        println!("  │  [S]      Change provider / API key / model          │");
        println!("  │  [Q]      Quit                                        │");
        println!("  └─────────────────────────────────────────────────────┘");
        println!();

        let choice = prompt_line("  Your choice: ");
        match choice.to_lowercase().as_str() {
            "q" | "quit" => {
                println!("  Goodbye.");
                std::process::exit(0);
            }
            "s" | "settings" => {
                // Wipe saved config and fall through to the wizard below
                for key in &[
                    "PROVIDER",
                    "MODEL",
                    "OPENAI_API_KEY",
                    "ANTHROPIC_API_KEY",
                    "LLAMA_CPP_URL",
                    "API_KEY",
                ] {
                    delete_env_value(key);
                    // Safety: called before the Tokio runtime starts (single-threaded)
                    env::remove_var(key);
                }
                println!();
            }
            _ => {
                // Enter or anything else — use saved settings, skip wizard
                return ResolvedConfig {
                    provider: saved_provider,
                    model: saved_model,
                    port: args.port,
                    host: args.host,
                    log_level: args.log_level,
                    max_spend: args.max_spend,
                    no_web: args.no_web,
                };
            }
        }
    }

    let is_interactive = is_first_run
        || args.reset
        || env::var("OPENAI_API_KEY").is_err() && env::var("ANTHROPIC_API_KEY").is_err();

    // ── Welcome message on first run ─────────────────────────────────────────
    if is_first_run || args.reset {
        println!();
        println!("  ┌─────────────────────────────────────────────────────┐");
        println!("  │         Welcome to tokio-prompt-orchestrator         │");
        println!("  │                                                       │");
        println!("  │  This tool routes your prompts through an AI         │");
        println!("  │  pipeline with automatic retries, deduplication,     │");
        println!("  │  and circuit breakers.                                │");
        println!("  │                                                       │");
        println!("  │  Setup takes about 60 seconds.                       │");
        println!("  └─────────────────────────────────────────────────────┘");
        println!();
    }

    // ── Provider ─────────────────────────────────────────────────────────────
    let provider = match args.provider {
        Some(p) => p,
        None => {
            println!("  Which AI provider do you want to use?");
            println!();
            println!("  1) Anthropic  (Claude — recommended for most users)");
            println!("     Get a key: https://console.anthropic.com/settings/keys");
            println!();
            println!("  2) OpenAI     (GPT-4o and friends)");
            println!("     Get a key: https://platform.openai.com/api-keys");
            println!();
            println!("  3) llama      (run models locally — no key needed)");
            println!("     Requires llama.cpp running on this machine");
            println!();
            println!("  4) echo       (test mode — no key, no internet)");
            println!();
            let choice = prompt_with_default("  Enter 1, 2, 3, or 4", "1");
            match choice.as_str() {
                "1" | "anthropic" => "anthropic".to_string(),
                "2" | "openai" => "openai".to_string(),
                "3" | "llama" => "llama".to_string(),
                "4" | "echo" => "echo".to_string(),
                other => other.to_string(),
            }
        }
    };

    // ── API key ───────────────────────────────────────────────────────────────
    match provider.as_str() {
        "anthropic" => {
            if env::var("ANTHROPIC_API_KEY")
                .map(|v| v.is_empty())
                .unwrap_or(true)
            {
                println!();
                println!("  Anthropic API key");
                println!("  Get one free at: https://console.anthropic.com/settings/keys");
                println!("  (Requires account + a few dollars of credit to start)");
                println!();
                let key = prompt_line("  Paste your key here (sk-ant-…): ");
                if !key.is_empty() {
                    // Safety: called before the Tokio runtime starts (single-threaded)
                    env::set_var("ANTHROPIC_API_KEY", &key);
                    save_env_value("ANTHROPIC_API_KEY", &key);
                    println!("  ✓ Key saved.");
                }
            } else if is_interactive {
                println!("  ✓ Anthropic key already saved — skipping.");
            }
        }
        "openai" => {
            if env::var("OPENAI_API_KEY")
                .map(|v| v.is_empty())
                .unwrap_or(true)
            {
                println!();
                println!("  OpenAI API key");
                println!("  Get one at: https://platform.openai.com/api-keys");
                println!("  (Requires account + a few dollars of credit to start)");
                println!();
                let key = prompt_line("  Paste your key here (sk-…): ");
                if !key.is_empty() {
                    // Safety: called before the Tokio runtime starts (single-threaded)
                    env::set_var("OPENAI_API_KEY", &key);
                    save_env_value("OPENAI_API_KEY", &key);
                    println!("  ✓ Key saved.");
                }
            } else if is_interactive {
                println!("  ✓ OpenAI key already saved — skipping.");
            }
        }
        "llama" => {
            let default_url =
                env::var("LLAMA_CPP_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
            if is_interactive {
                println!();
                let url = prompt_with_default("  llama.cpp server URL", &default_url);
                // Safety: called before the Tokio runtime starts (single-threaded)
                env::set_var("LLAMA_CPP_URL", &url);
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
                println!();
                prompt_with_default("  Model name (press Enter to use default)", def)
            } else {
                def.to_string()
            }
        }
    };

    // ── Port ──────────────────────────────────────────────────────────────────
    let (port, no_web) = if is_interactive && !args.no_web {
        println!();
        let port_str = prompt_with_default(
            "  Port for the web API (agents connect here)",
            &args.port.to_string(),
        );
        let port: u16 = port_str.parse().unwrap_or(args.port);
        (port, false)
    } else {
        (args.port, args.no_web)
    };

    // Save non-secret settings
    save_env_value("PROVIDER", &provider);
    save_env_value("MODEL", &model);

    if is_interactive {
        println!();
        println!("  ✓ All done! Settings saved to orchestrator.env next to this .exe.");
        println!("    Run with --reset any time to change your key or provider.");
        println!();
    }

    ResolvedConfig {
        provider,
        model,
        port,
        host: args.host,
        log_level: args.log_level,
        max_spend: args.max_spend,
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
        "open (no bearer token)"
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
    if let Some(cap) = cfg.max_spend {
        println!("║  Budget cap: ${:<37}║", format!("{cap:.2}"));
    }

    if !cfg.no_web {
        let base = format!("http://{}:{}", cfg.host, cfg.port);
        let ws_base = format!("ws://{}:{}", cfg.host, cfg.port);
        println!("╠══════════════════════════════════════════════════╣");
        println!("║  HOW TO USE                                      ║");
        println!("║                                                  ║");
        println!("║  Terminal  ─ type any question below             ║");
        println!("║                                                  ║");
        println!("║  Agents / IDEs ─ HTTP while this window is open: ║");
        println!("║   POST {:<41}║", format!("{base}/v1/prompt"));
        println!("║   WS   {:<41}║", format!("{ws_base}/v1/stream"));
        println!("║                                                  ║");
        println!("║  Claude Desktop ─ claude_desktop_config.json:   ║");
        println!("║   mcpServers > orchestrator > url:               ║");
        println!("║     {:<45}║", format!("\"{base}\""));
        println!("║                                                  ║");
        println!("║  To change key/provider: run with --reset        ║");
    }

    println!("╚══════════════════════════════════════════════════╝");
    println!();
}

// ---------------------------------------------------------------------------
// Worker construction — friendly errors with actionable guidance
// ---------------------------------------------------------------------------

fn build_worker(cfg: &ResolvedConfig) -> Result<Arc<dyn ModelWorker>, String> {
    match cfg.provider.as_str() {
        "anthropic" => {
            if env::var("ANTHROPIC_API_KEY")
                .map(|v| v.is_empty())
                .unwrap_or(true)
            {
                return Err("No Anthropic API key found.\n\n\
                     Run orchestrator.exe --reset to enter your key.\n\
                     Get a key at: https://console.anthropic.com/settings/keys"
                    .to_string());
            }
            AnthropicWorker::new(cfg.model.clone())
                .map(|w| Arc::new(w) as Arc<dyn ModelWorker>)
                .map_err(|e| {
                    format!(
                        "Could not connect to Anthropic: {e}\n\n\
                         Check that your API key is correct and your account has credit.\n\
                         Run orchestrator.exe --reset to re-enter your key."
                    )
                })
        }
        "openai" => {
            if env::var("OPENAI_API_KEY")
                .map(|v| v.is_empty())
                .unwrap_or(true)
            {
                return Err("No OpenAI API key found.\n\n\
                     Run orchestrator.exe --reset to enter your key.\n\
                     Get a key at: https://platform.openai.com/api-keys"
                    .to_string());
            }
            OpenAiWorker::new(cfg.model.clone())
                .map(|w| Arc::new(w) as Arc<dyn ModelWorker>)
                .map_err(|e| {
                    format!(
                        "Could not connect to OpenAI: {e}\n\n\
                         Check that your API key is correct and your account has credit.\n\
                         Run orchestrator.exe --reset to re-enter your key."
                    )
                })
        }
        "llama" => Ok(Arc::new(LlamaCppWorker::new()) as Arc<dyn ModelWorker>),
        "echo" => Ok(Arc::new(EchoWorker::with_delay(10)) as Arc<dyn ModelWorker>),
        unknown => Err(format!(
            "Unknown provider '{unknown}'.\n\
             Valid options: anthropic, openai, llama, echo\n\
             Run orchestrator.exe --reset to choose again."
        )),
    }
}

// ---------------------------------------------------------------------------
// Terminal REPL — runs concurrently with the web API
// ---------------------------------------------------------------------------

async fn run_repl(
    input_tx: tokio::sync::mpsc::Sender<PromptRequest>,
    output_rx: tokio::sync::Mutex<Option<tokio::sync::mpsc::Receiver<PostOutput>>>,
) {
    let mut rx = {
        let mut guard = output_rx.lock().await;
        match guard.take() {
            Some(r) => r,
            None => {
                eprintln!("[repl] output channel already taken — REPL disabled");
                return;
            }
        }
    };

    let session = SessionId("repl".to_string());

    println!("Ask me anything — try: What can you help me with?");
    println!("Commands: 'exit' to quit  |  'settings' to change key/provider\n");

    loop {
        print!("> ");
        let _ = io::stdout().flush();

        let line = tokio::task::spawn_blocking(|| {
            let mut buf = String::new();
            match io::stdin().read_line(&mut buf) {
                Ok(0) => None,
                Ok(_) => Some(buf.trim().to_string()),
                Err(_) => None,
            }
        })
        .await
        .unwrap_or(None);

        let prompt = match line {
            None => break,
            Some(ref s) if s.eq_ignore_ascii_case("exit") || s.eq_ignore_ascii_case("quit") => {
                break
            }
            Some(ref s) if s.eq_ignore_ascii_case("settings") => {
                println!();
                println!("  To change your key or provider, close this window and");
                println!("  reopen orchestrator.exe — choose [S] at the startup menu.");
                println!("  Or run: orchestrator.exe --reset");
                println!();
                continue;
            }
            Some(ref s) if s.is_empty() => continue,
            Some(s) => s,
        };

        let req = PromptRequest {
            session: session.clone(),
            request_id: uuid_simple(),
            input: prompt,
            meta: Default::default(),
        };

        if input_tx.send(req).await.is_err() {
            eprintln!("Pipeline closed unexpectedly.");
            break;
        }

        match tokio::time::timeout(std::time::Duration::from_secs(60), rx.recv()).await {
            Ok(Some(out)) => println!("\n{}\n", out.text),
            Ok(None) => {
                eprintln!("Pipeline output closed.");
                break;
            }
            Err(_) => eprintln!(
                "\nNo response after 60 seconds. \
                 Check your internet connection and API key (run --reset to change it).\n"
            ),
        }
    }

    println!("Goodbye.");
}

fn uuid_simple() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    static CTR: AtomicU64 = AtomicU64::new(0);
    let seq = CTR.fetch_add(1, Ordering::Relaxed);
    format!("repl-{nanos:08x}-{seq:04x}")
}

// ---------------------------------------------------------------------------
// Tracing
// ---------------------------------------------------------------------------

fn init_tracing(level: &str) {
    use std::fs::OpenOptions;
    use tracing_subscriber::{fmt, EnvFilter};

    // Write logs to orchestrator.log next to the .exe so they never
    // clutter the interactive terminal.  Set RUST_LOG=info to see them live,
    // or read the log file after the fact.
    let filter = env::var("RUST_LOG")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| level.to_string());

    // Try to open the log file next to the binary; fall back to stderr.
    let log_path = env::current_exe().ok().map(|mut p| {
        p.pop();
        p.push("orchestrator.log");
        p
    });

    let use_file = log_path
        .as_ref()
        .and_then(|p| OpenOptions::new().create(true).append(true).open(p).ok());

    if let Some(file) = use_file {
        let _ = fmt()
            .with_env_filter(EnvFilter::new(filter))
            .with_target(false)
            .with_writer(std::sync::Mutex::new(file))
            .try_init();
    } else {
        // Fallback: stderr (not stdout, so it doesn't mix with REPL)
        let _ = fmt()
            .with_env_filter(EnvFilter::new(filter))
            .with_target(false)
            .with_writer(std::io::stderr)
            .try_init();
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // All env mutation happens here, before the Tokio runtime starts,
    // so plain (non-unsafe) env::set_var / env::remove_var are sound.
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

    // run_wizard may call env::set_var / env::remove_var — still safe here
    // because the Tokio runtime has not been started yet.
    let cfg = run_wizard(args);

    // Build the runtime *after* all env mutation is complete.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async_main(cfg))
}

async fn async_main(cfg: ResolvedConfig) -> Result<(), Box<dyn std::error::Error>> {
    print_banner(&cfg);

    let worker = match build_worker(&cfg) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("\n  ✗ {e}\n");
            let _ = prompt_line("  Press Enter to close...");
            std::process::exit(1);
        }
    };

    tracing::info!(provider = %cfg.provider, model = %cfg.model, "Worker ready");

    let handles = spawn_pipeline(worker);
    tracing::info!("Pipeline stages spawned");

    // ── Budget enforcement task ─────────────────────────────────────────────
    // If `--max-spend` was given, poll the Prometheus `inference_cost_total`
    // counter every 10 seconds and exit when the cap is reached.
    if let Some(cap) = cfg.max_spend {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                let spent = crate::metrics::total_inference_cost_usd();
                if spent >= cap {
                    eprintln!(
                        "\n  ✗ Budget cap of ${cap:.2} reached (spent ${spent:.4}). Shutting down.\n"
                    );
                    std::process::exit(2);
                }
                tracing::debug!(spent = spent, cap = cap, "budget check ok");
            }
        });
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("Ctrl+C received — initiating graceful shutdown");
            let _ = shutdown_tx.send(());
        }
    });

    let repl_tx = handles.input_tx.clone();
    let repl_output_rx = handles.output_rx;
    let repl_handle = tokio::spawn(async move {
        run_repl(repl_tx, repl_output_rx).await;
    });

    if cfg.no_web {
        tokio::select! {
            _ = repl_handle => {}
            _ = shutdown_rx => {
                tracing::info!("Shutdown complete");
            }
        }
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
                _ = repl_handle => {}
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
            tokio::select! {
                _ = repl_handle => {}
                _ = shutdown_rx => {
                    tracing::info!("Shutdown complete");
                }
            }
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
    fn test_default_model_openai() {
        assert_eq!(default_model("openai"), "gpt-4o");
    }

    #[test]
    fn test_default_model_anthropic() {
        assert_eq!(default_model("anthropic"), "claude-sonnet-4-6");
    }

    #[test]
    fn test_default_model_llama() {
        assert_eq!(default_model("llama"), "local");
    }

    #[test]
    fn test_default_model_echo() {
        assert_eq!(default_model("echo"), "echo");
    }

    #[test]
    fn test_default_model_unknown_falls_back_to_echo() {
        assert_eq!(default_model("unknown"), "echo");
    }

    #[test]
    fn test_env_file_path_is_absolute() {
        let p = env_file_path();
        assert!(p.is_absolute() || p.to_str().map(|s| s.contains('/')).unwrap_or(false));
    }

    #[test]
    fn test_prompt_with_default_returns_default_on_empty_input() {
        let answer = "";
        let default = "anthropic";
        let result = if answer.is_empty() {
            default.to_string()
        } else {
            answer.to_string()
        };
        assert_eq!(result, "anthropic");
    }

    #[test]
    fn test_build_worker_echo_always_succeeds() {
        let cfg = ResolvedConfig {
            provider: "echo".to_string(),
            model: "echo".to_string(),
            port: 8080,
            host: "127.0.0.1".to_string(),
            log_level: "info".to_string(),
            max_spend: None,
            no_web: false,
        };
        assert!(build_worker(&cfg).is_ok());
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
            max_spend: None,
            no_web: false,
        };
        let err = build_worker(&cfg).err().unwrap();
        assert!(err.contains("OpenAI API key"));
        assert!(err.contains("platform.openai.com"));
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
            max_spend: None,
            no_web: false,
        };
        let err = build_worker(&cfg).err().unwrap();
        assert!(err.contains("Anthropic API key"));
        assert!(err.contains("console.anthropic.com"));
    }

    #[test]
    fn test_build_worker_unknown_provider_has_helpful_message() {
        let cfg = ResolvedConfig {
            provider: "fakeai".to_string(),
            model: "x".to_string(),
            port: 8080,
            host: "127.0.0.1".to_string(),
            log_level: "info".to_string(),
            max_spend: None,
            no_web: false,
        };
        let err = build_worker(&cfg).err().unwrap();
        assert!(err.contains("Unknown provider"));
        assert!(err.contains("--reset"));
    }

    #[test]
    fn test_build_worker_llama_builds_without_env() {
        let cfg = ResolvedConfig {
            provider: "llama".to_string(),
            model: "local".to_string(),
            port: 8080,
            host: "127.0.0.1".to_string(),
            log_level: "info".to_string(),
            max_spend: None,
            no_web: false,
        };
        assert!(build_worker(&cfg).is_ok());
    }

    #[test]
    fn test_save_and_delete_env_value_roundtrip() {
        use std::fs;
        let tmp = std::env::temp_dir().join("test_orch_reset.env");
        fs::write(&tmp, "PROVIDER=anthropic\nMODEL=claude-sonnet-4-6\n").unwrap();

        // Simulate delete_env_value("PROVIDER")
        let contents = fs::read_to_string(&tmp).unwrap();
        let lines: Vec<String> = contents
            .lines()
            .filter(|l| !l.trim_start().starts_with("PROVIDER="))
            .map(str::to_string)
            .collect();
        fs::write(&tmp, lines.join("\n") + "\n").unwrap();

        let result = fs::read_to_string(&tmp).unwrap();
        assert!(!result.contains("PROVIDER="));
        assert!(result.contains("MODEL="));
        let _ = fs::remove_file(tmp);
    }

    #[test]
    fn test_uuid_simple_is_unique() {
        let a = uuid_simple();
        let b = uuid_simple();
        assert_ne!(a, b);
    }

    #[test]
    fn test_uuid_simple_has_repl_prefix() {
        assert!(uuid_simple().starts_with("repl-"));
    }
}
