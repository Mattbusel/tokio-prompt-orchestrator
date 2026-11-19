//! `burn`: what your LLM logs say you paid for twice.
//!
//! ```text
//! # Replay request logs (JSONL or a JSON array; "-" for stdin)
//! burn logs.jsonl
//! burn --ttl 600 --per-user --json litellm_spend_logs.json
//!
//! # Find retry code that can burn money (Python, TypeScript, JavaScript, Rust, Go)
//! burn audit path/to/repo
//! ```
//!
//! See `tokio_prompt_orchestrator::burn` for the categories and the log
//! fields it reads.

use std::io::Read;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use tokio_prompt_orchestrator::burn::{analyze, audit_source, parse_log, BurnOptions, BurnReport};

/// You pay for the same prompt twice. This shows how much.
#[derive(Debug, Parser)]
#[command(name = "burn", version, about, long_about = None, args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Request logs: JSONL files or JSON arrays. `-` (or none) reads stdin.
    files: Vec<PathBuf>,

    /// Seconds after an answer within which an identical request is a
    /// repeat.
    #[arg(long, default_value_t = 300.0)]
    ttl: f64,

    /// Compare requests per user / session instead of across the log.
    #[arg(long)]
    per_user: bool,

    /// Dollar floor per hour of the simulated retry budget.
    #[arg(long, default_value_t = 0.50)]
    retry_budget: f64,

    /// Share of real spend added to the retry budget.
    #[arg(long, default_value_t = 0.10)]
    retry_ratio: f64,

    /// Most repeated requests to list.
    #[arg(long, default_value_t = 10)]
    top: usize,

    /// Price a model yourself, USD per 1M input and output tokens:
    /// `--price gpt-4-0314=30,60`. Wins over the list price; repeatable.
    #[arg(long, value_name = "MODEL=IN,OUT", value_parser = parse_price)]
    price: Vec<(String, (f64, f64))>,

    /// Print the report as JSON.
    #[arg(long)]
    json: bool,

    /// Exit with status 2 when more than this share (0 to 1) of spend went
    /// to duplicates and blind retries, for CI.
    #[arg(long, value_name = "SHARE")]
    fail_over: Option<f64>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Scan source for retry code with no dollar ceiling, retries on every
    /// exception (4xx included) and retry loops that never stop.
    Audit {
        /// Files or folders to scan.
        paths: Vec<PathBuf>,
        /// Print findings as JSON.
        #[arg(long)]
        json: bool,
    },
}

fn parse_price(s: &str) -> Result<(String, (f64, f64)), String> {
    let (model, rest) = s.split_once('=').ok_or("expected MODEL=IN,OUT")?;
    let (i, o) = rest.split_once(',').ok_or("expected MODEL=IN,OUT")?;
    let num = |x: &str| {
        x.trim()
            .parse::<f64>()
            .map_err(|_| format!("not a price: {x}"))
    };
    Ok((model.trim().to_string(), (num(i)?, num(o)?)))
}

fn usd(x: f64) -> String {
    if x >= 100.0 {
        format!("${x:.0}")
    } else if x >= 1.0 {
        format!("${x:.2}")
    } else {
        format!("${x:.4}")
    }
}

fn print_report(r: &BurnReport, opts: &BurnOptions) {
    let hours = ((r.last - r.first) / 3600.0).max(0.0);
    println!(
        "{} calls over {:.1} hours, {} priced, {} total{}",
        r.calls,
        hours,
        r.priced_calls,
        usd(r.total_usd),
        if r.estimated_calls > 0 {
            format!(
                " ({} with token counts estimated from text)",
                r.estimated_calls
            )
        } else {
            String::new()
        }
    );
    if r.skipped > 0 {
        println!("{} records skipped (no time or no request)", r.skipped);
    }
    println!();
    let row = |name: &str, b: &tokio_prompt_orchestrator::burn::Bucket, note: &str| {
        println!(
            "  {name:<34} {:>8} calls {:>12}  {note}",
            b.calls,
            usd(b.usd)
        );
    };
    row(
        "duplicate while first in flight",
        &r.in_flight_duplicate,
        "one call could have served both",
    );
    row(
        &format!("repeat within {:.0}s of an answer", opts.ttl_secs),
        &r.repeat_within_ttl,
        "already paid for",
    );
    row(
        "retry after a 4xx",
        &r.retry_after_4xx,
        "fails the same way every time",
    );
    row(
        "agent repeated action, same result",
        &r.agent_repeat,
        "a full-context call that learned nothing",
    );
    println!("  {}", "-".repeat(70));
    println!(
        "  {:<34} {:>8}       {:>12}  {:.1}% of spend",
        "burned",
        "",
        usd(r.burned_usd),
        r.burned_share() * 100.0
    );
    println!();
    row(
        "retries after 408/429/5xx",
        &r.retry,
        "legitimate, but capped by count only",
    );
    row(
        &format!(
            "  refused by a {} + {:.0}%/h budget",
            usd(opts.retry_budget_usd),
            opts.retry_ratio * 100.0
        ),
        &r.retry_refused_by_budget,
        "what a dollar cap would have saved",
    );
    if r.longest_retry_chain > 1 {
        println!(
            "  longest run of attempts of one request: {}",
            r.longest_retry_chain
        );
    }
    if !r.top_repeated.is_empty() {
        println!();
        println!("Most repeated requests:");
        for t in &r.top_repeated {
            println!(
                "  {}  {:<22} sent {:>5}x, {:>5} wasted {:>10}  {}",
                t.key,
                t.model.chars().take(22).collect::<String>(),
                t.sent,
                t.wasted_calls,
                usd(t.wasted_usd),
                t.preview
            );
        }
    }
    if !r.unpriced_models.is_empty() {
        println!();
        println!(
            "Unpriced (no cost in the log, no list price): {}",
            r.unpriced_models.join(", ")
        );
    }
}

fn read_input(files: &[PathBuf]) -> std::io::Result<Vec<String>> {
    if files.is_empty() || (files.len() == 1 && files[0].as_os_str() == "-") {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        return Ok(vec![s]);
    }
    files.iter().map(std::fs::read_to_string).collect()
}

const SOURCE_EXTS: &[&str] = &["py", "ts", "tsx", "js", "mjs", "cjs", "rs", "go"];
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    "vendor",
    ".tox",
];

fn walk(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        out.push(path.to_path_buf());
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if p.is_dir() {
            if !SKIP_DIRS.contains(&name) {
                walk(&p, out);
            }
        } else if p
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| SOURCE_EXTS.contains(&e))
        {
            out.push(p);
        }
    }
}

fn audit(paths: &[PathBuf], json: bool) -> i32 {
    let mut files = Vec::new();
    for p in paths {
        walk(p, &mut files);
    }
    let mut all = Vec::new();
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        let name = f.to_string_lossy().replace('\\', "/");
        for finding in audit_source(&name, &text) {
            all.push((name.clone(), finding));
        }
    }
    if json {
        let rows: Vec<serde_json::Value> = all
            .iter()
            .map(|(file, f)| {
                serde_json::json!({
                    "file": file, "line": f.line, "kind": f.kind,
                    "why": f.kind.explain(), "code": f.code,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        );
    } else {
        for (file, f) in &all {
            println!("{file}:{}: {}\n    {}", f.line, f.kind.explain(), f.code);
        }
        println!("{} findings in {} files", all.len(), files.len());
    }
    i32::from(!all.is_empty())
}

fn main() {
    let args = Args::parse();
    if let Some(Command::Audit { paths, json }) = &args.command {
        std::process::exit(audit(paths, *json));
    }
    let opts = BurnOptions {
        ttl_secs: args.ttl.max(0.0),
        per_user: args.per_user,
        retry_budget_usd: args.retry_budget.max(0.0),
        retry_ratio: args.retry_ratio.max(0.0),
        top: args.top,
        prices: args.price.iter().cloned().collect(),
    };
    let texts = match read_input(&args.files) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("burn: {e}");
            std::process::exit(1);
        }
    };
    let mut calls = Vec::new();
    let mut skipped = 0;
    for t in &texts {
        let (c, s) = parse_log(t);
        calls.extend(c);
        skipped += s;
    }
    let mut report = analyze(calls, &opts);
    report.skipped = skipped;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        print_report(&report, &opts);
    }
    if let Some(limit) = args.fail_over {
        if report.burned_share() > limit {
            std::process::exit(2);
        }
    }
}
