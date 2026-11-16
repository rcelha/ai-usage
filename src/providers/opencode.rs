use crate::{
    Provider, Session, Tokens,
    pricing::Pricing,
    util::{files, n, open_db, read_json, s, timestamp},
};
use anyhow::Result;
use serde_json::Value;
use std::{collections::HashMap, path::Path};

pub(super) fn load(
    root: &Path,
    pricing: &Pricing,
    warnings: &mut Vec<String>,
) -> Result<Vec<Session>> {
    let db = root.join("opencode.db");
    if db.exists() {
        return database(&db, pricing, warnings);
    }
    legacy(root, pricing, warnings)
}

fn database(path: &Path, pricing: &Pricing, warnings: &mut Vec<String>) -> Result<Vec<Session>> {
    let db = open_db(path)?;
    let mut sessions = HashMap::new();
    let mut stmt = db.prepare("SELECT id, title, parent_id, time_updated FROM session")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    for row in rows {
        let (id, title, parent, updated) = row?;
        let mut session = Session::new(id.clone(), Provider::Opencode);
        session.title = title;
        session.parent = parent;
        session.touch(timestamp(&Value::from(updated)));
        sessions.insert(id, session);
    }
    // JSON extraction avoids loading user prompts and tool output from a large database.
    let mut stmt = db.prepare("SELECT session_id, json_object('role',json_extract(data,'$.role'),'modelID',json_extract(data,'$.modelID'),'cost',json_extract(data,'$.cost'),'tokens',json_extract(data,'$.tokens'),'time',json_extract(data,'$.time')), time_updated FROM message WHERE json_extract(data,'$.role') = 'assistant'")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (id, raw, updated) = row?;
        if let Some(session) = sessions.get_mut(&id) {
            match serde_json::from_str(&raw) {
                Ok(message) => {
                    message_usage(session, &message, pricing);
                    session.touch(timestamp(&Value::from(updated)));
                }
                Err(e) => warnings.push(format!("opencode message in {id}: {e}")),
            }
        }
    }
    Ok(sessions.into_values().collect())
}

fn message_usage(session: &mut Session, message: &Value, pricing: &Pricing) {
    if s(message, "role") != Some("assistant") || !message["tokens"].is_object() {
        return;
    }
    let t = &message["tokens"];
    let tokens = Tokens {
        input: n(t, "input"),
        output: n(t, "output") + n(t, "reasoning"),
        cache_read: n(&t["cache"], "read"),
        cache_write: n(&t["cache"], "write"),
    };
    let model = s(message, "modelID").unwrap_or("unknown");
    let reported = message["cost"]
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0);
    session.usage.record(
        tokens,
        reported.or_else(|| pricing.estimate(model, tokens, 0)),
        reported.is_none(),
        model,
    );
    session.touch(
        timestamp(&message["time"]["completed"]).or_else(|| timestamp(&message["time"]["created"])),
    );
}

fn legacy(root: &Path, pricing: &Pricing, warnings: &mut Vec<String>) -> Result<Vec<Session>> {
    let storage = root.join("storage");
    let mut sessions = HashMap::new();
    for path in files(&storage.join("session"), "json", warnings) {
        match read_json(&path) {
            Ok(v) => {
                let Some(id) = s(&v, "id") else {
                    continue;
                };
                let mut session = Session::new(id.to_owned(), Provider::Opencode);
                session.title = s(&v, "title").unwrap_or(id).to_owned();
                session.parent = s(&v, "parentID").map(str::to_owned);
                session.touch(timestamp(&v["time"]["updated"]));
                sessions.insert(id.to_owned(), session);
            }
            Err(e) => warnings.push(format!("{}: {e:#}", path.display())),
        }
    }
    for path in files(&storage.join("message"), "json", warnings) {
        match read_json(&path) {
            Ok(v) => {
                let id = s(&v, "sessionID")
                    .map(str::to_owned)
                    .or_else(|| path.parent()?.file_name()?.to_str().map(str::to_owned));
                if let Some(session) = id.and_then(|id| sessions.get_mut(&id)) {
                    message_usage(session, &v, pricing);
                }
            }
            Err(e) => warnings.push(format!("{}: {e:#}", path.display())),
        }
    }
    Ok(sessions.into_values().collect())
}
