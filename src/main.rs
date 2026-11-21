use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use ai_usage::{
    Paths, Provider, collect, display,
    pricing::Pricing,
    server::{self, ServerConfig},
};
use anyhow::{Context, Result};
use chrono::NaiveDate;
use clap::Parser;

#[derive(Parser)]
#[command(
    version,
    about = "Session usage across OpenCode, Claude Code, and Codex (local, offline)"
)]
struct Args {
    /// Select tools; repeat or separate with commas [default: all]
    #[arg(short, long, value_enum, value_delimiter = ',')]
    provider: Vec<Provider>,
    /// Emit a JSON report with full titles, session IDs, totals, and warnings
    #[arg(long)]
    json: bool,
    /// Serve a real-time web report on [HOST:]PORT [default: 127.0.0.1:8080]
    #[arg(
        long,
        num_args = 0..=1,
        default_missing_value = "127.0.0.1:8080",
        value_name = "ADDR_OR_PORT"
    )]
    serve: Option<String>,
    /// Port for the real-time web server (implies --serve)
    #[arg(long, value_name = "PORT")]
    port: Option<u16>,
    /// Show only the N most recently used sessions (totals still include all matches)
    #[arg(short, long)]
    limit: Option<usize>,
    /// Sessions last used on or after this UTC date (YYYY-MM-DD)
    #[arg(long)]
    since: Option<NaiveDate>,
    /// OpenCode V2 data directory (contains opencode.db)
    #[arg(long)]
    opencode_dir: Option<PathBuf>,
    /// Claude config directory (contains projects/)
    #[arg(long)]
    claude_dir: Option<PathBuf>,
    /// Codex home directory (contains sessions/ and state_*.sqlite)
    #[arg(long)]
    codex_dir: Option<PathBuf>,
    /// Override per-model USD prices per million tokens using a JSON file
    #[arg(long)]
    pricing: Option<PathBuf>,
    /// Maximum title display width (JSON always contains full titles)
    #[arg(long, default_value_t = 40, value_parser = clap::value_parser!(u16).range(8..=200))]
    title_width: u16,
}

fn run() -> Result<()> {
    let args = Args::parse();
    let mut paths = Paths::discover()?;
    if let Some(p) = args.opencode_dir {
        paths.opencode = p;
    }
    if let Some(p) = args.claude_dir {
        paths.claude = p;
    }
    if let Some(p) = args.codex_dir {
        paths.codex = p;
    }
    let pricing = Pricing::load(args.pricing.as_deref())?;
    if args.serve.is_some() || args.port.is_some() {
        let addr = server::resolve_bind_addr(args.serve.as_deref(), args.port)?;
        return server::run_blocking(
            addr,
            ServerConfig {
                paths,
                pricing,
                pricing_path: args.pricing,
                providers: args.provider,
                since: args.since,
                limit: args.limit,
                title_width: args.title_width,
            },
        );
    }
    let mut report = collect(&paths, &args.provider, &pricing);
    let matched = report.apply_filters(args.since, args.limit);
    let mut out = io::BufWriter::new(io::stdout().lock());
    if args.json {
        serde_json::to_writer_pretty(&mut out, &report)?;
        writeln!(out)?;
    } else {
        display::write_table(&mut out, &report, matched, args.title_width as usize)?;
        for warning in &report.warnings {
            eprintln!("warning: {warning}");
        }
    }
    out.flush().context("writing report")
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e)
            if e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe) =>
        {
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
