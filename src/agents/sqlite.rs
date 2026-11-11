//! SQLite-store readers: OpenCode, Kilo (opencode-fork schema), ZCode, Devin,
//! Zed, Cursor.
use super::*;
use rayon::prelude::*;

/// Split a rowid range into ~2×thread-count chunks for parallel queries.
fn id_chunks(min: i64, max: i64) -> Vec<(i64, i64)> {
    let n = (std::thread::available_parallelism()
        .map(|n| n.get() as i64 * 2)
        .unwrap_or(8))
    .max(4);
    let size = ((max - min + 1) + n - 1) / n;
    (0..n)
        .map(|i| (min + i * size, (min + (i + 1) * size - 1).min(max)))
        .filter(|(lo, hi)| lo <= hi)
        .collect()
}

/// OpenCode-family stores (`opencode.db`, `kilo.db`): assistant messages carry
/// `data.tokens` + `data.modelID` + `data.cost`.
pub struct OpenCodeDb {
    pub id: &'static str,
    pub display: &'static str,
    pub path: PathBuf,
}

impl AgentReader for OpenCodeDb {
    fn id(&self) -> &'static str { self.id }
    fn display(&self) -> &'static str { self.display }
    fn detect(&self) -> bool { self.path.is_file() }
    fn entries(&self) -> Vec<UsageEntry> {
        // upstream reads both v1 (`message`) and v2 (`session_message`) rows;
        // shared message ids are deduped downstream
        let mut out = self.entries_table(false);
        out.extend(self.entries_table(true));
        out
    }
}

impl OpenCodeDb {
    fn entries_table(&self, v2: bool) -> Vec<UsageEntry> {
        let Some(conn) = open_db(&self.path) else { return vec![] };
        let table = if v2 { "session_message" } else { "message" };
        let (min, max): (i64, i64) = conn
            .query_row(
                &format!("SELECT min(rowid), max(rowid) FROM {table}"),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((1, 0));
        if max < min {
            return vec![];
        }
        let src = format!("{}:{table}", self.path.to_string_lossy());
        let cache_name = format!("{}{}", self.id, if v2 { "_v2" } else { "" });
        if let Some(mut cache) = EntryCache::open(&cache_name) {
            let wm = cache.watermark(&src).max(min - 1);
            if max <= wm {
                return cache.all(self.id);
            }
            let lo = (wm - CACHE_TAIL).max(min);
            let fresh = id_chunks(lo, max)
                .par_iter()
                .flat_map(|&(a, b)| self.entries_range(a, b, v2))
                .collect::<Vec<_>>();
            cache.refresh(lo, max, &fresh);
            cache.set_watermark(max, &src);
            return cache.all(self.id);
        }
        id_chunks(min, max)
            .par_iter()
            .flat_map(|&(lo, hi)| self.entries_range(lo, hi, v2))
            .map(|(_, e)| e)
            .collect()
    }
}

impl OpenCodeDb {
    fn entries_range(&self, lo: i64, hi: i64, v2: bool) -> Vec<(i64, UsageEntry)> {
        let Some(conn) = open_db(&self.path) else { return vec![] };
        let sql = if v2 {
            "SELECT m.rowid, m.id, m.session_id, m.time_created,                     json_extract(m.data,                       '$.tokens.input', '$.tokens.output', '$.tokens.reasoning',                       '$.tokens.cache.read', '$.tokens.cache.write',                       '$.model.id', '$.time.created', '$.cost'),                     s.directory              FROM session_message m              LEFT JOIN session_v2 s ON s.id = m.session_id              WHERE m.rowid >= ?1 AND m.rowid <= ?2 AND json_extract(m.data, '$.tokens.input') IS NOT NULL"
        } else {
            "SELECT rowid, id, session_id, time_created, json_extract(data,                '$.tokens.input', '$.tokens.output', '$.tokens.reasoning',                '$.tokens.cache.read', '$.tokens.cache.write',                '$.modelID', '$.time.created', '$.cost'),                NULL              FROM message              WHERE rowid >= ?1 AND rowid <= ?2                AND json_extract(data, '$.role') = 'assistant'"
        };
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let rows = stmt
            .query_map([lo, hi], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })
            .map(|r| r.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut out = Vec::with_capacity(rows.len());
        for (rowid, id, session, created, arr, directory) in rows {
            let Ok(d) = serde_json::from_str::<serde_json::Value>(&arr) else { continue };
            let input = d[0].as_u64().unwrap_or(0);
            let output = d[1].as_u64().unwrap_or(0);
            let reasoning = d[2].as_u64().unwrap_or(0);
            let cr = d[3].as_u64().unwrap_or(0);
            let cc = d[4].as_u64().unwrap_or(0);
            if input + output + reasoning + cr + cc == 0 {
                continue;
            }
            let ts = d[6].as_i64().unwrap_or(created);
            out.push((
                rowid,
                UsageEntry {
                    agent: self.id,
                    timestamp_ms: epoch_to_ms(ts),
                    session_id: session,
                    model: d[5].as_str().unwrap_or("<unknown>").to_string(),
                    input,
                    output,
                    reasoning,
                    cache_creation: cc,
                    cache_read: cr,
                    cost_usd: d[7].as_f64(),
                    dedup_key: id,
                    project: directory.unwrap_or_default(),
                },
            ));
        }
        out
    }
}

/// Devin CLI: `~/.local/share/devin/cli/sessions.db`. Per-message usage lives in
/// `message_nodes.chat_message.metadata.metrics`; model on the `sessions` row.
pub struct Devin;

fn devin_db() -> PathBuf { xdg_data().join("devin/cli/sessions.db") }

impl AgentReader for Devin {
    fn id(&self) -> &'static str { "devin" }
    fn display(&self) -> &'static str { "Devin" }
    fn detect(&self) -> bool { devin_db().is_file() }
    fn entries(&self) -> Vec<UsageEntry> {
        let path = devin_db();
        let Some(conn) = open_db(&path) else { return vec![] };
        let (min, max): (i64, i64) = conn
            .query_row("SELECT min(row_id), max(row_id) FROM message_nodes", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap_or((1, 0));
        if max < min {
            return vec![];
        }
        let src = path.to_string_lossy().to_string();
        if let Some(mut cache) = EntryCache::open("devin") {
            let dbg = std::env::var_os("RSUSAGE_DEBUG").is_some();
            let t0 = std::time::Instant::now();
            let wm = cache.watermark(&src).max(min - 1);
            if dbg { eprintln!("wm={wm} {:?}", t0.elapsed()); }
            if max <= wm {
                return cache.all("devin");
            }
            let lo = (wm - CACHE_TAIL).max(min);
            let fresh = id_chunks(lo, max)
                .par_iter()
                .flat_map(|&(a, b)| Self::entries_range(a, b))
                .collect::<Vec<_>>();
            if dbg { eprintln!("scan {} {:?}", fresh.len(), t0.elapsed()); }
            cache.refresh(lo, max, &fresh);
            if dbg { eprintln!("refresh {:?}", t0.elapsed()); }
            cache.set_watermark(max, &src);
            let all = cache.all("devin");
            if dbg { eprintln!("all {} {:?}", all.len(), t0.elapsed()); }
            return all;
        }
        id_chunks(min, max)
            .par_iter()
            .flat_map(|&(lo, hi)| Self::entries_range(lo, hi))
            .map(|(_, e)| e)
            .collect()
    }
}

impl Devin {
    fn entries_range(lo: i64, hi: i64) -> Vec<(i64, UsageEntry)> {
        let Some(conn) = open_db(&devin_db()) else { return vec![] };
        let mut stmt = match conn.prepare(
            "SELECT n.row_id, n.session_id, n.created_at,                     json_extract(n.chat_message,                       '$.metadata.metrics', '$.metadata.request_id',                       '$.metadata.created_at', '$.metadata.cost'),                     s.model, s.working_directory              FROM message_nodes n JOIN sessions s ON s.id = n.session_id              WHERE n.row_id >= ?1 AND n.row_id <= ?2                AND json_extract(n.chat_message, '$.metadata.metrics.input_tokens') IS NOT NULL",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let rows = stmt
            .query_map([lo, hi], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })
            .map(|r| r.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut out = Vec::with_capacity(rows.len());
        for (row_id, session, created, arr, model, cwd) in rows {
            let Ok(d) = serde_json::from_str::<serde_json::Value>(&arr) else { continue };
            let met = &d[0];
            let input = num(met, "input_tokens");
            let output = num(met, "output_tokens");
            let cr = num(met, "cache_read_tokens");
            let cc = num(met, "cache_creation_tokens");
            if input + output + cr + cc == 0 {
                continue;
            }
            let ts = parse_ts(&d[2])
                .filter(|t| *t > 0)
                .unwrap_or_else(|| epoch_to_ms(created));
            let cost = match &d[3] {
                serde_json::Value::Number(n) => n.as_f64(),
                serde_json::Value::Object(o) => {
                    o.get("costUSD").or_else(|| o.get("cost")).and_then(|c| c.as_f64())
                }
                _ => None,
            };
            out.push((
                row_id,
                UsageEntry {
                agent: "devin",
                timestamp_ms: ts,
                session_id: session,
                model: model.unwrap_or_else(|| "<unknown>".into()),
                input,
                output,
                reasoning: 0,
                cache_creation: cc,
                cache_read: cr,
                cost_usd: cost,
                dedup_key: d[1]
                    .as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| row_id.to_string()),
                project: cwd.unwrap_or_default(),
                },
            ));
        }
        out
    }
}

/// ZCode (z.ai/GLM) CLI: `~/.zcode/cli/db/db.sqlite`. One `model_usage` row
/// per logical request attempt.
pub struct ZCode;

fn zcode_db() -> PathBuf { home().join(".zcode/cli/db/db.sqlite") }

impl AgentReader for ZCode {
    fn id(&self) -> &'static str { "zcode" }
    fn display(&self) -> &'static str { "ZCode" }
    fn detect(&self) -> bool { zcode_db().is_file() }
    fn entries(&self) -> Vec<UsageEntry> {
        let Some(conn) = open_db(&zcode_db()) else { return vec![] };
        let mut stmt = match conn.prepare(
            "SELECT id, session_id, started_at, model_id, provider_id,                     input_tokens, output_tokens, reasoning_tokens,                     cache_creation_input_tokens, cache_read_input_tokens              FROM model_usage              WHERE input_tokens > 0 OR output_tokens > 0                 OR cache_read_input_tokens > 0 OR cache_creation_input_tokens > 0",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, i64>(9)?,
                ))
            })
            .map(|r| r.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        rows.into_iter()
            .map(|(id, session, started, model, provider, i, o, r_, cc, cr)| UsageEntry {
                agent: "zcode",
                timestamp_ms: epoch_to_ms(started),
                session_id: session,
                model: if model.is_empty() { provider } else { model },
                // zcode input_tokens include cache reads; upstream subtracts
                input: (i - cr).max(0) as u64,
                output: o.max(0) as u64,
                reasoning: r_.max(0) as u64,
                cache_creation: cc.max(0) as u64,
                cache_read: cr.max(0) as u64,
                cost_usd: None,
                dedup_key: id,
                project: String::new(),
            })
            .collect()
    }
}

/// Zed agent threads: `~/.local/share/zed/threads/threads.db`. `data` is a
/// serialized blob — best effort: parse JSON if possible, else skip.
pub struct Zed;

fn zed_db() -> PathBuf { xdg_data().join("zed/threads/threads.db") }

impl AgentReader for Zed {
    fn id(&self) -> &'static str { "zed" }
    fn display(&self) -> &'static str { "Zed" }
    fn detect(&self) -> bool { zed_db().is_file() }
    fn entries(&self) -> Vec<UsageEntry> {
        let Some(conn) = open_db(&zed_db()) else { return vec![] };
        let mut stmt = match conn.prepare("SELECT id, data_type, data, updated_at FROM threads") {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map(|r| r.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut out = vec![];
        for (id, _ty, data, updated) in rows {
            let Ok(text) = String::from_utf8(data) else { continue };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
            // walk the thread JSON for anything usage-shaped
            collect_usage_recursive(&v, "zed", &id, &updated, &mut out);
        }
        out
    }
}

fn collect_usage_recursive(
    v: &serde_json::Value,
    agent: &'static str,
    session: &str,
    updated: &str,
    out: &mut Vec<UsageEntry>,
) {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(u) = map.get("usage").filter(|u| u.is_object()) {
                let input = num(u, "input_tokens").max(num(u, "inputTokens"));
                let output = num(u, "output_tokens").max(num(u, "outputTokens"));
                let cr = num(u, "cache_read_input_tokens").max(num(u, "cachedReadTokens"));
                let cc = num(u, "cache_creation_input_tokens").max(num(u, "cacheCreationTokens"));
                if input + output + cr + cc > 0 {
                    let ts = map
                        .get("timestamp")
                        .and_then(parse_ts)
                        .or_else(|| parse_ts(&serde_json::Value::String(updated.to_string())))
                        .unwrap_or(0);
                    out.push(UsageEntry {
                        agent,
                        timestamp_ms: ts,
                        session_id: session.to_string(),
                        model: map
                            .get("model")
                            .and_then(|m| m.as_str())
                            .unwrap_or("<unknown>")
                            .to_string(),
                        input,
                        output,
                        reasoning: num(u, "reasoning_tokens").max(num(u, "reasoningTokens")),
                        cache_creation: cc,
                        cache_read: cr,
                        cost_usd: None,
                        dedup_key: format!("zed-{}", out.len()),
                        project: String::new(),
                    });
                }
            }
            for val in map.values() {
                collect_usage_recursive(val, agent, session, updated, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for val in arr {
                collect_usage_recursive(val, agent, session, updated, out);
            }
        }
        _ => {}
    }
}

/// Cursor agent CLI: `~/.cursor/ai-tracking/ai-code-tracking.db`. Tracks AI
/// code rather than tokens, but scan for any usage-bearing tables.
pub struct Cursor;

fn cursor_db() -> PathBuf { home().join(".cursor/ai-tracking/ai-code-tracking.db") }

impl AgentReader for Cursor {
    fn id(&self) -> &'static str { "cursor" }
    fn display(&self) -> &'static str { "Cursor" }
    fn detect(&self) -> bool { cursor_db().is_file() }
    fn entries(&self) -> Vec<UsageEntry> {
        let Some(conn) = open_db(&cursor_db()) else { return vec![] };
        // find tables that look like they contain usage rows
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .and_then(|mut s| {
                s.query_map([], |r| r.get::<_, String>(0))
                    .map(|rows| rows.flatten().collect())
            })
            .unwrap_or_default();
        let mut out = vec![];
        for t in tables {
            let lname = t.to_lowercase();
            if !(lname.contains("usage") || lname.contains("token") || lname.contains("composer")) {
                continue;
            }
            let sql = format!("SELECT * FROM \"{t}\"");
            let Ok(mut stmt) = conn.prepare(&sql) else { continue };
            let ncols = stmt.column_count();
            let colnames: Vec<String> =
                stmt.column_names().into_iter().map(|s| s.to_string()).collect();
            let has = |k: &str| colnames.iter().any(|c| c.to_lowercase() == k);
            let pick = |ks: &[&str]| -> Option<usize> {
                ks.iter()
                    .find_map(|k| colnames.iter().position(|c| c.to_lowercase() == *k))
            };
            let (ci, co) = (
                pick(&["input_tokens", "prompt_tokens"]),
                pick(&["output_tokens", "completion_tokens"]),
            );
            if ci.is_none() && co.is_none() && !has("usage") {
                continue;
            }
            let cts = pick(&["created_at", "timestamp", "time", "date"]);
            let cmodel = pick(&["model", "model_name"]);
            let csess = pick(&["session_id", "conversation_id", "composer_id"]);
            let cid = pick(&["id", "request_id", "message_id"]);
            let ccr = pick(&["cache_read_input_tokens", "cached_tokens"]);
            let ccc = pick(&["cache_creation_input_tokens"]);
            let mut rows = match stmt.query([]) {
                Ok(r) => r,
                Err(_) => continue,
            };
            while let Ok(Some(row)) = rows.next() {
                let gnum = |i: Option<usize>| -> u64 {
                    i.and_then(|i| row.get::<_, i64>(i).ok()).map(|n| n.max(0) as u64).unwrap_or(0)
                };
                let gstr = |i: Option<usize>| -> String {
                    i.and_then(|i| row.get::<_, String>(i).ok()).unwrap_or_default()
                };
                let input = gnum(ci);
                let output = gnum(co);
                let cr = gnum(ccr);
                let cc = gnum(ccc);
                if input + output + cr + cc == 0 {
                    continue;
                }
                let ts = cts
                    .and_then(|i| row.get::<_, rusqlite::types::Value>(i).ok())
                    .and_then(|v| match v {
                        rusqlite::types::Value::Integer(n) => Some(epoch_to_ms(n)),
                        rusqlite::types::Value::Text(t) => {
                            parse_ts(&serde_json::Value::String(t))
                        }
                        _ => None,
                    })
                    .unwrap_or(0);
                out.push(UsageEntry {
                    agent: "cursor",
                    timestamp_ms: ts,
                    session_id: gstr(csess),
                    model: gstr(cmodel),
                    input,
                    output,
                    reasoning: 0,
                    cache_creation: cc,
                    cache_read: cr,
                    cost_usd: None,
                    dedup_key: gstr(cid),
                    project: String::new(),
                });
            }
            let _ = ncols;
        }
        out
    }
}
