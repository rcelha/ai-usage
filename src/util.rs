use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

pub(crate) fn n(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}
pub(crate) fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}
pub(crate) fn timestamp(v: &Value) -> Option<DateTime<Utc>> {
    if let Some(s) = v.as_str() {
        return DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.with_timezone(&Utc));
    }
    v.as_i64().and_then(DateTime::from_timestamp_millis)
}
pub(crate) fn files(root: &Path, extension: &str, warnings: &mut Vec<String>) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    let mut paths = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        match entry {
            Ok(e)
                if e.file_type().is_file()
                    && e.path().extension().is_some_and(|ext| ext == extension) =>
            {
                paths.push(e.into_path())
            }
            Err(e) => warnings.push(format!("{}: {e}", root.display())),
            _ => {}
        }
    }
    paths.sort();
    paths
}
pub(crate) fn jsonl(
    path: &Path,
    warnings: &mut Vec<String>,
    mut visit: impl FnMut(Value),
) -> Result<()> {
    let file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut bad = 0;
    for line in BufReader::new(file).lines() {
        let line = line.with_context(|| format!("reading {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(&line) {
            Ok(v) => visit(v),
            Err(_) => bad += 1,
        }
    }
    if bad > 0 {
        warnings.push(format!(
            "{}: skipped {bad} malformed JSON line(s)",
            path.display()
        ));
    }
    Ok(())
}
pub(crate) fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_reader(BufReader::new(File::open(path)?))
        .with_context(|| format!("parsing {}", path.display()))
}
pub(crate) fn title(text: &str) -> Option<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}
pub(crate) fn content_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return title(text);
    }
    value.as_array()?.iter().find_map(|part| {
        s(part, "text")
            .or_else(|| s(part, "input_text"))
            .and_then(title)
    })
}
pub(crate) fn open_db(path: &Path) -> Result<rusqlite::Connection> {
    let c = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {} read-only", path.display()))?;
    c.busy_timeout(std::time::Duration::from_secs(2))?;
    Ok(c)
}
