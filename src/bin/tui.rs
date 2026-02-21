//! # Binary: TUI Dashboard
//!
//! ## Responsibility
//! Entry point for the tokio-prompt-orchestrator terminal dashboard.
//! Initializes the terminal, runs the event loop, and ensures clean exit.
//!
//! ## Usage
//! ```bash
//! cargo run --bin tui --features tui              # mock mode (default)
//! cargo run --bin tui --features tui -- --live     # live connection
//! cargo run --bin tui --features tui -- --live --metrics-url http://localhost:9090
//! ```
//!
//! ## Guarantees
//! - Terminal state always restored on exit, even on panic
//! - Clean shutdown on q, Esc, or Ctrl+C
//! - Graceful degradation if terminal does not support colors

use std::io;
use std::time::{Duration, Instant};

use crossterm::event;
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use tokio_prompt_orchestrator::tui::app::App;
use tokio_prompt_orchestrator::tui::events::{apply_event, poll_event};
use tokio_prompt_orchestrator::tui::metrics::{LiveMetrics, MockMetrics};
use tokio_prompt_orchestrator::tui::ui;

/// Render refresh rate: 10 frames per second.
const TICK_RATE: Duration = Duration::from_millis(100);

/// Data update rate: 1 update per second.
const DATA_RATE: Duration = Duration::from_millis(1000);

/// CLI arguments for the TUI binary.
struct CliArgs {
    /// Whether to use live metrics (false = mock mode).
    live: bool,
    /// Prometheus metrics endpoint URL.
    metrics_url: String,
}

/// Parses command-line arguments.
///
/// # Returns
/// Parsed `CliArgs` with defaults applied.
fn parse_args() -> CliArgs {
    let args: Vec<String> = std::env::args().collect();
    let mut cli = CliArgs {
        live: false,
        metrics_url: "http://localhost:9090/metrics".to_string(),
    };

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--live" => cli.live = true,
            "--metrics-url" => {
                i += 1;
                if i < args.len() {
                    cli.metrics_url = args[i].clone();
                }
            }
            _ => {} // Ignore unknown args
        }
        i += 1;
    }

    cli
}

/// Sets up the terminal for TUI rendering.
///
/// # Returns
/// A configured `Terminal` instance, or an IO error.
///
/// # Errors
/// Returns `io::Error` if terminal initialization fails.
fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>, io::Error> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    // Enable mouse capture for potential future use
    execute!(stdout, event::EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

/// Restores the terminal to its original state.
///
/// # Arguments
/// * `terminal` - The terminal instance to restore.
///
/// # Returns
/// `Ok(())` on success, or an IO error.
fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<(), io::Error> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        event::DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = parse_args();

    // Install panic hook that restores terminal before printing panic message
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            event::DisableMouseCapture
        );
        default_hook(info);
    }));

    let mut terminal = setup_terminal()?;
    let mut app = App::new(DATA_RATE);

    let result = if cli.live {
        run_live(&mut terminal, &mut app, &cli.metrics_url)
    } else {
        run_mock(&mut terminal, &mut app)
    };

    restore_terminal(&mut terminal)?;

    if let Err(e) = result {
        eprintln!("TUI error: {}", e);
        std::process::exit(1);
    }

    Ok(())
}

/// Runs the TUI event loop with mock data.
fn run_mock(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<(), Box<dyn std::error::Error>> {
    let mock = MockMetrics::new();
    let mut last_data_tick = Instant::now();

    loop {
        // Render
        terminal.draw(|f| ui::draw(f, app))?;

        // Poll input
        let event = poll_event(TICK_RATE);
        apply_event(app, event);

        if app.should_quit {
            break;
        }

        // Data tick (1hz)
        if last_data_tick.elapsed() >= DATA_RATE && !app.paused {
            mock.tick(app);
            last_data_tick = Instant::now();
        }
    }

    Ok(())
}

/// Runs the TUI event loop with live Prometheus metrics.
fn run_live(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    metrics_url: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let mut live = LiveMetrics::new(metrics_url.to_string());
    let mut last_data_tick = Instant::now();

    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        let event = poll_event(TICK_RATE);
        apply_event(app, event);

        if app.should_quit {
            break;
        }

        if last_data_tick.elapsed() >= DATA_RATE && !app.paused {
            if let Err(e) = rt.block_on(live.tick(app)) {
                app.push_log(tokio_prompt_orchestrator::tui::app::LogEntry {
                    timestamp: chrono::Local::now().format("%H:%M:%S").to_string(),
                    level: tokio_prompt_orchestrator::tui::app::LogLevel::Error,
                    message: "Metrics fetch failed".to_string(),
                    fields: e,
                });
            }
            last_data_tick = Instant::now();
        }
    }

    Ok(())
}
