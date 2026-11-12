//! JSONL-transcript readers: Claude Code, Codex, Grok, Gemini, generic fallback.
use super::sqlite::{file_sig, sig_scan};
use super::{fnum, is_jsonl, num, parse_ts, strv, walk, AgentReader};
use crate::paths::home;
use crate::types::{Tokens, UsageEntry, UNKNOWN_MODEL};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

fn read_jsonl(path: &Path, mut f: impl FnMut(&serde_json::Value)) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    for line in BufReader::new(file).lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.len() < 3 {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        f(&v);
    }
}

/// Claude Code: `~/.claude/projects/**/*.jsonl` (+ CLAUDE_CONFIG_DIR, legacy
/// `~/.config/claude`). Entries are `type=assistant` lines with `message.usage`.
pub struct Claude;

fn claude_files() -> Vec<PathBuf> {
    let mut files = vec![];
    for d in claude_dirs() {
        walk(&d, &is_jsonl, &mut files, 8);
    }
    files.sort();
    files
}

fn claude_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![];
    if let Ok(cfg) = std::env::var("CLAUDE_CONFIG_DIR") {
        for d in cfg.split(':') {
            if !d.is_empty() {
                dirs.push(PathBuf::from(d).join("projects"));
            }
        }
    }
    dirs.push(home().join(".claude/projects"));
    dirs.push(home().join(".config/claude/projects"));
    dirs
}

fn claude_entry(
    v: &serde_json::Value,
    agent: &'static str,
    project: &str,
    fallback_session: &str,
) -> Option<UsageEntry> {
    let msg = v.get("message")?;
    let usage = msg.get("usage")?;
    if !usage.is_object() {
        return None;
    }
    let tokens = Tokens {
        input: num(usage, "input_tokens"),
        output: num(usage, "output_tokens"),
        reasoning: 0,
        cache_creation: num(usage, "cache_creation_input_tokens"),
        cache_read: num(usage, "cache_read_input_tokens"),
    };
    if tokens.is_zero() {
        return None;
    }
    let ts = parse_ts(v.get("timestamp").unwrap_or(&serde_json::Value::Null))?;
    let model = strv(msg, "model").unwrap_or(UNKNOWN_MODEL).to_string();
    // upstream derives session id from the transcript file stem
    let session = fallback_session.to_string();
    let dedup_key = match (
        strv(msg, "id"),
        strv(v, "requestId").or_else(|| strv(v, "request_id")),
    ) {
        (Some(m), Some(r)) => format!("{m}:{r}"),
        _ => String::new(),
    };
    // upstream reports the raw encoded project dir name, not cwd
    let project = project.to_string();
    Some(UsageEntry {
        agent,
        timestamp_ms: ts,
        session_id: session.into(),
        model: model.into(),
        provider: "".into(),
        tokens,
        cost_usd: fnum(v, "costUSD").or_else(|| fnum(v, "cost_usd")),
        dedup_key: dedup_key.into(),
        dedup_id: 0,
                total_override: None,
                is_fallback: false,
        project: project.into(),
    })
}

impl AgentReader for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }
    fn display(&self) -> &'static str {
        "Claude"
    }
    fn detect(&self) -> bool {
        claude_dirs().iter().any(|d| d.is_dir())
    }
    fn cache_key(&self) -> Option<String> {
        file_sig(&claude_files())
    }

    fn entries(&self) -> Vec<UsageEntry> {
        let agent = self.id();
        let files = claude_files();
        let sig = file_sig(&files);
        sig_scan("claude", agent, sig, move || -> Vec<UsageEntry> {
            files
                .par_iter()
                .flat_map(|path| {
                    let mut out = vec![];
                    // project = dir under projects/, session fallback = file stem
                    let project = path
                        .parent()
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let fallback = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    read_jsonl(path, |v| {
                        if let Some(e) = claude_entry(v, agent, &project, &fallback) {
                            out.push(e);
                        }
                    });
                    out
                })
                .collect()
        })
    }
}

/// Codex: `~/.codex/sessions/**/rollout-*.jsonl`. `token_usage_record` events
/// carry per-request usage; `turn_context` supplies the model per turn.
pub struct Codex;

fn codex_files() -> Vec<PathBuf> {
    let mut files = vec![];
    walk(&codex_dir(), &is_jsonl, &mut files, 8);
    files.sort();
    files
}

fn codex_dir() -> PathBuf {
    std::env::var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".codex"))
        .join("sessions")
}

fn usage_from(v: &serde_json::Value) -> Tokens {
    Tokens {
        input: num(v, "input_tokens"),
        output: num(v, "output_tokens"),
        reasoning: num(v, "reasoning_output_tokens"),
        cache_creation: num(v, "cache_write_input_tokens"),
        cache_read: num(v, "cached_input_tokens"),
    }
}

impl AgentReader for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn display(&self) -> &'static str {
        "Codex"
    }
    fn detect(&self) -> bool {
        codex_dir().is_dir()
    }
    fn cache_key(&self) -> Option<String> {
        file_sig(&codex_files())
    }

    fn entries(&self) -> Vec<UsageEntry> {
        let agent = self.id();
        let files = codex_files();
        let sig = file_sig(&files);
        let sessions_dir = codex_dir();
        sig_scan("codex", agent, sig, move || -> Vec<UsageEntry> {
            files
                .par_iter()
                .flat_map(|path| {
                    let session_id = codex_session_id(&sessions_dir, path);
                    let mut out: Vec<UsageEntry> = vec![];
                    // upstream: `current_model` tracks the last turn_context
                    // model file-wide; a request record's model (when present)
                    // resolves through a request-local copy and does not
                    // advance it.
                    let mut current_model: Option<String> = None;
                    let mut current_is_fallback = false;
                    let mut cwd = String::new();
                    let mut has_records = false;
                    let mut legacy: Vec<UsageEntry> = vec![];
                    // a record flushed by a `compacted` marker (or arriving
                    // after one) is usage replayed from a compacted window —
                    // upstream flags it `isFallback`. Records pending a marker
                    // are retro-flagged via `pending_idx`.
                    let mut compacted_ids: FxHashSet<String> = FxHashSet::default();
                    let mut pending_idx: FxHashMap<String, usize> = FxHashMap::default();
                    // upstream's session `lastActivity` is the max timestamp
                    // across emitted usage events — event_msg/token_count
                    // lines emit an event even when their usage dedupes to
                    // nothing, so track the max over usage-kind lines too
                    let mut max_usage_ts: i64 = 0;
                    let mut last_model: Option<String> = None;
                    read_jsonl(path, |v| {
                        let ty = strv(v, "type").unwrap_or("");
                        let p = v.get("payload").cloned().unwrap_or_default();
                        if matches!(ty, "token_usage_record" | "compacted")
                            || (ty == "event_msg" && strv(&p, "type") == Some("token_count"))
                        {
                            if let Some(t) = parse_ts(
                                v.get("timestamp").unwrap_or(&serde_json::Value::Null),
                            ) {
                                max_usage_ts = max_usage_ts.max(t);
                            }
                        }
                        match ty {
                            "session_meta" => {
                                cwd = strv(&p, "cwd").unwrap_or("").to_string();
                            }
                            "turn_context" => {
                                if let Some(m) = codex_model_of(&p) {
                                    current_model = Some(m);
                                    current_is_fallback = false;
                                }
                            }
                            "compacted" => {
                                if let Some(id) = strv(&p, "compaction_response_id")
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                                {
                                    compacted_ids.insert(id.to_string());
                                    if let Some(idx) = pending_idx.remove(id) {
                                        out[idx].is_fallback = true;
                                    }
                                }
                            }
                            "token_usage_record" => {
                                has_records = true;
                                let Some(response_id) = strv(&p, "response_id")
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                                else {
                                    return;
                                };
                                let u = p.get("usage").cloned().unwrap_or_default();
                                let tokens = usage_from(&u);
                                if tokens.is_zero() {
                                    return;
                                }
                                let ts_str =
                                    strv(&v, "timestamp").unwrap_or("").to_string();
                                let Some(ts) = parse_ts(
                                    v.get("timestamp").unwrap_or(&serde_json::Value::Null),
                                ) else {
                                    return;
                                };
                                let parsed = codex_model_of(&p);
                                let mut request_model = current_model.clone();
                                let mut request_fallback = current_is_fallback;
                                let (model, is_fallback) = resolve_codex_usage_model(
                                    parsed,
                                    &ts_str,
                                    &mut request_model,
                                    &mut request_fallback,
                                );
                                let is_fallback =
                                    is_fallback || compacted_ids.contains(response_id);
                                let model =
                                    model.unwrap_or_else(|| UNKNOWN_MODEL.into());
                                last_model = Some(model.clone());
                                if !compacted_ids.contains(response_id) {
                                    pending_idx
                                        .entry(response_id.to_string())
                                        .or_insert(out.len());
                                }
                                out.push(UsageEntry {
                                    agent,
                                    timestamp_ms: ts,
                                    session_id: session_id.clone().into(),
                                    model: model.into(),
                                    provider: "".into(),
                                    // raw input_tokens stay in `input`; the
                                    // report subtracts cached+creation at
                                    // output time, and `total_override` keeps
                                    // the source's own total (excl. reasoning)
                                    tokens,
                                    cost_usd: None,
                                    dedup_key: response_id.into(),
                                    dedup_id: 0,
                                    total_override: u.get("total_tokens").and_then(|t| t.as_u64()),
                                    is_fallback,
                                    project: cwd.clone().into(),
                                });
                            }
                            // older rollouts: event_msg token_count with per-request `last_token_usage`
                            "event_msg" if strv(&p, "type") == Some("token_count") => {
                                let info = p.get("info").cloned().unwrap_or_default();
                                let u = info.get("last_token_usage").cloned().unwrap_or_default();
                                let tokens = usage_from(&u);
                                if tokens.is_zero() {
                                    return;
                                }
                                let ts_str =
                                    strv(&v, "timestamp").unwrap_or("").to_string();
                                let Some(ts) = parse_ts(
                                    v.get("timestamp").unwrap_or(&serde_json::Value::Null),
                                ) else {
                                    return;
                                };
                                let parsed = codex_model_of(&p).or_else(|| {
                                    info.get("model").and_then(|m| m.as_str()).map(str::to_string)
                                });
                                let mut request_model = current_model.clone();
                                let mut request_fallback = current_is_fallback;
                                let (model, is_fallback) = resolve_codex_usage_model(
                                    parsed,
                                    &ts_str,
                                    &mut request_model,
                                    &mut request_fallback,
                                );
                                let model =
                                    model.unwrap_or_else(|| UNKNOWN_MODEL.into());
                                last_model = Some(model.clone());
                                let ord = num(v, "ordinal");
                                legacy.push(UsageEntry {
                                    agent,
                                    timestamp_ms: ts,
                                    session_id: session_id.clone().into(),
                                    model: model.into(),
                                    provider: "".into(),
                                    tokens,
                                    cost_usd: None,
                                    dedup_key: format!("{session_id}:{ord}").into(),
                                    dedup_id: 0,
                                    total_override: u.get("total_tokens").and_then(|t| t.as_u64()),
                                    is_fallback,
                                    project: cwd.clone().into(),
                                });
                            }
                            _ => {}
                        }
                    });
                    if !has_records {
                        out.append(&mut legacy);
                    }
                    // sentinel carrying the file's last usage-line timestamp:
                    // zero tokens keep every aggregate untouched while the
                    // session group's `lastActivity` picks it up
                    if let (Some(m), true) = (last_model, max_usage_ts > 0) {
                        out.push(UsageEntry {
                            agent,
                            timestamp_ms: max_usage_ts,
                            session_id: session_id.clone().into(),
                            model: m.into(),
                            provider: "".into(),
                            tokens: Tokens::default(),
                            cost_usd: None,
                            dedup_key: format!("{session_id}:lastact").into(),
                            dedup_id: 0,
                            total_override: Some(0),
                            is_fallback: false,
                            project: cwd.into(),
                        });
                    }
                    out
                })
                .collect()
        })
    }
}

/// `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<id>.jsonl` → the same relative
/// path minus its extension — upstream's session id for codex reports.
fn codex_session_id(sessions_dir: &std::path::Path, path: &std::path::Path) -> String {
    let rel = path.strip_prefix(sessions_dir).unwrap_or(path);
    rel.with_extension("")
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

/// `model` / `model_name` / `metadata.model` — upstream's payload model pick.
fn codex_model_of(v: &serde_json::Value) -> Option<String> {
    let non_empty = |k: &serde_json::Value| {
        k.as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    v.get("model")
        .and_then(non_empty)
        .or_else(|| v.get("model_name").and_then(non_empty))
        .or_else(|| v.get("metadata").and_then(|m| m.get("model")).and_then(non_empty))
}

/// Dated `codex-auto-review` → real-model fallback table, newest first —
/// generated from the models.dev snapshot upstream embeds.
const CODEX_AUTO_REVIEW_FALLBACKS: &[(&str, &str)] = &[
    ("2026-07-30", "gpt-5.6-luna"),
    ("2026-03-05", "gpt-5.4"),
    ("2026-02-05", "gpt-5.3-codex"),
    ("2025-12-11", "gpt-5.2-codex"),
    ("2025-11-13", "gpt-5.1-codex"),
    ("2025-09-15", "gpt-5-codex"),
    ("2025-08-07", "gpt-5"),
];

/// Upstream `codex_log_model_fallback`: a `codex-auto-review` model resolves
/// to the newest real model released on/before the event date (`gpt-5` when
/// undated or older than every entry).
fn codex_log_model_fallback<'a>(model: &'a str, timestamp: &str) -> Option<&'a str> {
    if model != "codex-auto-review" {
        return None;
    }
    let date = timestamp.get(..10);
    Some(
        date.and_then(|d| {
            CODEX_AUTO_REVIEW_FALLBACKS
                .iter()
                .find_map(|(on, m)| (d >= *on).then_some(*m))
        })
        .unwrap_or("gpt-5"),
    )
}

/// Upstream `resolve_codex_usage_model` (20.0.28 shape — request records
/// resolve on a request-local copy; the flag ORs the auto-review map).
fn resolve_codex_usage_model(
    parsed: Option<String>,
    timestamp: &str,
    current: &mut Option<String>,
    current_fallback: &mut bool,
) -> (Option<String>, bool) {
    if let Some(m) = parsed.as_ref() {
        *current = Some(m.clone());
        *current_fallback = false;
    }
    let mut is_fallback = false;
    let model = parsed.or_else(|| current.clone()).or_else(|| {
        is_fallback = true;
        *current_fallback = true;
        *current = Some("gpt-5".to_string());
        current.clone()
    });
    // `parsed_model_is_missing`: inferred model (flag still set) marks the
    // record a fallback.
    if model.is_some() && current.is_some() && *current_fallback {
        is_fallback = true;
    }
    (
        model.map(|m| {
            codex_log_model_fallback(&m, timestamp)
                .map(|f| {
                    is_fallback = true;
                    f.to_string()
                })
                .unwrap_or(m)
        }),
        is_fallback,
    )
}

/// Grok: `~/.grok/sessions/<cwd>/<sid>/updates.jsonl`. Lines are
/// `{method, params:{sessionId, update:{prompt_id, usage, modelUsage}}, timestamp}`
/// where `update.usage` is a cumulative snapshot per prompt — keep the last.
pub struct Grok;

fn grok_files() -> Vec<PathBuf> {
    let mut files = vec![];
    walk(
        &grok_dir(),
        &|p| p.file_name().is_some_and(|n| n == "updates.jsonl"),
        &mut files,
        6,
    );
    files.sort();
    files
}

fn grok_dir() -> PathBuf {
    home().join(".grok/sessions")
}

fn grok_usage(u: &serde_json::Value) -> Tokens {
    // upstream `split_input_tokens`: inputTokens includes cache reads and is
    // also the ceiling for cache writes; reasoning is never in the total
    let input = num(u, "inputTokens");
    let cr = num(u, "cachedReadTokens").min(input);
    let uncached = input - cr;
    let cc = num(u, "cacheCreationTokens").min(uncached);
    Tokens {
        input: uncached - cc,
        output: num(u, "outputTokens"),
        reasoning: 0,
        cache_creation: cc,
        cache_read: cr,
    }
}

/// Grok reports cost in 1e-10 USD ticks; zero ticks means "not recorded".
fn grok_ticks(u: &serde_json::Value) -> Option<f64> {
    fnum(u, "costUsdTicks").filter(|&t| t > 0.0).map(|t| t / 1e10)
}

impl AgentReader for Grok {
    fn id(&self) -> &'static str {
        "grok"
    }
    fn display(&self) -> &'static str {
        "Grok"
    }
    fn detect(&self) -> bool {
        grok_dir().is_dir()
    }
    fn cache_key(&self) -> Option<String> {
        file_sig(&grok_files())
    }

    fn entries(&self) -> Vec<UsageEntry> {
        let agent = self.id();
        let files = grok_files();
        let sig = file_sig(&files);
        sig_scan("grok", agent, sig, move || -> Vec<UsageEntry> {
            files
                .par_iter()
                .flat_map(|path| {
                    let meta = grok_session_meta(path);
                    let mut seen = std::collections::HashSet::new();
                    let mut out = vec![];
                    read_jsonl(path, |v| {
                        let Some(upd) = v.pointer("/params/update") else {
                            return;
                        };
                        // upstream reads `turn_completed` rows only — each is a
                        // finished turn's usage, not a snapshot to reduce
                        if strv(upd, "sessionUpdate") != Some("turn_completed") {
                            return;
                        }
                        let Some(u) = upd.get("usage") else { return };
                        if !u.is_object() {
                            return;
                        }
                        let meta_v = v.pointer("/params/_meta");
                        // `agentTimestampMs` (ms) beats envelope `timestamp`
                        // (seconds); both may be absent → epoch 0
                        let ts = meta_v
                            .and_then(|m| m.get("agentTimestampMs"))
                            .and_then(|t| t.as_i64().or_else(|| t.as_f64().map(|f| f as i64)))
                            .filter(|&t| t > 0)
                            .or_else(|| {
                                parse_ts(v.get("timestamp").unwrap_or(&serde_json::Value::Null))
                            })
                            .unwrap_or(0);
                        let event_id = meta_v.and_then(|m| strv(m, "eventId"));
                        let sess = v
                            .pointer("/params/sessionId")
                            .and_then(|s| s.as_str())
                            .unwrap_or(&meta.session_id)
                            .to_string();
                        // `modelUsage` rows (name-sorted like upstream's map),
                        // else one row on the session's default model
                        let rows: Vec<(String, &serde_json::Value)> =
                            match u.get("modelUsage").and_then(|m| m.as_object()) {
                                Some(mus) if !mus.is_empty() => {
                                    let mut r: Vec<_> = mus
                                        .iter()
                                        .map(|(m, mu)| (m.clone(), mu))
                                        .collect();
                                    r.sort_by(|a, b| a.0.cmp(&b.0));
                                    r
                                }
                                _ => vec![(meta.default_model.clone(), u)],
                            };
                        for (model, mu) in rows {
                            let tokens = grok_usage(mu);
                            if tokens.is_zero() {
                                continue;
                            }
                            let key = match event_id {
                                Some(e) => format!("{e}|{model}"),
                                None => format!(
                                    "{sess}|{ts}|{model}|{}|{}|{}|{}",
                                    tokens.input,
                                    tokens.output,
                                    tokens.cache_read,
                                    tokens.cache_creation
                                ),
                            };
                            if !seen.insert(key.clone()) {
                                continue;
                            }
                            out.push(UsageEntry {
                                agent,
                                timestamp_ms: ts,
                                session_id: sess.clone().into(),
                                model: model.into(),
                                provider: "".into(),
                                tokens,
                                cost_usd: grok_ticks(mu),
                                dedup_key: key.into(),
                                dedup_id: 0,
                                total_override: None,
                                is_fallback: false,
                                project: meta.project.clone().into(),
                            });
                        }
                    });
                    out
                })
                .collect()
        })
    }
}

/// `summary.json` next to `updates.jsonl` supplies session id, project path
/// and default model; absent fields fall back to the directory layout.
struct GrokMeta {
    session_id: String,
    project: String,
    default_model: String,
}

fn grok_session_meta(updates: &Path) -> GrokMeta {
    let dir_name = updates
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".into());
    let mut project = updates
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .map(|n| grok_url_decode(&n.to_string_lossy()))
        .unwrap_or_else(|| "unknown".into());
    let mut session_id = dir_name;
    let mut default_model = UNKNOWN_MODEL.to_string();
    let summary = updates.with_file_name("summary.json");
    if let Ok(s) = std::fs::read_to_string(&summary) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
            if let Some(id) = v.pointer("/info/id").and_then(|i| i.as_str()) {
                session_id = id.to_string();
            }
            if let Some(cwd) = v
                .pointer("/info/cwd")
                .and_then(|c| c.as_str())
                .or_else(|| v.get("gitRootDir").and_then(|g| g.as_str()))
            {
                project = cwd.to_string();
            }
            if let Some(m) = v.get("currentModelId").and_then(|m| m.as_str()) {
                default_model = m.to_string();
            }
        }
    }
    GrokMeta {
        session_id,
        project,
        default_model,
    }
}

/// Upstream `url_decode_lightweight` — percent-escapes only; `+` stays literal.
fn grok_url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| -> Option<u8> {
                match c {
                    c @ b'0'..=b'9' => Some(c - b'0'),
                    c @ b'a'..=b'f' => Some(c - b'a' + 10),
                    c @ b'A'..=b'F' => Some(c - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Gemini CLI: `~/.gemini/tmp/<proj>/chats/*.json` — array of messages that
/// carry a `tokens` object.
pub struct Gemini;

fn gemini_files() -> Vec<PathBuf> {
    let mut files = vec![];
    walk(
        &gemini_dir(),
        &|p| {
            p.file_name().is_some_and(|n| n == "logs.json")
                || (p.extension().is_some_and(|e| e == "json")
                    && p.to_string_lossy().contains("/chats/"))
        },
        &mut files,
        8,
    );
    files.sort();
    files
}

fn gemini_dir() -> PathBuf {
    home().join(".gemini")
}

impl AgentReader for Gemini {
    fn id(&self) -> &'static str {
        "gemini"
    }
    fn display(&self) -> &'static str {
        "Gemini"
    }
    fn detect(&self) -> bool {
        gemini_dir().is_dir()
    }
    fn cache_key(&self) -> Option<String> {
        file_sig(&gemini_files())
    }

    fn entries(&self) -> Vec<UsageEntry> {
        let agent = self.id();
        let files = gemini_files();
        let sig = file_sig(&files);
        sig_scan("gemini", agent, sig, move || -> Vec<UsageEntry> {
            let mut out = vec![];
            for path in files {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                let msgs = v.get("messages").cloned().unwrap_or(v.clone());
                let session = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                if let Some(arr) = msgs.as_array() {
                    for m in arr {
                        let Some(t) = m.get("tokens") else { continue };
                        let tokens = Tokens {
                            input: num(t, "input"),
                            output: num(t, "output").max(num(t, "tool")),
                            reasoning: num(t, "thoughts"),
                            cache_creation: 0,
                            cache_read: num(t, "cached"),
                        };
                        if tokens.is_zero() {
                            continue;
                        }
                        let Some(ts) = parse_ts(
                            m.get("timestamp")
                                .or_else(|| m.get("startTime"))
                                .unwrap_or(&serde_json::Value::Null),
                        ) else {
                            continue;
                        };
                        out.push(UsageEntry {
                            agent,
                            timestamp_ms: ts,
                            session_id: session.clone().into(),
                            model: strv(m, "model").unwrap_or(UNKNOWN_MODEL).into(),
                            provider: "".into(),
                            tokens,
                            cost_usd: None,
                            dedup_key: strv(m, "id").unwrap_or("").into(),
                            dedup_id: 0,
                total_override: None,
                is_fallback: false,
                            project: "".into(),
                        });
                    }
                }
            }
            out
        })
    }
}

/// Generic fallback: scan an agent's dirs for *.jsonl lines that look like
/// usage records (object with a `usage`/`tokens` object plus a timestamp).
pub struct GenericJsonl {
    pub id: &'static str,
    pub display: &'static str,
    pub dirs: Vec<PathBuf>,
}

impl AgentReader for GenericJsonl {
    fn id(&self) -> &'static str {
        self.id
    }
    fn display(&self) -> &'static str {
        self.display
    }
    fn detect(&self) -> bool {
        self.dirs.iter().any(|d| d.is_dir())
    }
    fn cache_key(&self) -> Option<String> {
        let mut files = vec![];
        for d in &self.dirs {
            walk(d, &is_jsonl, &mut files, 8);
        }
        files.sort();
        file_sig(&files)
    }

    fn entries(&self) -> Vec<UsageEntry> {
        let agent = self.id();
        let mut files = vec![];
        for d in &self.dirs {
            walk(d, &is_jsonl, &mut files, 8);
        }
        files.sort();
        let sig = file_sig(&files);
        sig_scan(self.id, agent, sig, move || -> Vec<UsageEntry> {
            files
                .par_iter()
                .flat_map(|path| {
                    let mut out = vec![];
                    let session = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    read_jsonl(path, |v| {
                        let u = v
                            .get("usage")
                            .or_else(|| v.get("tokens"))
                            .or_else(|| v.pointer("/message/usage"));
                        let Some(u) = u else { return };
                        if !u.is_object() {
                            return;
                        }
                        let get = |ks: &[&str]| -> u64 { ks.iter().map(|k| num(u, k)).sum() };
                        let tokens = Tokens {
                            input: get(&["input_tokens", "inputTokens", "input", "prompt_tokens"]),
                            output: get(&[
                                "output_tokens",
                                "outputTokens",
                                "output",
                                "completion_tokens",
                            ]),
                            reasoning: get(&["reasoning_tokens", "reasoningTokens", "reasoning"]),
                            cache_creation: get(&[
                                "cache_creation_input_tokens",
                                "cacheCreationTokens",
                                "cache_write",
                            ]),
                            cache_read: get(&[
                                "cache_read_input_tokens",
                                "cachedReadTokens",
                                "cached_input_tokens",
                                "cache_read",
                                "cached",
                            ]),
                        };
                        if tokens.is_zero() {
                            return;
                        }
                        let Some(ts) = parse_ts(
                            v.get("timestamp")
                                .or_else(|| v.get("created_at"))
                                .or_else(|| v.get("time"))
                                .unwrap_or(&serde_json::Value::Null),
                        ) else {
                            return;
                        };
                        out.push(UsageEntry {
                            agent,
                            timestamp_ms: ts,
                            session_id: strv(v, "session_id")
                                .or_else(|| strv(v, "sessionId"))
                                .unwrap_or(&session)
                                .into(),
                            model: strv(v, "model")
                                .or_else(|| v.pointer("/message/model").and_then(|m| m.as_str()))
                                .unwrap_or(UNKNOWN_MODEL)
                                .into(),
                            provider: "".into(),
                            tokens,
                            cost_usd: fnum(v, "costUSD")
                                .or_else(|| fnum(u, "cost"))
                                .or_else(|| fnum(u, "costUsd")),
                            dedup_key: strv(v, "id")
                                .or_else(|| strv(v, "requestId"))
                                .unwrap_or("")
                                .into(),
                            dedup_id: 0,
                total_override: None,
                is_fallback: false,
                            project: strv(v, "cwd").unwrap_or("").into(),
                        });
                    });
                    out
                })
                .collect()
        })
    }
}
