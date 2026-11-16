use crate::{Report, Usage};
use std::io::{self, Write};
use tabled::{
    Table,
    builder::Builder,
    settings::{
        Alignment, Padding, Style, Width,
        object::{Columns, Object, Rows},
        style::HorizontalLine,
    },
};

const USAGE_HEADERS: [&str; 7] = [
    "Cost (USD)",
    "Tokens in",
    "Tokens out",
    "Cache reads",
    "Cache writes",
    "Requests",
    "Subagents",
];

fn usage_cells(usage: &Usage, subagents: u64) -> [String; 7] {
    [
        cost(usage),
        number(usage.tokens.input),
        number(usage.tokens.output),
        number(usage.tokens.cache_read),
        number(usage.tokens.cache_write),
        number(usage.requests),
        number(subagents),
    ]
}

fn cost(usage: &Usage) -> String {
    match usage.cost_usd {
        Some(c) => format!("{}${c:.4}", if usage.estimated { "~" } else { "" }),
        None if usage.known_cost_usd > 0.0 => format!(">=${:.4}?", usage.known_cost_usd),
        None => "unknown".to_owned(),
    }
}
fn number(n: u64) -> String {
    let raw = n.to_string();
    let mut out = String::new();
    for (i, c) in raw.chars().enumerate() {
        if i > 0 && (raw.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn write_table(
    out: &mut impl Write,
    report: &Report,
    matched: usize,
    title_width: usize,
) -> io::Result<()> {
    if report.sessions.is_empty() && matched == 0 {
        writeln!(
            out,
            "No sessions found. Use --help for data directory overrides."
        )?;
        return Ok(());
    }
    let mut rows = Builder::default();
    rows.push_record(
        ["Session title", "Provider", "Last used (UTC)"]
            .into_iter()
            .chain(USAGE_HEADERS),
    );
    for s in &report.sessions {
        let title: String = s
            .title
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let mut row = vec![
            title,
            s.provider.to_string(),
            s.last_used
                .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "unknown".to_owned()),
        ];
        row.extend(usage_cells(&s.usage, s.subagents));
        rows.push_record(row);
    }
    let mut sessions = table(rows, 3);
    sessions.modify(
        Columns::first().intersect(Rows::new(1..)),
        Width::truncate(title_width).suffix("…"),
    );
    writeln!(out, "{sessions}")?;
    let t = &report.totals;
    writeln!(
        out,
        "\nShowing {} of {matched} sessions. Total for all matches: {} | {} requests",
        report.sessions.len(),
        cost(t),
        number(t.requests)
    )?;
    writeln!(
        out,
        "Input: {} | Output: {} | Cache reads: {} | Cache writes: {}",
        number(t.tokens.input),
        number(t.tokens.output),
        number(t.tokens.cache_read),
        number(t.tokens.cache_write)
    )?;
    writeln!(
        out,
        "~ estimated at model list prices; >=…? partial cost; input excludes cache tokens. Subagents included in parent totals."
    )?;
    if !t.unpriced_models.is_empty() {
        writeln!(
            out,
            "Missing prices: {} (override with --pricing)",
            t.unpriced_models
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )?;
    }
    writeln!(out, "\nTotals per provider (all matching sessions)")?;
    let mut rows = Builder::default();
    rows.push_record(["Provider", "Sessions"].into_iter().chain(USAGE_HEADERS));
    for total in &report.provider_totals {
        let mut row = vec![total.provider.to_string(), number(total.sessions)];
        row.extend(usage_cells(&total.usage, total.subagents));
        rows.push_record(row);
    }
    writeln!(out, "{}", table(rows, 1))
}

fn table(rows: Builder, numeric_start: usize) -> Table {
    let mut table = rows.build();
    table
        .with(Style::sharp())
        .modify(Columns::last(), Padding::zero())
        .modify(Columns::new(numeric_start..), Alignment::right());
    table
}
