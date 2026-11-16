use crate::{
    Provider, Session, Tokens,
    pricing::Pricing,
    util::{content_text, files, jsonl, n, s, timestamp, title},
};
use anyhow::Result;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

pub(super) fn load(
    root: &Path,
    pricing: &Pricing,
    warnings: &mut Vec<String>,
) -> Result<Vec<Session>> {
    let mut sessions: HashMap<String, Session> = HashMap::new();
    // A message may occur multiple times, with growing streaming usage. Keep the latest snapshot.
    let mut messages: HashMap<(String, String), Value> = HashMap::new();
    let mut explicit_titles = HashSet::new();
    for path in files(&root.join("projects"), "jsonl", warnings) {
        let Some(stem) = path.file_stem().and_then(|n| n.to_str()) else {
            continue;
        };
        let child = path
            .parent()
            .is_some_and(|p| p.file_name().is_some_and(|n| n == "subagents"));
        let parent = if child {
            path.parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .and_then(|n| n.to_str())
                .map(str::to_owned)
        } else {
            None
        };
        let mut sequence = 0;
        let result = jsonl(&path, warnings, |v| {
            sequence += 1;
            let id = if child {
                format!("{}:{stem}", parent.as_deref().unwrap_or("unknown"))
            } else {
                s(&v, "sessionId").unwrap_or(stem).to_owned()
            };
            // Ignore metadata without any session identity in unrelated JSONL files.
            let session = sessions
                .entry(id.clone())
                .or_insert_with(|| Session::new(id.clone(), Provider::Claude));
            session.parent = parent.clone();
            session.touch(timestamp(&v["timestamp"]));
            match s(&v, "type").unwrap_or("") {
                "custom-title" | "ai-title" => {
                    // A user title takes precedence over automatically generated titles.
                    let custom = s(&v, "type") == Some("custom-title");
                    if (custom || !explicit_titles.contains(&id))
                        && let Some(t) = s(&v, "customTitle")
                            .or_else(|| s(&v, "aiTitle"))
                            .and_then(title)
                    {
                        session.title = t;
                    }
                    if custom {
                        explicit_titles.insert(id);
                    }
                }
                "summary" if session.title == session.id => {
                    if let Some(t) = s(&v, "summary").and_then(title) {
                        session.title = t;
                    }
                }
                "user" if session.title == session.id => {
                    if let Some(t) = content_text(&v["message"]["content"]) {
                        session.title = t;
                    }
                }
                "assistant" if v["message"]["usage"].is_object() => {
                    let message = &v["message"];
                    if s(message, "model") == Some("<synthetic>") {
                        return;
                    }
                    let key = s(message, "id")
                        .or_else(|| s(&v, "requestId"))
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("{}:{sequence}", path.display()));
                    // Retain only usage metadata, never transcript contents.
                    messages.insert(
                        (id, key),
                        serde_json::json!({"model":message["model"],"usage":message["usage"]}),
                    );
                }
                _ => {}
            }
        });
        if let Err(e) = result {
            warnings.push(format!("claude: {e:#}"));
        }
    }
    for ((id, _), message) in messages {
        let u = &message["usage"];
        let tokens = Tokens {
            input: n(u, "input_tokens"),
            output: n(u, "output_tokens"),
            cache_read: n(u, "cache_read_input_tokens"),
            cache_write: n(u, "cache_creation_input_tokens"),
        };
        let model = s(&message, "model").unwrap_or("unknown");
        let mut cost = pricing.estimate(
            model,
            tokens,
            n(&u["cache_creation"], "ephemeral_1h_input_tokens"),
        );
        if s(u, "inference_geo") == Some("us") {
            cost = cost.map(|c| c * 1.1);
        }
        if s(u, "speed") == Some("fast") {
            cost = cost.map(|c| c * 2.0);
        }
        if let Some(session) = sessions.get_mut(&id) {
            session.usage.record(tokens, cost, true, model);
        }
    }
    // Empty metadata files aren't conversations.
    Ok(sessions
        .into_values()
        .filter(|s| s.last_used.is_some() || s.usage.requests > 0)
        .collect())
}
