use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

use ai_usage::{
    Paths,
    pricing::Pricing,
    server::{ServerConfig, bind_and_start, resolve_bind_addr},
};
use chrono::NaiveDate;
use rusqlite::Connection;
use serde_json::{Value, json};
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

fn claude_message(id: &str, input: u64, output: u64) -> Value {
    json!({
        "sessionId": "main",
        "type": "assistant",
        "timestamp": "2026-11-02T12:00:00Z",
        "message": {
            "id": id,
            "model": "claude-sonnet-5",
            "usage": {
                "input_tokens": input,
                "output_tokens": output,
                "cache_read_input_tokens": 100,
                "cache_creation_input_tokens": 20,
                "cache_creation": { "ephemeral_1h_input_tokens": 10 }
            }
        }
    })
}

fn http_get(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let status_line = head.lines().next().unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (status, body.to_owned())
}

fn open_sse(addr: std::net::SocketAddr) -> BufReader<TcpStream> {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET /api/events HTTP/1.1\r\nHost: {addr}\r\nAccept: text/event-stream\r\n\r\n"
    )
    .unwrap();
    let mut reader = BufReader::new(stream);
    // Read HTTP response headers until blank line
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }
    reader
}

fn read_sse_json(reader: &mut BufReader<TcpStream>) -> Value {
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).unwrap();
        assert!(n > 0, "SSE stream closed unexpectedly");
        let trimmed = line.trim();
        if let Some(data) = trimmed.strip_prefix("data:") {
            let payload = data.trim();
            if !payload.is_empty() {
                return serde_json::from_str(payload).unwrap();
            }
        }
    }
}

#[test]
fn resolve_bind_addr_supports_ports_and_host_pairs() {
    assert_eq!(
        resolve_bind_addr(None, None).unwrap().to_string(),
        "127.0.0.1:8080"
    );
    assert_eq!(
        resolve_bind_addr(Some("3000"), None).unwrap().to_string(),
        "127.0.0.1:3000"
    );
    assert_eq!(
        resolve_bind_addr(Some(":4000"), None).unwrap().to_string(),
        "127.0.0.1:4000"
    );
    assert_eq!(
        resolve_bind_addr(Some("0.0.0.0:9000"), None)
            .unwrap()
            .to_string(),
        "0.0.0.0:9000"
    );
    assert_eq!(
        resolve_bind_addr(Some("127.0.0.1:8080"), Some(5555))
            .unwrap()
            .to_string(),
        "127.0.0.1:5555"
    );
}

#[tokio::test]
async fn server_serves_dashboard_api_filters_and_live_sse_updates() {
    let (_tmp, paths) = setup();
    let claude_log = paths.claude.join("projects/demo/main.jsonl");
    jsonl(
        &claude_log,
        &[
            json!({"type":"custom-title","sessionId":"main","customTitle":"Realtime Claude"}),
            claude_message("msg-1", 10, 5),
        ],
    );

    let config = ServerConfig {
        paths: paths.clone(),
        pricing: Pricing::load(None).unwrap(),
        pricing_path: None,
        providers: vec![],
        since: Some(NaiveDate::from_ymd_opt(2026, 11, 1).unwrap()),
        limit: Some(10),
        title_width: 40,
    };

    let server = bind_and_start("127.0.0.1:0".parse().unwrap(), config)
        .await
        .unwrap();
    let addr = server.local_addr();

    let (html_status, html_body, report_status, report_body) =
        tokio::task::spawn_blocking(move || {
            let (hs, hb) = http_get(addr, "/");
            let (rs, rb) = http_get(addr, "/api/report");
            (hs, hb, rs, rb)
        })
        .await
        .unwrap();

    assert_eq!(html_status, 200);
    assert!(html_body.contains("<!DOCTYPE html>"));
    assert!(html_body.contains("/api/events"));

    assert_eq!(report_status, 200);
    let initial_report: Value = serde_json::from_str(&report_body).unwrap();
    assert_eq!(initial_report["revision"], 1);
    assert_eq!(initial_report["matched"], 1);
    assert_eq!(initial_report["sessions"][0]["title"], "Realtime Claude");
    assert_eq!(initial_report["totals"]["requests"], 1);
    assert_eq!(initial_report["totals"]["input"], 10);
    assert_eq!(initial_report["defaults"]["since"], "2026-11-01");
    assert_eq!(initial_report["defaults"]["limit"], 10);

    // Query parameter filtering on /api/report
    let (filtered_status, filtered_body, bad_status) = tokio::task::spawn_blocking(move || {
        let (fs, fb) = http_get(addr, "/api/report?provider=codex");
        let (bs, _) = http_get(addr, "/api/report?since=invalid-date");
        (fs, fb, bs)
    })
    .await
    .unwrap();
    assert_eq!(filtered_status, 200);
    let filtered_v: Value = serde_json::from_str(&filtered_body).unwrap();
    assert_eq!(filtered_v["matched"], 0);
    assert!(filtered_v["sessions"].as_array().unwrap().is_empty());
    assert_eq!(bad_status, 400);

    // Connect to SSE and watch live updates as usage grows on disk
    let claude_log_clone = claude_log.clone();
    let opencode_dir = paths.opencode.clone();
    let (sse_initial, sse_second, sse_third) = tokio::task::spawn_blocking(move || {
        let mut sse = open_sse(addr);
        let first = read_sse_json(&mut sse);

        // Append a second message to the Claude session JSONL
        jsonl(
            &claude_log_clone,
            &[
                json!({"type":"custom-title","sessionId":"main","customTitle":"Realtime Claude"}),
                claude_message("msg-1", 10, 5),
                claude_message("msg-2", 40, 15),
            ],
        );
        let second = read_sse_json(&mut sse);

        // Now create a brand-new OpenCode V2 SQLite database while the server is running
        fs::create_dir_all(&opencode_dir).unwrap();
        let db = Connection::open(opencode_dir.join("opencode.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY,title TEXT,parent_id TEXT,time_updated INTEGER);
             CREATE TABLE session_message (id TEXT PRIMARY KEY,session_id TEXT,type TEXT,data TEXT,time_updated INTEGER);
             INSERT INTO session_v2 VALUES ('oc-1','Live OpenCode',NULL,1793620800000);",
        )
        .unwrap();
        db.execute(
            "INSERT INTO session_message VALUES ('m1','oc-1','assistant',?1,1793620800000)",
            [json!({"model":{"id":"gpt-6.1-sol"},"cost":0.25,"tokens":{"input":100,"output":20,"cache":{"read":0,"write":0}},"time":{"completed":1793620800000i64}}).to_string()],
        )
        .unwrap();
        drop(db);

        // Wait until SSE yields the update containing OpenCode usage
        let mut latest = read_sse_json(&mut sse);
        while latest["totals"]["requests"].as_u64().unwrap_or(0) < 3 {
            latest = read_sse_json(&mut sse);
        }
        (first, second, latest)
    })
    .await
    .unwrap();

    assert_eq!(sse_initial["revision"], 1);
    assert_eq!(sse_initial["totals"]["requests"], 1);
    assert_eq!(sse_initial["totals"]["input"], 10);

    assert!(sse_second["revision"].as_u64().unwrap() >= 2);
    assert_eq!(sse_second["totals"]["requests"], 2);
    assert_eq!(sse_second["totals"]["input"], 50);
    assert_eq!(sse_second["totals"]["output"], 20);

    assert_eq!(sse_third["totals"]["requests"], 3);
    assert_eq!(sse_third["totals"]["input"], 150);
    assert_eq!(sse_third["matched"], 2);

    server.shutdown().await.unwrap();
}

#[test]
fn cli_serve_flag_starts_server_and_streams_realtime_updates() {
    let (_tmp, p) = setup();
    let claude_log = p.claude.join("projects/p/main.jsonl");
    jsonl(&claude_log, &[claude_message("a", 10, 5)]);

    let mut child = Command::new(env!("CARGO_BIN_EXE_ai-usage"))
        .args(["--serve", "0", "--claude-dir"])
        .arg(&p.claude)
        .arg("--opencode-dir")
        .arg(&p.opencode)
        .arg("--codex-dir")
        .arg(&p.codex)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let mut out_reader = BufReader::new(stdout);
    let mut line = String::new();
    out_reader.read_line(&mut line).unwrap();
    assert!(
        line.starts_with("Listening on http://"),
        "unexpected startup line: {line}"
    );
    let addr_str = line.trim().strip_prefix("Listening on http://").unwrap();
    let addr: std::net::SocketAddr = addr_str.parse().unwrap();

    let mut sse = open_sse(addr);
    let initial = read_sse_json(&mut sse);
    assert_eq!(initial["totals"]["requests"], 1);
    assert_eq!(initial["totals"]["input"], 10);

    // Append more usage and verify real-time push
    jsonl(
        &claude_log,
        &[claude_message("a", 10, 5), claude_message("b", 25, 10)],
    );
    let updated = read_sse_json(&mut sse);
    assert_eq!(updated["totals"]["requests"], 2);
    assert_eq!(updated["totals"]["input"], 35);
    assert_eq!(updated["totals"]["output"], 15);

    let _ = child.kill();
    let _ = child.wait();
}
