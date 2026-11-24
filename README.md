# ai-usage

A Rust CLI that reads local **OpenCode**, **Claude Code (Anthropic)**, and **Codex** histories and prints session usage, newest activity first.

```sh
cargo run --release -- --limit 20
```

Each row shows the session title, session ID, provider, last use in UTC, USD cost, input tokens, output tokens, cache reads, cache writes, requests, and subagents. Here, “provider” identifies the coding tool. OpenCode can use several underlying model vendors.

## Usage

```sh
# All sessions, sorted by last use descending
cargo run --release

# Only Claude Code and Codex
cargo run --release -- --provider claude,codex

# Sessions active since a UTC date; usage is the session's lifetime total
cargo run --release -- --since 2026-11-01 --limit 20

# Structured output, including full titles, session IDs, and warnings
cargo run --release -- --json > usage.json

# Read a backup or a nonstandard installation
cargo run --release -- --provider codex --codex-dir /path/to/codex-home

# Cost/token breakdown per model for one session (ID, ID prefix, or title text)
cargo run --release -- --session 0199a2b4
cargo run --release -- --session "fix login" --provider claude --json

# Longer titles in the table
cargo run --release -- --title-width 70

# Serve a real-time web report on http://127.0.0.1:8080 (auto-updates via SSE)
cargo run --release -- --serve

# Serve on a custom port or bind address with initial filters
cargo run --release -- --serve 3000 --since 2026-11-01 --limit 20
```

Build a standalone executable with `cargo build --release`, then run `./target/release/ai-usage`. Run `--help` for all options. The final table shows totals per provider: sessions, cost, tokens, cache usage, requests, and subagents. `--limit` limits displayed rows; overall and provider totals include **all matching sessions**. JSON includes the same breakdown in `provider_totals`.

`--session <ID_OR_TITLE>` prints a single session's breakdown per model: cost, share of the session's cost, tokens, cache usage, and requests, with subagent usage attributed to the model that served it. The query is matched as an exact session ID, then a unique ID prefix, then a unique case-insensitive title fragment; an ambiguous query lists the candidates. Session IDs appear in the table's `Session ID` column, in `--json` output, and in the web report. Every session in the JSON report carries the same data in its `models` array.

With `--serve [HOST:]PORT` (or `--port PORT`), `ai-usage` starts a local web server (default `127.0.0.1:8080`), watches the data directories for file and SQLite changes with a fallback poll, and streams live updates to the browser via Server-Sent Events (`/api/events` and `/api/report`) so costs, tokens, and session rows update automatically without refreshing. Click a session row to expand its per-model breakdown; expanded rows keep updating live.

## Data sources

| Tool        | Default location                                        | Records                                                                                    |
| ----------- | ------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| OpenCode V2 | `$XDG_DATA_HOME/opencode`, or `~/.local/share/opencode` | `opencode.db`: `session_v2` and `session_message` tables |
| Claude Code | `$CLAUDE_CONFIG_DIR`, or `~/.claude`                    | `projects/**/*.jsonl`, including `subagents/` logs                                         |
| Codex       | `$CODEX_HOME`, or `~/.codex`                            | `sessions/`, `archived_sessions/`, latest `state_*.sqlite`, and title indices              |

Override these with `--opencode-dir`, `--claude-dir`, and `--codex-dir`. The application works offline, opens SQLite databases read-only, and does not read authentication files. Missing installations are skipped. Unreadable files, malformed JSONL records, and provider errors produce warnings while other records remain available. Warnings appear on stderr for the table and in the JSON report's `warnings` array.

Only OpenCode V2 is supported. Old `session`/`message` tables left behind after migration and V1 JSON storage are not read.

## Counting rules

- Subagents and nested descendants roll into their root session, including costs, tokens, requests, and latest activity. The subagent column counts distinct descendant sessions. An orphaned agent remains a separate row when its parent log is unavailable.
- Input tokens **exclude** cache reads and writes, so the three input categories can be added together. Output includes reasoning tokens when recorded by the tool. Absent token fields are counted as zero; this means no recorded usage in that category, rather than proof of no usage.
- Requests count recorded model responses with usage, rather than user prompts, tool calls, or failed attempts. Claude streaming snapshots share one message ID and count once. Codex requests are inferred from changes in cumulative token counters; repeated snapshots are ignored. Truncated or aggregated records can undercount requests.
- Live and archived copies of a Codex session count once. Inherited token records from a Codex fork are excluded. A normal user fork is its own session; an explicit subagent is attached to its parent.
- Session titles use available title metadata, then the first user message, then the session ID. The table truncates titles; JSON preserves them.
- `--since` filters on a session's last activity, including its agents. It does **not** limit token usage to that date range.

## Costs

OpenCode's recorded costs take precedence, including explicit zero costs. Claude Code and Codex costs are token estimates, marked `~`. A session mixing recorded and estimated amounts is also marked `~`.

The bundled [pricing.json](pricing.json) is an offline snapshot checked on **2026-11-16**, using [Anthropic pricing](https://platform.claude.com/docs/en/about-claude/pricing), [OpenAI pricing](https://developers.openai.com/api/docs/pricing), the [GPT-6 Sol model card](https://developers.openai.com/api/docs/models/gpt-6-sol), the [GPT-5.5 model card](https://developers.openai.com/api/docs/models/gpt-5.5), the [GPT-5.3 Codex model card](https://developers.openai.com/api/docs/models/gpt-5.3-codex), and the [OpenAI enterprise rate card](https://help.openai.com/en/articles/20001415-chatgpt-rate-card-enterprise-token-based-pricing). Other models available in opencode (GitHub Copilot, Meta and opencode providers) use opencode's model metadata, checked on **2026-11-21**. Update this file as prices change.

Estimates use list prices, not your subscription bill. They do not reconstruct historical prices, negotiated discounts, taxes, paid tools, or every context/service-tier premium. Claude's recorded one-hour cache writes, US inference multiplier, and fast-mode multiplier are applied. Other cache writes use the default duration. Large-context tiers are applied where included in the rate file. Claude's persisted `cost-state` snapshots are not added, because they can overlap message and agent usage.

Unknown models never silently cost $0. A partial total displays `>=amount?`; JSON sets `cost_usd` to `null`, preserves `known_cost_usd`, and lists `unpriced_models` and `unpriced_requests`.

Add models or override rates with a JSON file containing **USD per million tokens**:

```json
{
  "my-model": {
    "input": 2.0,
    "output": 10.0,
    "cache_read": 0.2,
    "cache_write": 2.5,
    "cache_write_1h": 4.0
  }
}
```

```sh
cargo run --release -- --pricing /path/to/prices.json
```

Overrides replace individual model entries and keep other bundled models. All four base rates are required; `cache_write_1h` is optional. Exact model IDs take precedence; dated `-YYYYMMDD` and `-YYYY-MM-DD` snapshots fall back to their base model. An optional `above` object contains `tokens` (exclusive prompt threshold) plus a complete alternative rate, as shown in the bundled file.

## Development

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Tests use synthetic temporary histories and databases. They cover deduplication, forked history, subagent rollups, token normalization, recorded and estimated costs, price overrides, malformed records, sorting, filters, and limits.
