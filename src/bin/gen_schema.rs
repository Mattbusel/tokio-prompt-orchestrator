//! # gen_schema — JSON Schema generator for `PipelineConfig`
//!
//! Derives the JSON Schema for the root [`PipelineConfig`] struct and writes
//! it to `config.schema.json` in the current working directory.
//!
//! ## Usage
//!
//! ```text
//! cargo run --bin gen_schema --features schema
//! ```
//!
//! The resulting `config.schema.json` can be referenced from your editor or
//! from a TOML/JSON config file to get IDE autocompletion and validation.
//!
//! ### VS Code (`settings.json`)
//!
//! ```json
//! {
//!   "evenBetterToml.schema.associations": {
//!     "pipeline\\.toml$": "./config.schema.json"
//!   }
//! }
//! ```
//!
//! ### JetBrains IDEs
//!
//! Add the schema under *Settings → Languages & Frameworks → Schemas and DTDs
//! → JSON Schema Mappings* and point file-pattern `pipeline*.toml` at the
//! generated file.

use std::fs;
use std::io;
use std::path::Path;

use tokio_prompt_orchestrator::config::export_schema;

fn main() -> io::Result<()> {
    let schema_json = export_schema().map_err(|e| {
        io::Error::new(
            io::ErrorKind::Other,
            format!("failed to generate JSON Schema: {e}"),
        )
    })?;

    let out_path = Path::new("config.schema.json");
    fs::write(out_path, &schema_json)?;

    println!(
        "JSON Schema written to {}  ({} bytes)",
        out_path.display(),
        schema_json.len()
    );
    println!();
    println!("Usage tips:");
    println!("  VS Code  — install the 'Even Better TOML' extension, then add to settings.json:");
    println!(
        r#"    "evenBetterToml.schema.associations": {{ "pipeline\\.toml$": "./config.schema.json" }}"#
    );
    println!("  JetBrains — Settings → Languages & Frameworks → JSON Schema Mappings");
    println!("              map file pattern 'pipeline*.toml' to ./config.schema.json");

    Ok(())
}
