//! JSONL-transcript readers: Claude Code, Codex, Grok, Gemini, generic fallback.
use super::sqlite::{file_sig, sig_scan};
use super::{fnum, is_jsonl, num, parse_ts, strv, walk, AgentReader};
use crate::paths::home;
use crate::types::{Tokens, UsageEntry, UNKNOWN_MODEL};
use rayon::prelude::*;
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
        tokens,
        cost_usd: fnum(v, "costUSD").or_else(|| fnum(v, "cost_usd")),
        dedup_key: dedup_key.into(),
        dedup_id: 0,
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
        sig_scan("codex", agent, sig, move || -> Vec<UsageEntry> {
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
                                if let (Some(t), Some(m)) = (strv(&p, "turn_id"), strv(&p, "model"))
                                {
                                    turn_model.insert(t.to_string(), m.to_string());
                                    file_model = m.to_string();
                                }
                            }
                            "token_usage_record" => {
                                has_records = true;
                                let u = p.get("usage").cloned().unwrap_or_default();
                                let mut tokens = usage_from(&u);
                                if tokens.is_zero() {
                                    return;
                                }
                                let Some(ts) = parse_ts(
                                    v.get("timestamp").unwrap_or(&serde_json::Value::Null),
                                ) else {
                                    return;
                                };
                                // codex input_tokens include cache reads; upstream subtracts
                                tokens.input = tokens.input.saturating_sub(tokens.cache_read);
                                let turn = strv(&p, "turn_id").unwrap_or("").to_string();
                                let model = turn_model
                                    .get(&turn)
                                    .cloned()
                                    .or_else(|| {
                                        (!file_model.is_empty()).then(|| file_model.clone())
                                    })
                                    .unwrap_or_else(|| UNKNOWN_MODEL.into());
                                out.push(UsageEntry {
                                    agent,
                                    timestamp_ms: ts,
                                    session_id: strv(&p, "session_id")
                                        .unwrap_or(&meta_session)
                                        .into(),
                                    model: model.into(),
                                    tokens,
                                    cost_usd: None,
                                    dedup_key: strv(&p, "response_id").unwrap_or("").into(),
                                    dedup_id: 0,
                                    project: cwd.clone().into(),
                                });
                            }
                            // older rollouts: event_msg token_count with per-request `last_token_usage`
                            "event_msg" if strv(&p, "type") == Some("token_count") => {
                                let info = p.get("info").cloned().unwrap_or_default();
                                let u = info.get("last_token_usage").cloned().unwrap_or_default();
                                let mut tokens = usage_from(&u);
                                if tokens.is_zero() {
                                    return;
                                }
                                let Some(ts) = parse_ts(
                                    v.get("timestamp").unwrap_or(&serde_json::Value::Null),
                                ) else {
                                    return;
                                };
                                tokens.input = tokens.input.saturating_sub(tokens.cache_read);
                                let ord = num(v, "ordinal");
                                legacy.push(UsageEntry {
                                    agent,
                                    timestamp_ms: ts,
                                    session_id: meta_session.clone().into(),
                                    model: file_model.clone().into(),
                                    tokens,
                                    cost_usd: None,
                                    dedup_key: format!("{}:{ord}", meta_session).into(),
                                    dedup_id: 0,
                                    project: cwd.clone().into(),
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
        })
    }
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
    let cr = num(u, "cachedReadTokens");
    Tokens {
        // grok inputTokens include cached reads; upstream subtracts them
        input: num(u, "inputTokens").saturating_sub(cr),
        output: num(u, "outputTokens"),
        reasoning: num(u, "reasoningTokens"),
        cache_creation: num(u, "cacheCreationTokens"),
        cache_read: cr,
    }
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
                    // prompt_id -> entry; cumulative snapshots, last wins
                    let mut last: std::collections::HashMap<String, UsageEntry> =
                        Default::default();
                    read_jsonl(path, |v| {
                        let Some(upd) = v.pointer("/params/update") else {
                            return;
                        };
                        let Some(u) = upd.get("usage") else { return };
                        if !u.is_object() {
                            return;
                        }
                        // a snapshot without a timestamp still wins "last" for
                        // its prompt — keep it under epoch 0 rather than dropping
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
                                let tokens = grok_usage(mu);
                                if tokens.is_zero() {
                                    continue;
                                }
                                any = true;
                                let key = format!("{sess}:{prompt}:{model}");
                                last.insert(
                                    key.clone(),
                                    UsageEntry {
                                        agent,
                                        timestamp_ms: ts,
                                        session_id: sess.clone().into(),
                                        model: model.clone().into(),
                                        tokens,
                                        cost_usd: cost,
                                        dedup_key: key.into(),
                                        dedup_id: 0,
                                        project: project.clone().into(),
                                    },
                                );
                            }
                        }
                        if !any {
                            let tokens = grok_usage(u);
                            if tokens.is_zero() {
                                return;
                            }
                            let key = format!("{sess}:{prompt}");
                            last.insert(
                                key.clone(),
                                UsageEntry {
                                    agent,
                                    timestamp_ms: ts,
                                    session_id: sess.clone().into(),
                                    model: strv(upd, "model")
                                        .or_else(|| strv(u, "model"))
                                        .unwrap_or(UNKNOWN_MODEL)
                                        .into(),
                                    tokens,
                                    cost_usd: cost,
                                    dedup_key: key.into(),
                                    dedup_id: 0,
                                    project: project.clone().into(),
                                },
                            );
                        }
                    });
                    last.into_values().collect::<Vec<_>>()
                })
                .collect()
        })
    }
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
                            tokens,
                            cost_usd: None,
                            dedup_key: strv(m, "id").unwrap_or("").into(),
                            dedup_id: 0,
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
                            tokens,
                            cost_usd: fnum(v, "costUSD")
                                .or_else(|| fnum(u, "cost"))
                                .or_else(|| fnum(u, "costUsd")),
                            dedup_key: strv(v, "id")
                                .or_else(|| strv(v, "requestId"))
                                .unwrap_or("")
                                .into(),
                            dedup_id: 0,
                            project: strv(v, "cwd").unwrap_or("").into(),
                        });
                    });
                    out
                })
                .collect()
        })
    }
}
