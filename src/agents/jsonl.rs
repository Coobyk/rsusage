//! JSONL-transcript readers: Claude Code, Codex, Grok, Gemini, generic fallback.
use super::*;
use rayon::prelude::*;
use std::io::{BufRead, BufReader};

fn read_jsonl(path: &Path, mut f: impl FnMut(&serde_json::Value)) {
    let Ok(file) = std::fs::File::open(path) else { return };
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


fn claude_entry(v: &serde_json::Value, project: &str, fallback_session: &str) -> Option<UsageEntry> {
    let msg = v.get("message")?;
    let usage = msg.get("usage")?;
    if !usage.is_object() {
        return None;
    }
    let input = num(usage, "input_tokens");
    let output = num(usage, "output_tokens");
    let cc = num(usage, "cache_creation_input_tokens");
    let cr = num(usage, "cache_read_input_tokens");
    if input + output + cc + cr == 0 {
        return None;
    }
    let ts = parse_ts(v.get("timestamp").unwrap_or(&serde_json::Value::Null))?;
    let model = strv(msg, "model").unwrap_or("<unknown>").to_string();
    // upstream derives session id from the transcript file stem
    let _ = strv(v, "sessionId");
    let session = fallback_session.to_string();
    let dedup_key = match (strv(msg, "id"), strv(v, "requestId").or_else(|| strv(v, "request_id"))) {
        (Some(m), Some(r)) => format!("{m}:{r}"),
        _ => String::new(),
    };
    // upstream reports the raw encoded project dir name, not cwd
    let project = project.to_string();
    Some(UsageEntry {
        agent: "claude",
        timestamp_ms: ts,
        session_id: session,
        model,
        input,
        output,
        reasoning: 0,
        cache_creation: cc,
        cache_read: cr,
        cost_usd: fnum(v, "costUSD").or_else(|| fnum(v, "cost_usd")),
        dedup_key,
        project,
    })
}

impl AgentReader for Claude {
    fn id(&self) -> &'static str { "claude" }
    fn display(&self) -> &'static str { "Claude" }
    fn detect(&self) -> bool {
        claude_dirs().iter().any(|d| d.is_dir())
    }
    fn entries(&self) -> Vec<UsageEntry> {
        let mut files = vec![];
        for d in claude_dirs() {
            walk(&d, &is_jsonl, &mut files, 8);
        }
        files.sort();
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
                    if let Some(e) = claude_entry(v, &project, &fallback) {
                        out.push(e);
                    }
                });
                out
            })
            .collect()
    }
}

/// Codex: `~/.codex/sessions/**/rollout-*.jsonl`. `token_usage_record` events
/// carry per-request usage; `turn_context` supplies the model per turn.
pub struct Codex;

fn codex_dir() -> PathBuf {
    std::env::var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".codex"))
        .join("sessions")
}

fn usage_from(v: &serde_json::Value) -> (u64, u64, u64, u64, u64) {
    (
        num(v, "input_tokens"),
        num(v, "output_tokens"),
        num(v, "reasoning_output_tokens"),
        num(v, "cache_write_input_tokens"),
        num(v, "cached_input_tokens"),
    )
}

impl AgentReader for Codex {
    fn id(&self) -> &'static str { "codex" }
    fn display(&self) -> &'static str { "Codex" }
    fn detect(&self) -> bool { codex_dir().is_dir() }
    fn entries(&self) -> Vec<UsageEntry> {
        let mut files = vec![];
        walk(&codex_dir(), &is_jsonl, &mut files, 8);
        files.sort();
        files
            .par_iter()
            .flat_map(|path| {
                let mut out = vec![];
                let mut turn_model: std::collections::HashMap<String, String> =
                    Default::default();
                let mut cwd = String::new();
                let mut meta_session = String::new();
                let mut file_model = String::new();
                let mut has_records = false;
                let mut legacy: Vec<UsageEntry> = vec![];
                read_jsonl(path, |v| {
                    let ty = strv(v, "type").unwrap_or("");
                    let p = v.get("payload").cloned().unwrap_or_default();
                    match ty {
                        "session_meta" => {
                            cwd = strv(&p, "cwd").unwrap_or("").to_string();
                            meta_session = strv(&p, "session_id")
                                .or_else(|| strv(&p, "id"))
                                .unwrap_or("")
                                .to_string();
                        }
                        "turn_context" => {
                            if let (Some(t), Some(m)) = (strv(&p, "turn_id"), strv(&p, "model")) {
                                turn_model.insert(t.to_string(), m.to_string());
                                file_model = m.to_string();
                            }
                        }
                        "token_usage_record" => {
                            has_records = true;
                            let u = p.get("usage").cloned().unwrap_or_default();
                            let (input, output, reasoning, cc, cr) = usage_from(&u);
                            if input + output + reasoning + cc + cr == 0 {
                                return;
                            }
                            let Some(ts) = parse_ts(
                                v.get("timestamp").unwrap_or(&serde_json::Value::Null),
                            ) else { return };
                            let input = input.saturating_sub(cr);
                            let turn = strv(&p, "turn_id").unwrap_or("").to_string();
                            let model = turn_model
                                .get(&turn)
                                .cloned()
                                .or_else(|| (!file_model.is_empty()).then(|| file_model.clone()))
                                .unwrap_or_else(|| "<unknown>".into());
                            out.push(UsageEntry {
                                agent: "codex",
                                timestamp_ms: ts,
                                session_id: strv(&p, "session_id")
                                    .unwrap_or(&meta_session)
                                    .to_string(),
                                model,
                                input,
                                output,
                                reasoning,
                                cache_creation: cc,
                                cache_read: cr,
                                cost_usd: None,
                                dedup_key: strv(&p, "response_id")
                                    .unwrap_or("")
                                    .to_string(),
                                project: cwd.clone(),
                            });
                        }
                        // older rollouts: event_msg token_count with per-request `last_token_usage`
                        "event_msg" if strv(&p, "type") == Some("token_count") => {
                            let info = p.get("info").cloned().unwrap_or_default();
                            let u = info.get("last_token_usage").cloned().unwrap_or_default();
                            let (input, output, reasoning, cc, cr) = usage_from(&u);
                            if input + output + reasoning + cc + cr == 0 {
                                return;
                            }
                            let Some(ts) = parse_ts(
                                v.get("timestamp").unwrap_or(&serde_json::Value::Null),
                            ) else { return };
                            let input = input.saturating_sub(cr);
                            let ord = num(v, "ordinal");
                            legacy.push(UsageEntry {
                                agent: "codex",
                                timestamp_ms: ts,
                                session_id: meta_session.clone(),
                                model: file_model.clone(),
                                input,
                                output,
                                reasoning,
                                cache_creation: cc,
                                cache_read: cr,
                                cost_usd: None,
                                dedup_key: format!("{}:{ord}", meta_session),
                                project: cwd.clone(),
                            });
                        }
                        _ => {}
                    }
                });
                if !has_records {
                    out.append(&mut legacy);
                }
                out
            })
            .collect()
    }
}

/// Grok: `~/.grok/sessions/<cwd>/<sid>/updates.jsonl`. Lines are
/// `{method, params:{sessionId, update:{prompt_id, usage, modelUsage}}, timestamp}`
/// where `update.usage` is a cumulative snapshot per prompt — keep the last.
pub struct Grok;

fn grok_dir() -> PathBuf { home().join(".grok/sessions") }

fn grok_usage(u: &serde_json::Value) -> (u64, u64, u64, u64, u64) {
    // grok inputTokens include cached reads; upstream subtracts them
    let cr = num(u, "cachedReadTokens");
    (
        num(u, "inputTokens").saturating_sub(cr),
        num(u, "outputTokens"),
        num(u, "reasoningTokens"),
        num(u, "cacheCreationTokens"),
        cr,
    )
}

impl AgentReader for Grok {
    fn id(&self) -> &'static str { "grok" }
    fn display(&self) -> &'static str { "Grok" }
    fn detect(&self) -> bool { grok_dir().is_dir() }
    fn entries(&self) -> Vec<UsageEntry> {
        let mut files = vec![];
        walk(&grok_dir(), &|p| p.file_name().is_some_and(|n| n == "updates.jsonl"), &mut files, 6);
        files
            .par_iter()
            .flat_map(|path| {
                let session = path
                    .parent()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let project = path
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().replace("%2F", "/"))
                    .unwrap_or_default();
                // prompt_id -> (ts, entry); cumulative snapshots, last wins
                let mut last: std::collections::HashMap<String, UsageEntry> =
                    Default::default();
                let mut seq = 0u64;
                read_jsonl(path, |v| {
                    let Some(upd) = v.pointer("/params/update") else { return };
                    let Some(u) = upd.get("usage") else { return };
                    if !u.is_object() {
                        return;
                    }
                    let ts = parse_ts(v.get("timestamp").unwrap_or(&serde_json::Value::Null))
                        .unwrap_or(0);
                    let prompt = strv(upd, "prompt_id").unwrap_or("").to_string();
                    let cost = fnum(u, "costUsdTicks").map(|t| t / 1e8);
                    let sess = v
                        .pointer("/params/sessionId")
                        .and_then(|s| s.as_str())
                        .unwrap_or(&session)
                        .to_string();
                    // per-model breakdown when available
                    let mut any = false;
                    if let Some(mus) = u.get("modelUsage").and_then(|m| m.as_object()) {
                        for (model, mu) in mus {
                            let (i, o, r, cc, cr) = grok_usage(mu);
                            if i + o + r + cc + cr == 0 {
                                continue;
                            }
                            any = true;
                            seq += 1;
                            last.insert(
                                format!("{prompt}:{model}"),
                                UsageEntry {
                                    agent: "grok",
                                    timestamp_ms: ts,
                                    session_id: sess.clone(),
                                    model: model.clone(),
                                    input: i,
                                    output: o,
                                    reasoning: r,
                                    cache_creation: cc,
                                    cache_read: cr,
                                    cost_usd: cost,
                                    dedup_key: format!("{prompt}:{model}:{seq}"),
                                    project: project.clone(),
                                },
                            );
                        }
                    }
                    if !any {
                        let (i, o, r, cc, cr) = grok_usage(u);
                        if i + o + r + cc + cr == 0 {
                            return;
                        }
                        last.insert(
                            prompt.clone(),
                            UsageEntry {
                                agent: "grok",
                                timestamp_ms: ts,
                                session_id: sess.clone(),
                                model: strv(upd, "model")
                                    .or_else(|| strv(u, "model"))
                                    .unwrap_or("<unknown>")
                                    .to_string(),
                                input: i,
                                output: o,
                                reasoning: r,
                                cache_creation: cc,
                                cache_read: cr,
                                cost_usd: cost,
                                dedup_key: prompt,
                                project: project.clone(),
                            },
                        );
                    }
                });
                last.into_values().collect::<Vec<_>>()
            })
            .collect()
    }
}

/// Gemini CLI: `~/.gemini/tmp/<proj>/chats/*.json` — array of messages that
/// carry a `tokens` object.
pub struct Gemini;

fn gemini_dir() -> PathBuf { home().join(".gemini") }

impl AgentReader for Gemini {
    fn id(&self) -> &'static str { "gemini" }
    fn display(&self) -> &'static str { "Gemini" }
    fn detect(&self) -> bool { gemini_dir().is_dir() }
    fn entries(&self) -> Vec<UsageEntry> {
        let mut files = vec![];
        walk(&gemini_dir(), &|p| {
            p.file_name().is_some_and(|n| n == "logs.json")
                || (p.extension().is_some_and(|e| e == "json")
                    && p.to_string_lossy().contains("/chats/"))
        }, &mut files, 8);
        let mut out = vec![];
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
            let msgs = v.get("messages").cloned().unwrap_or(v.clone());
            let session = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if let Some(arr) = msgs.as_array() {
                for m in arr {
                    let Some(t) = m.get("tokens") else { continue };
                    let input = num(t, "input");
                    let output = num(t, "output").max(num(t, "tool"));
                    let reasoning = num(t, "thoughts");
                    let cr = num(t, "cached");
                    if input + output + reasoning + cr == 0 {
                        continue;
                    }
                    let Some(ts) = parse_ts(
                        m.get("timestamp")
                            .or_else(|| m.get("startTime"))
                            .unwrap_or(&serde_json::Value::Null),
                    ) else { continue };
                    out.push(UsageEntry {
                        agent: "gemini",
                        timestamp_ms: ts,
                        session_id: session.clone(),
                        model: strv(m, "model").unwrap_or("<unknown>").to_string(),
                        input,
                        output,
                        reasoning,
                        cache_creation: 0,
                        cache_read: cr,
                        cost_usd: None,
                        dedup_key: strv(m, "id").unwrap_or("").to_string(),
                        project: String::new(),
                    });
                }
            }
        }
        out
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
    fn id(&self) -> &'static str { self.id }
    fn display(&self) -> &'static str { self.display }
    fn detect(&self) -> bool { self.dirs.iter().any(|d| d.is_dir()) }
    fn entries(&self) -> Vec<UsageEntry> {
        let mut files = vec![];
        for d in &self.dirs {
            walk(d, &is_jsonl, &mut files, 8);
        }
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
                    let get = |ks: &[&str]| -> u64 {
                        ks.iter().map(|k| num(u, k)).sum()
                    };
                    let input = get(&["input_tokens", "inputTokens", "input", "prompt_tokens"]);
                    let output = get(&["output_tokens", "outputTokens", "output", "completion_tokens"]);
                    let reasoning = get(&["reasoning_tokens", "reasoningTokens", "reasoning"]);
                    let cc = get(&["cache_creation_input_tokens", "cacheCreationTokens", "cache_write"]);
                    let cr = get(&["cache_read_input_tokens", "cachedReadTokens", "cached_input_tokens", "cache_read", "cached"]);
                    if input + output + reasoning + cc + cr == 0 {
                        return;
                    }
                    let Some(ts) = parse_ts(
                        v.get("timestamp")
                            .or_else(|| v.get("created_at"))
                            .or_else(|| v.get("time"))
                            .unwrap_or(&serde_json::Value::Null),
                    ) else { return };
                    out.push(UsageEntry {
                        agent: self.id,
                        timestamp_ms: ts,
                        session_id: strv(v, "session_id")
                            .or_else(|| strv(v, "sessionId"))
                            .unwrap_or(&session)
                            .to_string(),
                        model: strv(v, "model")
                            .or_else(|| v.pointer("/message/model").and_then(|m| m.as_str()))
                            .unwrap_or("<unknown>")
                            .to_string(),
                        input,
                        output,
                        reasoning,
                        cache_creation: cc,
                        cache_read: cr,
                        cost_usd: fnum(v, "costUSD").or_else(|| fnum(u, "cost")).or_else(|| fnum(u, "costUsd")),
                        dedup_key: strv(v, "id")
                            .or_else(|| strv(v, "requestId"))
                            .unwrap_or("")
                            .to_string(),
                        project: strv(v, "cwd").unwrap_or("").to_string(),
                    });
                });
                out
            })
            .collect()
    }
}
