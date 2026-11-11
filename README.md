<img src="assets/icon.svg" width="64" height="64" alt="">

# rsusage

Fast Rust rewrite of [ccusage](https://ccusage.com) — token-usage and cost
reporting across coding-agent CLIs, with pluggable readers for more harnesses
than the original.

## Install

```sh
cargo install --path .
```

## Usage

```sh
rsusage                 # merged daily report across all detected agents
rsusage daily --json    # JSON output (ccusage-compatible schema)
rsusage weekly          # merged weekly
rsusage monthly         # merged monthly
rsusage session         # per-session report
rsusage blocks          # 5-hour billing blocks (all agents, not just Claude)
rsusage blocks --live   # live-updating block view (ratatui)
rsusage statusline      # compact status line for Claude Code hooks
```

Per-agent reports:

```sh
rsusage claude daily    rsusage codex weekly     rsusage opencode monthly
rsusage devin daily     rsusage zcode daily      rsusage grok session
rsusage kilo daily      rsusage amp daily        rsusage gemini daily
rsusage cursor daily    rsusage zed daily
```

Common flags: `--json`, `--since/--until`, `--timezone`, `--mode
auto|calculate|display`, `--breakdown`, `--offline`, `--no-cost`, `--compact`,
`--last N`, `--sections daily,weekly,...`, `--by-agent`,
`-w/--start-of-week sunday|monday|...`, blocks: `--active`, `--recent`,
`--token-limit <n|max>`, `--session-length <h>`.

## Data sources

| Agent    | Source                                             | Format  |
|----------|----------------------------------------------------|---------|
| Claude   | `~/.claude/projects/` (or `$CLAUDE_CONFIG_DIR`)    | JSONL   |
| Codex    | `~/.codex/sessions/`                               | JSONL   |
| OpenCode | `~/.local/share/opencode/opencode.db`              | SQLite  |
| Kilo     | `~/.local/share/kilo/kilo.db`                      | SQLite  |
| Devin    | `~/.local/share/devin/cli/sessions.db`             | SQLite  |
| ZCode    | `~/.zcode/cli/db/db.sqlite`                        | SQLite  |
| Grok     | `~/.grok/` session `updates.jsonl`                 | JSONL   |
| Amp      | `~/.config/cliamp/`                                | logs    |
| Gemini   | `~/.gemini/`                                       | JSONL   |
| Cursor   | `~/.cursor/ai-tracking/ai-code-tracking.db`        | SQLite  |
| Zed      | `~/.local/share/zed/threads/threads.db`            | SQLite  |

OpenCode reads both the legacy `message` table and the v2
`session_message`/`session_v2` schema, deduplicating by message id — matching
upstream's coverage of newer OpenCode versions.

## Layout

`main.rs` is argument dispatch only. `app.rs` orchestrates (reader selection,
entry loading, filtering, `--last` windows); `report.rs` aggregates and
serializes rows; `render.rs` draws tables; `agents/` holds readers — a static
registry in `agents/mod.rs` maps ids to constructors. SQLite readers share
`cached_scan` for incremental watermark scanning through the entry cache.
Run tests with `cargo test`.

## Performance notes

- Parallel readers (rayon): JSONL files parsed across threads; big SQLite
  stores are queried in rowid-range chunks with a per-thread connection.
- Single-parse extraction: usage fields are pulled via one multi-path
  `json_extract` per row inside SQLite — no full-document parsing in Rust.
- Incremental entry cache (`~/.cache/rsusage/agent-cache.db`): rowid sources
  only re-scan rows beyond the last watermark plus a tail window, so repeat
  runs skip the multi-GB scan entirely.
- Pricing catalogs (models.dev primary, LiteLLM fallback) are fetched and
  cached under `~/.cache/rsusage/`; `--offline` skips the network fetch.
- Per-model price lookups are memoized — fuzzy matching never runs twice for
  the same model.

Measured on this machine (~5.5 GB of local agent data across Devin, OpenCode,
ZCode, Codex, Grok, Kilo):

| Command                    | ccusage 20.0.49 | rsusage |
|----------------------------|-----------------|---------|
| `daily --json` (warm)      | ~2.3 s          | ~140 ms |
| `daily --json` (cold cache)| ~2.3 s          | ~690 ms |

Cold-cache includes the one-time full scan of the Devin/OpenCode databases;
upstream ccusage does not read Devin at all.

## Compatibility notes

- CLI surface mirrors ccusage (`daily`/`monthly`/`weekly`/`session`/`blocks`/
  `statusline` + agent subcommands). Merged top-level reports use `agent:
  "all"` with `metadata.agents`, per upstream.
- Claude schema verified row-for-row against ccusage 20.0.49 (daily/weekly/
  monthly/session/blocks); remaining diffs are ±1-ulp float formatting.
- Codex, ZCode, Grok, Kilo, OpenCode totals verified identical to upstream.
- Pricing is deterministic: exact match, then first containment match in
  sorted-key order; misses are memoized. Upstream 20.0.50 resolves a few
  free-tier models (`deepseek-v4.1-flash-free` etc.) through its own
  catalog-rules engine — rsusage marks those `missingPricing` and lists them
  under `unpricedModels` in totals, matching upstream's field.
- `--last N` is a period window ("most recent N periods": today/this
  week/this month), matching upstream — weekly windows honor
  `--start-of-week`. Week defaults: Sunday for `claude`, Monday otherwise.
- A stored cost of `0` is treated as "not computed" (harnesses write it for
  free-tier models), so cost falls back to catalog pricing in auto mode.
- Session-row ordering is deterministic (first-activity) rather than
  upstream's filesystem-dependent order.
