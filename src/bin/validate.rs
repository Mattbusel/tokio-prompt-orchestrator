//! # validate — pipeline configuration validator
//!
//! Loads a `pipeline.toml` (or a path supplied via `--config`) and validates
//! its structure and semantic constraints.
//!
//! ## Usage
//!
//! ```bash
//! # Validate the default pipeline.toml in the current directory
//! cargo run --bin validate
//!
//! # Validate a specific file
//! cargo run --bin validate -- --config path/to/pipeline.toml
//! ```
//!
//! Exit codes:
//! - `0` — configuration is valid
//! - `1` — configuration has errors

use std::path::PathBuf;
use tokio_prompt_orchestrator::config::loader::load_from_file;

fn main() {
    let config_path = parse_config_path();

    match load_from_file(&config_path) {
        Ok(config) => {
            println!("✓ Config valid — pipeline: \"{}\"", config.pipeline.name);
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("✗ Config invalid: {e}");
            std::process::exit(1);
        }
    }
}

/// Parse the `--config` / `-c` flag, defaulting to `"pipeline.toml"`.
fn parse_config_path() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" | "-c" => {
                if i + 1 < args.len() {
                    return PathBuf::from(&args[i + 1]);
                } else {
                    eprintln!("error: --config requires a path argument");
                    std::process::exit(1);
                }
            }
            "--help" | "-h" => {
                println!(
                    "Usage: validate [--config <path>]\n\
                     \n  --config, -c <path>  Path to pipeline TOML (default: pipeline.toml)\n  --help,   -h         Show this help"
                );
                std::process::exit(0);
            }
            _ => {}
        }
        i += 1;
    }
    PathBuf::from("pipeline.toml")
}
