use ai_usage::{Paths, Provider, collect, display, pricing::Pricing};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

fn setup() -> (TempDir, Paths) {
    let tmp = TempDir::new().unwrap();
    let paths = Paths {
        opencode: tmp.path().join("opencode"),
        claude: tmp.path().join("claude"),
        codex: tmp.path().join("codex"),
    };
    (tmp, paths)
}
fn jsonl(path: &Path, rows: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        rows.iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
}
fn write_json(path: &Path, value: Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, value.to_string()).unwrap();
}
fn prices() -> Pricing {
    Pricing::load(None).unwrap()
}
fn claude_message(id: &str, input: u64, output: u64) -> Value {
    json!({"sessionId":"main","type":"assistant","timestamp":"2026-11-01T12:00:00Z","message":{"id":id,"model":"claude-sonnet-5","usage":{"input_tokens":input,"output_tokens":output,"cache_read_input_tokens":100,"cache_creation_input_tokens":20,"cache_creation":{"ephemeral_1h_input_tokens":10}}}})
}
fn meta(id: &str) -> Value {
    json!({"type":"session_meta","timestamp":"2026-11-01T12:00:00Z","payload":{"id":id}})
}
fn context() -> Value {
    json!({"type":"turn_context","payload":{"model":"gpt-6.1-sol"}})
}
fn token(time: &str, input: u64, cached: u64, output: u64) -> Value {
    json!({"type":"event_msg","timestamp":time,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":input,"cached_input_tokens":cached,"output_tokens":output,"reasoning_output_tokens":2},"last_token_usage":{"input_tokens":10,"output_tokens":1}}}})
}

#[test]
fn claude_deduplicates_streaming_usage_and_rolls_up_agents() {
    let (_tmp, p) = setup();
    jsonl(
        &p.claude.join("projects/project/main.jsonl"),
        &[
            json!({"type":"user","sessionId":"main","timestamp":"2026-11-01T11:00:00Z","message":{"content":"Original prompt"}}),
            claude_message("a", 10, 2),
            claude_message("a", 10, 5),
            json!({"type":"custom-title","sessionId":"main","customTitle":"My session"}),
            json!({"type":"ai-title","sessionId":"main","aiTitle":"Automatic title"}),
        ],
    );
    let mut child = claude_message("b", 20, 7);
    child["timestamp"] = json!("2026-11-02T12:00:00Z");
    jsonl(
        &p.claude
            .join("projects/project/main/subagents/agent-one.jsonl"),
        &[child],
    );
    let report = collect(&p, &[], &prices());
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.sessions.len(), 1);
    let s = &report.sessions[0];
    assert_eq!(s.title, "My session");
    assert_eq!(s.subagents, 1);
    assert_eq!(s.usage.requests, 2);
    assert_eq!(s.usage.tokens.input, 30);
    assert_eq!(s.usage.tokens.output, 12);
    assert_eq!(s.usage.tokens.cache_read, 200);
    assert_eq!(s.last_used.unwrap().date_naive().to_string(), "2026-11-02");
    // 30*2 + 12*10 + 200*.2 + 20*2.5 + 20*4, per million.
    assert!((s.usage.cost_usd.unwrap() - 0.00035).abs() < 1e-10);
}

#[test]
fn codex_counts_deltas_once_and_excludes_inherited_fork_usage() {
    let (_tmp, p) = setup();
    let first = token("2026-11-01T12:01:00Z", 100, 60, 10);
    let rows = vec![
        meta("root"),
        context(),
        first.clone(),
        first,
        token("2026-11-01T12:02:00Z", 180, 100, 20),
    ];
    jsonl(&p.codex.join("sessions/root.jsonl"), &rows);
    jsonl(&p.codex.join("archived_sessions/root-copy.jsonl"), &rows);
    jsonl(
        &p.codex.join("sessions/child.jsonl"),
        &[
            json!({"type":"session_meta","timestamp":"2026-11-02T12:00:00Z","payload":{"id":"child","forked_from_id":"root","source":{"subagent":{"thread_spawn":{"parent_thread_id":"root"}}}}}),
            context(),
            token("2026-11-01T12:02:00Z", 180, 100, 20),
            token("2026-11-02T12:01:00Z", 230, 120, 25),
        ],
    );
    jsonl(
        &p.codex.join("session_index.jsonl"),
        &[json!({"id":"root","thread_name":"Root title"})],
    );
    let report = collect(&p, &[Provider::Codex], &prices());
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.sessions.len(), 1);
    let s = &report.sessions[0];
    assert_eq!(s.title, "Root title");
    assert_eq!(s.subagents, 1);
    assert_eq!(s.usage.requests, 3);
    assert_eq!(s.usage.tokens.input, 110);
    assert_eq!(s.usage.tokens.cache_read, 120);
    assert_eq!(s.usage.tokens.output, 25); // Reasoning is already included.
    assert!((s.usage.cost_usd.unwrap() - 0.000482).abs() < 1e-10);
}

fn opencode_v2_database(root: &Path) -> Connection {
    fs::create_dir_all(root).unwrap();
    let db = Connection::open(root.join("opencode.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE session_v2 (id TEXT PRIMARY KEY,title TEXT,parent_id TEXT,time_updated INTEGER);
         CREATE TABLE session_message (id TEXT PRIMARY KEY,session_id TEXT,type TEXT,data TEXT,time_updated INTEGER);",
    )
    .unwrap();
    db
}

#[test]
fn opencode_v2_reads_current_sessions_messages_and_nested_agents() {
    let (_tmp, p) = setup();
    let db = opencode_v2_database(&p.opencode);
    db.execute_batch(
        "INSERT INTO session_v2 VALUES
            ('root','Current OpenCode session',NULL,1000),
            ('child','Agent','root',2000),
            ('nested','Nested agent','child',3000),
            ('untitled',NULL,NULL,5000);",
    )
    .unwrap();
    for id in ["root", "child", "nested"] {
        // V2 stores the role in a column and the model ID in a nested object.
        db.execute(
            "INSERT INTO session_message VALUES (?1,?1,'assistant',?2,4000)",
            rusqlite::params![
                id,
                json!({"model":{"providerID":"litellm","id":"unknown-model"},"cost":0.5,"tokens":{"input":20,"output":3,"reasoning":2,"cache":{"read":10,"write":5}},"time":{"created":3500,"completed":4000}}).to_string()
            ],
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO session_message VALUES ('user','root','user',?1,6000)",
        [json!({"text":"Not a model request"}).to_string()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO session_message VALUES ('streaming','root','assistant',?1,7000)",
        [json!({"model":{"id":"unknown-model"},"time":{"created":7000}}).to_string()],
    )
    .unwrap();
    drop(db);
    let before = fs::read(p.opencode.join("opencode.db")).unwrap();

    let report = collect(&p, &[Provider::Opencode], &prices());
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.sessions.len(), 2);
    let s = &report.sessions[0];
    assert_eq!(s.id, "root");
    assert_eq!(s.title, "Current OpenCode session");
    assert_eq!(s.subagents, 2);
    assert_eq!(s.usage.requests, 3);
    assert_eq!(s.usage.cost_usd, Some(1.5));
    assert_eq!(s.usage.tokens.input, 60);
    assert_eq!(s.usage.tokens.output, 15);
    assert_eq!(s.usage.tokens.cache_read, 30);
    assert_eq!(s.usage.tokens.cache_write, 15);
    assert!(!s.usage.estimated);
    assert_eq!(s.last_used.unwrap().timestamp_millis(), 7000);
    assert_eq!(report.sessions[1].title, "untitled");
    assert_eq!(report.sessions[1].usage.requests, 0);
    assert_eq!(fs::read(p.opencode.join("opencode.db")).unwrap(), before);
}

#[test]
fn opencode_v2_ignores_stale_v1_tables() {
    let (_tmp, p) = setup();
    let db = opencode_v2_database(&p.opencode);
    db.execute_batch(
        "CREATE TABLE session (id TEXT,title TEXT,parent_id TEXT,time_updated INTEGER);
         CREATE TABLE message (session_id TEXT,data TEXT,time_updated INTEGER);
         INSERT INTO session VALUES ('migrated','Stale title',NULL,1000),('v1-only','Old session',NULL,2000);
         INSERT INTO session_v2 VALUES ('migrated','Updated title',NULL,3000),('new','New session',NULL,4000);",
    )
    .unwrap();
    for id in ["migrated", "v1-only"] {
        db.execute(
            "INSERT INTO message VALUES (?1,?2,2000)",
            rusqlite::params![
                id,
                json!({"role":"assistant","cost":1.0,"tokens":{"input":10,"output":1}}).to_string()
            ],
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO session_message VALUES ('current','migrated','assistant',?1,3000)",
        [json!({"cost":2.0,"tokens":{"input":20,"output":2}}).to_string()],
    )
    .unwrap();
    drop(db);

    let report = collect(&p, &[Provider::Opencode], &prices());
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.sessions.len(), 2);
    assert!(!report.sessions.iter().any(|s| s.id == "v1-only"));
    let migrated = report.sessions.iter().find(|s| s.id == "migrated").unwrap();
    assert_eq!(migrated.title, "Updated title");
    assert_eq!(migrated.usage.requests, 1);
    assert_eq!(migrated.usage.cost_usd, Some(2.0));
    assert_eq!(report.totals.requests, 1);
    assert_eq!(report.totals.tokens.input, 20);
    assert_eq!(report.totals.cost_usd, Some(2.0));
}

#[test]
fn opencode_v2_estimates_nested_models_and_preserves_explicit_zero_costs() {
    let (_tmp, p) = setup();
    let db = opencode_v2_database(&p.opencode);
    db.execute_batch("INSERT INTO session_v2 VALUES ('root','Costs',NULL,1000);")
        .unwrap();
    for (id, message) in [
        (
            "estimated",
            json!({"model":{"providerID":"openai","id":"gpt-6.1-sol"},"tokens":{"input":100,"output":10,"reasoning":5,"cache":{"read":50,"write":0}}}),
        ),
        (
            "free",
            json!({"model":{"id":"unlisted"},"cost":0.0,"tokens":{"input":10}}),
        ),
    ] {
        db.execute(
            "INSERT INTO session_message VALUES (?1,'root','assistant',?2,2000)",
            rusqlite::params![id, message.to_string()],
        )
        .unwrap();
    }
    drop(db);

    let report = collect(&p, &[Provider::Opencode], &prices());
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.sessions.len(), 1);
    assert_eq!(report.totals.requests, 2);
    assert!(report.totals.estimated);
    assert_eq!(report.totals.unpriced_requests, 0);
    let expected = (100.0 * 2.0 + 15.0 * 10.0 + 50.0 * 0.1) / 1_000_000.0;
    assert!((report.totals.cost_usd.unwrap() - expected).abs() < 1e-10);
}

#[test]
fn opencode_v2_unknown_prices_do_not_silently_become_zero() {
    let (_tmp, p) = setup();
    let db = opencode_v2_database(&p.opencode);
    db.execute_batch("INSERT INTO session_v2 VALUES ('root','Unknown model',NULL,1000);")
        .unwrap();
    db.execute(
        "INSERT INTO session_message VALUES ('m','root','assistant',?1,2000)",
        [json!({"tokens":{"input":10,"output":1},"model":{"id":"unlisted"}}).to_string()],
    )
    .unwrap();
    drop(db);
    let report = collect(&p, &[], &prices());
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.sessions.len(), 1);
    assert_eq!(report.totals.cost_usd, None);
    assert_eq!(report.totals.unpriced_requests, 1);
    assert!(report.totals.unpriced_models.contains("unlisted"));
}

#[test]
fn custom_prices_snapshot_suffix_and_tier_threshold() {
    let (tmp, p) = setup();
    let custom = tmp.path().join("prices.json");
    write_json(
        &custom,
        json!({"claude-sonnet-5":{"input":10,"output":20,"cache_read":1,"cache_write":2,"cache_write_1h":4}}),
    );
    let mut msg = claude_message("a", 10, 5);
    msg["message"]["model"] = json!("claude-sonnet-5-20261101");
    jsonl(&p.claude.join("projects/p/main.jsonl"), &[msg]);
    let report = collect(&p, &[], &Pricing::load(Some(&custom)).unwrap());
    assert!((report.totals.cost_usd.unwrap() - 0.00036).abs() < 1e-10);
    let mut msg = claude_message("a", 100001, 1);
    msg["message"]["model"] = json!("claude-haiku-5-5");
    jsonl(&p.claude.join("projects/p/main.jsonl"), &[msg]);
    let report = collect(&p, &[], &prices());
    assert!((report.totals.cost_usd.unwrap() - 0.05002425).abs() < 1e-10);
}

#[test]
fn malformed_lines_warn_and_other_providers_still_work() {
    let (_tmp, p) = setup();
    jsonl(
        &p.codex.join("sessions/a.jsonl"),
        &[
            meta("a"),
            context(),
            token("2026-11-01T12:00:01Z", 10, 0, 1),
        ],
    );
    use std::io::Write;
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(p.codex.join("sessions/a.jsonl"))
            .unwrap(),
        "{{truncated"
    )
    .unwrap();
    fs::create_dir_all(&p.opencode).unwrap();
    fs::write(p.opencode.join("opencode.db"), "invalid database").unwrap();
    let report = collect(&p, &[], &prices());
    assert_eq!(report.sessions.len(), 1);
    assert_eq!(report.warnings.len(), 2);
    assert!(report.warnings.iter().any(|w| w.contains("malformed")));
}

#[test]
fn cli_sorts_filters_and_limits_without_changing_totals() {
    let (_tmp, p) = setup();
    for (id, date) in [("a", "2026-11-01T12:00:01Z"), ("b", "2026-11-02T12:00:01Z")] {
        jsonl(
            &p.codex.join(format!("sessions/{id}.jsonl")),
            &[meta(id), context(), token(date, 10, 0, 1)],
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_ai-usage"))
        .args([
            "--json",
            "--provider",
            "codex",
            "--limit",
            "1",
            "--since",
            "2026-11-01",
            "--codex-dir",
        ])
        .arg(&p.codex)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(v["sessions"][0]["id"], "b");
    assert_eq!(v["totals"]["requests"], 2);
    assert_eq!(v["provider_totals"][0]["provider"], "codex");
    assert_eq!(v["provider_totals"][0]["sessions"], 2);
    assert_eq!(v["provider_totals"][0]["requests"], 2);
}

#[test]
fn provider_totals_include_hidden_sessions_and_agents_and_respect_filters() {
    let (_tmp, p) = setup();
    let db = opencode_v2_database(&p.opencode);
    db.execute_batch("INSERT INTO session_v2 VALUES ('old','Old session',NULL,1000);")
        .unwrap();
    db.execute(
        "INSERT INTO session_message VALUES ('m','old','assistant',?1,1000)",
        [json!({"tokens":{"input":10,"output":1},"model":{"id":"unlisted"}}).to_string()],
    )
    .unwrap();
    drop(db);
    jsonl(
        &p.claude.join("projects/p/main.jsonl"),
        &[claude_message("a", 10, 5)],
    );
    let mut child = claude_message("b", 20, 7);
    child["timestamp"] = json!("2026-11-02T12:00:00Z");
    jsonl(
        &p.claude.join("projects/p/main/subagents/agent-one.jsonl"),
        &[child],
    );
    for (id, date) in [("a", "2026-11-02T12:00:01Z"), ("b", "2026-11-03T12:00:01Z")] {
        jsonl(
            &p.codex.join(format!("sessions/{id}.jsonl")),
            &[meta(id), context(), token(date, 10, 0, 1)],
        );
    }
    let report = collect(&p, &[], &prices());
    assert_eq!(report.provider_totals.len(), 3);
    assert_eq!(report.provider_totals[0].provider, Provider::Opencode);
    assert_eq!(report.provider_totals[0].usage.cost_usd, None);
    assert_eq!(report.provider_totals[0].usage.unpriced_requests, 1);
    assert_eq!(report.provider_totals[1].sessions, 1);
    assert_eq!(report.provider_totals[1].subagents, 1);
    assert_eq!(report.provider_totals[1].usage.tokens.input, 30);
    assert!((report.provider_totals[1].usage.cost_usd.unwrap() - 0.00035).abs() < 1e-10);
    assert_eq!(report.provider_totals[2].sessions, 2);

    let mut command = Command::new(env!("CARGO_BIN_EXE_ai-usage"));
    command
        .args(["--since", "2026-11-02", "--limit", "0", "--opencode-dir"])
        .arg(&p.opencode)
        .arg("--claude-dir")
        .arg(&p.claude)
        .arg("--codex-dir")
        .arg(&p.codex);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let footer = text
        .split("Totals per provider (all matching sessions)")
        .nth(1)
        .unwrap();
    assert!(footer.contains("claude"));
    assert!(footer.contains("codex"));
    assert!(!footer.contains("opencode"));
    assert!(footer.contains("~$"));
    let output = command.arg("--json").output().unwrap();
    assert!(output.status.success());
    let v: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(v["sessions"].as_array().unwrap().is_empty());
    assert_eq!(v["provider_totals"].as_array().unwrap().len(), 2);
    assert_eq!(v["provider_totals"][0]["provider"], "claude");
    assert_eq!(v["provider_totals"][0]["requests"], 2);
    assert_eq!(v["provider_totals"][0]["subagents"], 1);
    assert!((v["provider_totals"][0]["cost_usd"].as_f64().unwrap() - 0.00035).abs() < 1e-10);
    assert_eq!(v["provider_totals"][1]["provider"], "codex");
    assert_eq!(v["provider_totals"][1]["sessions"], 2);
    assert_eq!(v["totals"]["requests"], 4);
}

#[test]
fn table_handles_unicode_and_terminal_control_characters() {
    let (_tmp, p) = setup();
    jsonl(
        &p.codex.join("sessions/a.jsonl"),
        &[
            meta("a"),
            context(),
            token("2026-11-01T12:00:01Z", 10, 0, 1),
        ],
    );
    jsonl(
        &p.codex.join("session_index.jsonl"),
        &[json!({"id":"a","thread_name":"测试session\u{1b}[31m\nsecond line"})],
    );
    let report = collect(&p, &[], &prices());
    for (width, title) in [
        (8, "测试ses…"),
        (12, "测试session…"),
        (40, "测试session [31m second line"),
    ] {
        let mut output = Vec::new();
        display::write_table(&mut output, &report, 1, width).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(!text.contains('\u{1b}'));
        let table = text.split("\n\n").next().unwrap();
        let lines: Vec<_> = table.lines().collect();
        assert_eq!(lines.len(), 3, "{table}");
        assert!(lines[0].starts_with("Session title"));
        assert!(lines[2].starts_with(title), "{}", lines[2]);
        assert!(lines[2].contains("codex"));
    }
}

#[test]
fn invalid_prices_fail_with_context_and_missing_data_is_empty() {
    let (tmp, p) = setup();
    let report = collect(&p, &[], &prices());
    assert!(report.sessions.is_empty());
    assert_eq!(report.totals.cost_usd, Some(0.0));
    let file = tmp.path().join("prices.json");
    write_json(
        &file,
        json!({"bad":{"input":-1,"output":1,"cache_read":1,"cache_write":1}}),
    );
    assert!(Pricing::load(Some(&file)).is_err());
}

#[test]
fn session_breakdown_splits_usage_per_model_including_subagents() {
    let (_tmp, p) = setup();
    let mut other = claude_message("b", 40, 8);
    other["message"]["model"] = json!("mystery-model");
    jsonl(
        &p.claude.join("projects/project/main.jsonl"),
        &[
            json!({"type":"custom-title","sessionId":"main","customTitle":"Mixed models"}),
            claude_message("a", 10, 5),
            other,
        ],
    );
    jsonl(
        &p.claude
            .join("projects/project/main/subagents/agent-one.jsonl"),
        &[claude_message("c", 20, 7)],
    );
    let report = collect(&p, &[], &prices());
    let s = report.find_session("mixed").unwrap();
    assert_eq!(s.id, "main");
    assert_eq!(report.find_session("ma").unwrap().id, "main");
    assert!(report.find_session("absent").is_err());

    // Priced models come first; the subagent's usage joins its model's row.
    let names: Vec<_> = s.models.iter().map(|m| m.model.as_str()).collect();
    assert_eq!(names, ["claude-sonnet-5", "mystery-model"]);
    let sonnet = &s.models[0].usage;
    assert_eq!(sonnet.requests, 2);
    assert_eq!(sonnet.tokens.input, 30);
    assert_eq!(sonnet.tokens.output, 12);
    assert!((sonnet.cost_usd.unwrap() - 0.00035).abs() < 1e-10);
    let mystery = &s.models[1].usage;
    assert_eq!(mystery.requests, 1);
    assert_eq!(mystery.tokens.input, 40);
    assert_eq!(mystery.cost_usd, None);
    assert_eq!(s.usage.requests, 3);
    assert_eq!(
        s.usage.tokens.input,
        s.models.iter().map(|m| m.usage.tokens.input).sum::<u64>()
    );

    let mut output = Vec::new();
    display::write_session(&mut output, s).unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("Mixed models"), "{text}");
    assert!(text.contains("claude-sonnet-5"), "{text}");
    assert!(text.contains("unknown"), "{text}");
    assert!(text.contains("Total"), "{text}");

    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_ai-usage"))
            .args(args)
            .arg("--claude-dir")
            .arg(&p.claude)
            .args(["--provider", "claude"])
            .output()
            .unwrap()
    };
    let output = run(&["--session", "main", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["id"], "main");
    assert_eq!(v["models"][0]["model"], "claude-sonnet-5");
    assert_eq!(v["models"][0]["requests"], 2);
    assert_eq!(v["models"][1]["cost_usd"], Value::Null);
    assert!(!run(&["--session", "absent"]).status.success());
}
