use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use ai_usage::{Paths, Provider, Report, collect, display, pricing::Pricing};
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
    /// Show only the N most recently used sessions (totals still include all matches)
    #[arg(short, long)]
    limit: Option<usize>,
    /// Sessions last used on or after this UTC date (YYYY-MM-DD)
    #[arg(long)]
    since: Option<NaiveDate>,
    /// OpenCode data directory (contains opencode.db or storage/)
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
    let mut report = collect(&paths, &args.provider, &pricing);
    if let Some(date) = args.since {
        report
            .sessions
            .retain(|s| s.last_used.is_some_and(|t| t.date_naive() >= date));
    }
    report.totals = Report::totals(&report.sessions);
    report.provider_totals = Report::provider_totals(&report.sessions);
    let matched = report.sessions.len();
    if let Some(limit) = args.limit {
        report.sessions.truncate(limit);
    }
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
