use crate::{
    Provider, Session, Tokens,
    pricing::Pricing,
    util::{content_text, files, jsonl, n, open_db, read_json, s, timestamp, title},
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
};

#[derive(Default)]
struct Metadata {
    title: String,
    parent: Option<String>,
    updated: Option<DateTime<Utc>>,
    path: PathBuf,
}

fn parent(source: &Value) -> Option<String> {
    source
        .pointer("/subagent/thread_spawn/parent_thread_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

pub(super) fn load(
    root: &Path,
    pricing: &Pricing,
    warnings: &mut Vec<String>,
) -> Result<Vec<Session>> {
    let mut metadata: HashMap<String, Metadata> = HashMap::new();
    let index = root.join("session_index.jsonl");
    if index.exists()
        && let Err(e) = jsonl(&index, warnings, |v| {
            if let (Some(id), Some(name)) = (s(&v, "id"), s(&v, "thread_name")) {
                metadata.entry(id.to_owned()).or_default().title = name.to_owned();
            }
        })
    {
        warnings.push(format!("codex index: {e:#}"));
    }
    let global = root.join(".codex-global-state.json");
    if global.exists()
        && let Ok(v) = read_json(&global)
        && let Some(titles) = v
            .pointer("/thread-titles/titles")
            .and_then(Value::as_object)
    {
        for (id, name) in titles {
            if let Some(name) = name.as_str() {
                let m = metadata.entry(id.clone()).or_default();
                if m.title.is_empty() {
                    m.title = name.to_owned();
                }
            }
        }
    }
    // Choose the newest schema version, rather than combining stale copies of state.
    let mut databases = Vec::new();
    for e in std::fs::read_dir(root)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(version) = name
            .strip_prefix("state_")
            .and_then(|s| s.strip_suffix(".sqlite"))
            .and_then(|s| s.parse::<u32>().ok())
        {
            databases.push((version, e.path()));
        }
    }
    databases.sort_by_key(|(version, _)| *version);
    if let Some((_, path)) = databases.last()
        && let Err(e) = database_metadata(path, &mut metadata)
    {
        warnings.push(format!("codex metadata: {e:#}"));
    }
    let mut paths: BTreeSet<PathBuf> = files(&root.join("sessions"), "jsonl", warnings)
        .into_iter()
        .collect();
    paths.extend(files(&root.join("archived_sessions"), "jsonl", warnings));
    for m in metadata.values() {
        if !m.path.as_os_str().is_empty() {
            paths.insert(m.path.clone());
        }
    }
    let path_ids: HashMap<_, _> = metadata
        .iter()
        .filter(|(_, m)| !m.path.as_os_str().is_empty())
        .map(|(id, m)| (m.path.clone(), id.clone()))
        .collect();
    let mut sessions: HashMap<String, Session> = HashMap::new();
    for path in paths {
        match rollout(
            &path,
            path_ids.get(&path).map(String::as_str),
            pricing,
            warnings,
        ) {
            Ok(Some(mut session)) => {
                if let Some(m) = metadata.get(&session.id) {
                    if !m.title.is_empty() {
                        session.title = m.title.clone();
                    }
                    session.parent = m.parent.clone().or(session.parent);
                    session.touch(m.updated);
                }
                // An archived and live copy can coexist. Keep one complete snapshot.
                if sessions.get(&session.id).is_none_or(|old| {
                    (session.usage.requests, session.last_used)
                        > (old.usage.requests, old.last_used)
                }) {
                    sessions.insert(session.id.clone(), session);
                }
            }
            Ok(None) => {}
            Err(e) => warnings.push(format!("codex: {e:#}")),
        }
    }
    Ok(sessions.into_values().collect())
}

fn database_metadata(path: &Path, metadata: &mut HashMap<String, Metadata>) -> Result<()> {
    let db = open_db(path)?;
    let mut stmt = db.prepare("SELECT id,title,rollout_path,source,updated_at FROM threads")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;
    for row in rows {
        let (id, title, path, source, updated) = row?;
        metadata.insert(
            id,
            Metadata {
                title,
                parent: serde_json::from_str(&source).ok().as_ref().and_then(parent),
                path: PathBuf::from(path),
                updated: DateTime::from_timestamp(updated, 0),
            },
        );
    }
    Ok(())
}

fn counters(v: &Value) -> Tokens {
    Tokens {
        input: n(v, "input_tokens"),
        output: n(v, "output_tokens"),
        cache_read: n(v, "cached_input_tokens"),
        cache_write: n(v, "cache_write_input_tokens"),
    }
}
fn difference(now: Tokens, previous: Tokens) -> Tokens {
    Tokens {
        input: now.input.saturating_sub(previous.input),
        output: now.output.saturating_sub(previous.output),
        cache_read: now.cache_read.saturating_sub(previous.cache_read),
        cache_write: now.cache_write.saturating_sub(previous.cache_write),
    }
}

fn rollout(
    path: &Path,
    fallback_id: Option<&str>,
    pricing: &Pricing,
    warnings: &mut Vec<String>,
) -> Result<Option<Session>> {
    let mut session = Session::new(fallback_id.unwrap_or("").to_owned(), Provider::Codex);
    let mut model = "unknown".to_owned();
    let mut previous: Option<Tokens> = None;
    let mut created = None;
    let mut forked = false;
    let mut has_title = false;
    jsonl(path, warnings, |v| {
        let p = &v["payload"];
        let time = timestamp(&v["timestamp"]);
        match s(&v, "type").unwrap_or("") {
            "session_meta" => {
                if let Some(id) = s(p, "id") {
                    session.id = id.to_owned();
                    session.title = id.to_owned();
                }
                session.parent = parent(&p["source"]);
                created = timestamp(&p["timestamp"]).or(time);
                forked = s(p, "forked_from_id").is_some() || session.parent.is_some();
            }
            "turn_context" => {
                if let Some(m) = s(p, "model") {
                    model = m.to_owned();
                }
            }
            "event_msg" if s(p, "type") == Some("user_message") && !has_title => {
                if let Some(t) = s(p, "message").and_then(title) {
                    session.title = t;
                    has_title = true;
                }
            }
            "response_item" if s(p, "role") == Some("user") && !has_title => {
                if let Some(t) = content_text(&p["content"])
                    && !t.starts_with("<environment_context>")
                    && !t.starts_with("# AGENTS.md")
                {
                    session.title = t;
                    has_title = true;
                }
            }
            "event_msg" if s(p, "type") == Some("token_count") && p["info"].is_object() => {
                let info = &p["info"];
                let total = info["total_token_usage"]
                    .is_object()
                    .then(|| counters(&info["total_token_usage"]));
                let last = info["last_token_usage"]
                    .is_object()
                    .then(|| counters(&info["last_token_usage"]));
                let inherited = forked && time.zip(created).is_some_and(|(t, c)| t < c);
                let delta = match (total, previous) {
                    (Some(now), Some(before)) if now == before => return,
                    (Some(now), Some(before))
                        if now.input >= before.input && now.output >= before.output =>
                    {
                        difference(now, before)
                    }
                    (Some(now), None) if !forked => now,
                    _ => match last {
                        Some(last) => last,
                        None => return,
                    },
                };
                previous = total.or(previous);
                if inherited {
                    return;
                }
                // Codex input includes cached input (and explicit cache writes).
                let tokens = Tokens {
                    input: delta
                        .input
                        .saturating_sub(delta.cache_read)
                        .saturating_sub(delta.cache_write),
                    ..delta
                };
                if tokens == Tokens::default() {
                    return;
                }
                session
                    .usage
                    .record(tokens, pricing.estimate(&model, tokens, 0), true, &model);
            }
            _ => {}
        }
        session.touch(time);
    })?;
    Ok((!session.id.is_empty()).then_some(session))
}
