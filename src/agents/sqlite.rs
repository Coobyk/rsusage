//! SQLite-store readers: OpenCode, Kilo (opencode-fork schema), ZCode, Devin,
//! Zed, Cursor.
use super::{epoch_to_ms, num, parse_ts, AgentReader};
use crate::paths::{self, home, xdg_data};
use crate::types::{Tokens, UsageEntry, UNKNOWN_MODEL};
use rayon::prelude::*;
use std::path::{Path, PathBuf};

/// Open a SQLite database read-only, tolerating live WAL writers.
fn open_db(path: &Path) -> Option<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

// ---------------------------------------------------------------------------
// Incremental entry cache
// ---------------------------------------------------------------------------

/// Incremental entry cache for SQLite readers. Source tables use monotonic
/// rowids, so each run only re-scans rows beyond `watermark - CACHE_TAIL`.
/// Rows are stored as serialized `UsageEntry` blobs so the cache schema can't
/// drift from the entry type; `agent` is implied by the per-agent table.
///
/// All failures are treated as "cache unavailable" — the source scan is the
/// source of truth, so a broken cache must degrade to a full scan, not an
/// error.
pub struct EntryCache {
    conn: rusqlite::Connection,
    name: String,
}

const CACHE_TAIL: i64 = 2000;

/// Bump when `UsageEntry`'s serialized layout changes — old blobs are
/// undecodable, so every cached entry and watermark is dropped.
const CACHE_FORMAT: i64 = 1;

impl EntryCache {
    fn open(name: &str) -> Option<Self> {
        let dir = paths::xdg_cache();
        std::fs::create_dir_all(&dir).ok()?;
        let conn = rusqlite::Connection::open(dir.join("agent-cache.db")).ok()?;
        // readers run in parallel against this one file; wait rather than
        // fail with SQLITE_BUSY when a sibling holds the write lock
        conn.busy_timeout(std::time::Duration::from_secs(10)).ok()?;
        // meta schema check: older versions keyed watermarks by `agent`
        let meta_ok = conn
            .prepare("PRAGMA table_info(cache_meta)")
            .and_then(|mut s| {
                s.query_map([], |r| r.get::<_, String>(1))
                    .map(|rows| rows.flatten().any(|c| c == "name"))
            })
            .unwrap_or(false);
        if !meta_ok {
            let _ = conn.execute("DROP TABLE IF EXISTS cache_meta", []);
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS cache_meta (
                name TEXT PRIMARY KEY, wm INTEGER, source TEXT);",
        )
        .ok()?;
        // blob format version: mismatch means every stored entry is suspect
        let fmt_ok = conn
            .query_row("SELECT wm FROM cache_meta WHERE name='__format'", [], |r| {
                r.get::<_, i64>(0)
            })
            .ok()
            == Some(CACHE_FORMAT);
        if !fmt_ok {
            let tables: Vec<String> = conn
                .prepare(
                    "SELECT name FROM sqlite_master
                     WHERE type='table' AND name LIKE 'entries_%'",
                )
                .ok()
                .and_then(|mut s| {
                    s.query_map([], |r| r.get(0))
                        .ok()
                        .map(|it| it.flatten().collect())
                })
                .unwrap_or_default();
            for t in tables {
                let _ = conn.execute(&format!("DROP TABLE IF EXISTS {t}"), []);
            }
            let _ = conn.execute("DELETE FROM cache_meta", []);
            let _ = conn.execute(
                "INSERT INTO cache_meta (name, wm, source) VALUES ('__format', ?1, '')",
                [CACHE_FORMAT],
            );
        }
        // drop tables left by earlier row formats so stale schemas can't
        // make inserts/decodes silently fail — we need exactly (src_rowid, entry BLOB)
        let blob_schema = conn
            .prepare(&format!("PRAGMA table_info(entries_{name})"))
            .and_then(|mut s| {
                s.query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
                    .map(|rows| rows.flatten().any(|(c, t)| c == "entry" && t == "BLOB"))
            })
            .unwrap_or(false);
        if !blob_schema {
            let _ = conn.execute(&format!("DROP TABLE IF EXISTS entries_{name}"), []);
        }
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS entries_{name} (
                src_rowid INTEGER PRIMARY KEY, entry BLOB NOT NULL);"
        ))
        .ok()?;
        Some(Self {
            conn,
            name: name.to_string(),
        })
    }

    /// Rows beyond this rowid (minus TAIL) get re-scanned. A different source
    /// path than what was cached invalidates the whole table.
    fn watermark(&self, source: &str) -> i64 {
        let row: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT wm, source FROM cache_meta WHERE name=?1",
                [&self.name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        match row {
            Some((wm, src)) if src == source => wm,
            _ => {
                let _ = self
                    .conn
                    .execute(&format!("DELETE FROM entries_{}", self.name), []);
                0
            }
        }
    }

    fn set_watermark(&self, wm: i64, source: &str) {
        let _ = self.conn.execute(
            "INSERT INTO cache_meta (name, wm, source) VALUES (?1,?2,?3)
             ON CONFLICT(name) DO UPDATE SET wm=excluded.wm, source=excluded.source",
            rusqlite::params![self.name, wm, source],
        );
    }

    /// Delete cached rows in [lo, hi] then insert the fresh (rowid, entry)
    /// pairs, atomically. Returns false unless the whole batch committed —
    /// callers must not advance the watermark on failure, or the unscanned
    /// range would be lost for good.
    fn refresh(&self, lo: i64, hi: i64, rows: &[(i64, UsageEntry)]) -> bool {
        if self.conn.execute_batch("BEGIN IMMEDIATE").is_err() {
            return false;
        }
        let committed = self
            .conn
            .execute(
                &format!(
                    "DELETE FROM entries_{} WHERE src_rowid BETWEEN ?1 AND ?2",
                    self.name
                ),
                rusqlite::params![lo, hi],
            )
            .is_ok()
            && self
                .conn
                .prepare(&format!(
                    "INSERT INTO entries_{} (src_rowid, entry) VALUES (?1, ?2)",
                    self.name
                ))
                .map(|mut st| {
                    rows.iter()
                        .all(|(rid, e)| st.execute(rusqlite::params![rid, e.cache_blob()]).is_ok())
                })
                .unwrap_or(false);
        self.conn
            .execute_batch(if committed { "COMMIT" } else { "ROLLBACK" })
            .is_ok()
            && committed
    }

    /// All cached entries, in source-row order, tagged with `agent`.
    fn all(&self, agent: &'static str) -> Vec<UsageEntry> {
        let mut st = match self.conn.prepare(&format!(
            "SELECT entry FROM entries_{} ORDER BY src_rowid",
            self.name
        )) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        st.query_map([], |r| r.get::<_, Vec<u8>>(0))
            .map(|r| {
                r.flatten()
                    .filter_map(|s| UsageEntry::from_cache_blob(&s, agent))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Split a rowid range into ~2×thread-count chunks for parallel queries.
fn id_chunks(min: i64, max: i64) -> Vec<(i64, i64)> {
    let n = (std::thread::available_parallelism()
        .map(|n| n.get() as i64 * 2)
        .unwrap_or(8))
    .max(4);
    let size = (max - min + 1 + n - 1) / n;
    (0..n)
        .map(|i| (min + i * size, (min + (i + 1) * size - 1).min(max)))
        .filter(|(lo, hi)| lo <= hi)
        .collect()
}

/// Read a rowid-monotonic source table through the incremental cache:
/// rows beyond `watermark - CACHE_TAIL` are re-scanned (the tail catches
/// rows whose usage fields fill in late), the cache is refreshed in one
/// transaction, and the full entry set comes back.
///
/// `scan_range` runs per rowid chunk, possibly in parallel — it must open
/// its own connection.
fn cached_scan(
    path: &Path,
    rowid_col: &str,
    table: &str,
    cache_name: &str,
    agent: &'static str,
    scan_range: &(dyn Fn(i64, i64) -> Vec<(i64, UsageEntry)> + Sync),
) -> Vec<UsageEntry> {
    let Some(conn) = open_db(path) else {
        return vec![];
    };
    let (min, max): (i64, i64) = conn
        .query_row(
            &format!("SELECT min({rowid_col}), max({rowid_col}) FROM {table}"),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((1, 0));
    if max < min {
        return vec![];
    }
    let Some(cache) = EntryCache::open(cache_name) else {
        return id_chunks(min, max)
            .par_iter()
            .flat_map(|&(lo, hi)| scan_range(lo, hi))
            .map(|(_, e)| e)
            .collect();
    };
    let src = format!("{}:{table}", path.to_string_lossy());
    let wm = cache.watermark(&src).max(min - 1);
    if max <= wm {
        return cache.all(agent);
    }
    let lo = (wm - CACHE_TAIL).max(min);
    let fresh: Vec<(i64, UsageEntry)> = id_chunks(lo, max)
        .par_iter()
        .flat_map(|&(a, b)| scan_range(a, b))
        .collect();
    // advancing the watermark without a committed refresh would drop the
    // range permanently — only set it when the cache actually updated
    if cache.refresh(lo, max, &fresh) {
        cache.set_watermark(max, &src);
    }
    cache.all(agent)
}

// ---------------------------------------------------------------------------
// OpenCode family
// ---------------------------------------------------------------------------

/// Which generation of the opencode message schema to read. `V2` stores usage
/// on `session_message` rows joined to `session_v2` for the project dir;
/// `V1` stores it on `message` rows for assistant roles only.
#[derive(Clone, Copy)]
enum OcSchema {
    V1,
    V2,
}

impl OcSchema {
    fn table(&self) -> &'static str {
        match self {
            Self::V1 => "message",
            Self::V2 => "session_message",
        }
    }
}

/// OpenCode-family stores (`opencode.db`, `kilo.db`): messages carry token
/// usage under `data.tokens` + model + cost. Upstream reads both schema
/// generations, deduplicated by message id downstream.
pub struct OpenCodeDb {
    pub id: &'static str,
    pub display: &'static str,
    pub path: PathBuf,
}

impl AgentReader for OpenCodeDb {
    fn id(&self) -> &'static str {
        self.id
    }
    fn display(&self) -> &'static str {
        self.display
    }
    fn detect(&self) -> bool {
        self.path.is_file()
    }
    fn entries(&self) -> Vec<UsageEntry> {
        let mut out = self.entries_table(OcSchema::V1);
        out.extend(self.entries_table(OcSchema::V2));
        out
    }
}

impl OpenCodeDb {
    fn entries_table(&self, schema: OcSchema) -> Vec<UsageEntry> {
        let cache_name = match schema {
            OcSchema::V1 => self.id.to_string(),
            OcSchema::V2 => format!("{}_v2", self.id),
        };
        let path = self.path.clone();
        let id = self.id;
        cached_scan(
            &self.path,
            "rowid",
            schema.table(),
            &cache_name,
            id,
            &move |lo, hi| Self::scan_range(&path, id, lo, hi, schema),
        )
    }

    fn scan_range(
        path: &Path,
        agent: &'static str,
        lo: i64,
        hi: i64,
        schema: OcSchema,
    ) -> Vec<(i64, UsageEntry)> {
        let Some(conn) = open_db(path) else {
            return vec![];
        };
        let sql = match schema {
            // the trailing NULL column keeps v1/v2 on one row-decoding path
            OcSchema::V2 => {
                "SELECT m.rowid, m.id, m.session_id, m.time_created, \
                        json_extract(m.data, \
                            '$.tokens.input', '$.tokens.output', '$.tokens.reasoning', \
                            '$.tokens.cache.read', '$.tokens.cache.write', \
                            '$.model.id', '$.time.created', '$.cost'), \
                        s.directory \
                 FROM session_message m \
                 LEFT JOIN session_v2 s ON s.id = m.session_id \
                 WHERE m.rowid >= ?1 AND m.rowid <= ?2 \
                   AND json_extract(m.data, '$.tokens.input') IS NOT NULL"
            }
            OcSchema::V1 => {
                "SELECT rowid, id, session_id, time_created, json_extract(data, \
                    '$.tokens.input', '$.tokens.output', '$.tokens.reasoning', \
                    '$.tokens.cache.read', '$.tokens.cache.write', \
                    '$.modelID', '$.time.created', '$.cost'), \
                    NULL \
                 FROM message \
                 WHERE rowid >= ?1 AND rowid <= ?2 \
                   AND json_extract(data, '$.role') = 'assistant'"
            }
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
            let Ok(d) = serde_json::from_str::<serde_json::Value>(&arr) else {
                continue;
            };
            let tokens = Tokens {
                input: d[0].as_u64().unwrap_or(0),
                output: d[1].as_u64().unwrap_or(0),
                reasoning: d[2].as_u64().unwrap_or(0),
                cache_creation: d[4].as_u64().unwrap_or(0),
                cache_read: d[3].as_u64().unwrap_or(0),
            };
            if tokens.is_zero() {
                continue;
            }
            let ts = d[6].as_i64().unwrap_or(created);
            out.push((
                rowid,
                UsageEntry {
                    agent,
                    timestamp_ms: epoch_to_ms(ts),
                    session_id: session,
                    model: d[5].as_str().unwrap_or(UNKNOWN_MODEL).to_string(),
                    tokens,
                    cost_usd: d[7].as_f64(),
                    dedup_key: id,
                    project: directory.unwrap_or_default(),
                },
            ));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Devin
// ---------------------------------------------------------------------------

/// Devin CLI: `~/.local/share/devin/cli/sessions.db`. Per-message usage lives in
/// `message_nodes.chat_message.metadata.metrics`; model on the `sessions` row.
pub struct Devin;

fn devin_db() -> PathBuf {
    xdg_data().join("devin/cli/sessions.db")
}

impl AgentReader for Devin {
    fn id(&self) -> &'static str {
        "devin"
    }
    fn display(&self) -> &'static str {
        "Devin"
    }
    fn detect(&self) -> bool {
        devin_db().is_file()
    }
    fn entries(&self) -> Vec<UsageEntry> {
        let path = devin_db();
        let scan_path = path.clone();
        cached_scan(
            &path,
            "row_id",
            "message_nodes",
            "devin",
            "devin",
            &move |lo, hi| Self::scan_range(&scan_path, lo, hi),
        )
    }
}

impl Devin {
    fn scan_range(path: &Path, lo: i64, hi: i64) -> Vec<(i64, UsageEntry)> {
        let Some(conn) = open_db(path) else {
            return vec![];
        };
        let mut stmt = match conn.prepare(
            "SELECT n.row_id, n.session_id, n.created_at, \
                    json_extract(n.chat_message, \
                        '$.metadata.metrics', '$.metadata.request_id', \
                        '$.metadata.created_at', '$.metadata.cost'), \
                    s.model, s.working_directory \
             FROM message_nodes n JOIN sessions s ON s.id = n.session_id \
             WHERE n.row_id >= ?1 AND n.row_id <= ?2 \
               AND json_extract(n.chat_message, '$.metadata.metrics.input_tokens') IS NOT NULL",
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
            let Ok(d) = serde_json::from_str::<serde_json::Value>(&arr) else {
                continue;
            };
            let met = &d[0];
            let tokens = Tokens {
                input: num(met, "input_tokens"),
                output: num(met, "output_tokens"),
                reasoning: 0,
                cache_creation: num(met, "cache_creation_tokens"),
                cache_read: num(met, "cache_read_tokens"),
            };
            if tokens.total_no_reasoning() == 0 {
                continue;
            }
            let ts = parse_ts(&d[2])
                .filter(|t| *t > 0)
                .unwrap_or_else(|| epoch_to_ms(created));
            let cost = match &d[3] {
                serde_json::Value::Number(n) => n.as_f64(),
                serde_json::Value::Object(o) => o
                    .get("costUSD")
                    .or_else(|| o.get("cost"))
                    .and_then(|c| c.as_f64()),
                _ => None,
            };
            out.push((
                row_id,
                UsageEntry {
                    agent: "devin",
                    timestamp_ms: ts,
                    session_id: session,
                    model: model.unwrap_or_else(|| UNKNOWN_MODEL.into()),
                    tokens,
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

// ---------------------------------------------------------------------------
// ZCode
// ---------------------------------------------------------------------------

/// ZCode (z.ai/GLM) CLI: `~/.zcode/cli/db/db.sqlite`. One `model_usage` row
/// per logical request.
pub struct ZCode;

fn zcode_db() -> PathBuf {
    home().join(".zcode/cli/db/db.sqlite")
}

impl AgentReader for ZCode {
    fn id(&self) -> &'static str {
        "zcode"
    }
    fn display(&self) -> &'static str {
        "ZCode"
    }
    fn detect(&self) -> bool {
        zcode_db().is_file()
    }
    fn entries(&self) -> Vec<UsageEntry> {
        let Some(conn) = open_db(&zcode_db()) else {
            return vec![];
        };
        let mut stmt = match conn.prepare(
            "SELECT id, session_id, started_at, model_id, provider_id, \
                    input_tokens, output_tokens, reasoning_tokens, \
                    cache_creation_input_tokens, cache_read_input_tokens \
             FROM model_usage \
             WHERE input_tokens > 0 OR output_tokens > 0 \
                OR cache_read_input_tokens > 0 OR cache_creation_input_tokens > 0",
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
            .map(
                |(id, session, started, model, provider, i, o, r_, cc, cr)| UsageEntry {
                    agent: "zcode",
                    timestamp_ms: epoch_to_ms(started),
                    session_id: session,
                    model: if model.is_empty() { provider } else { model },
                    // zcode input_tokens include cache reads; upstream subtracts
                    tokens: Tokens {
                        input: (i - cr).max(0) as u64,
                        output: o.max(0) as u64,
                        reasoning: r_.max(0) as u64,
                        cache_creation: cc.max(0) as u64,
                        cache_read: cr.max(0) as u64,
                    },
                    cost_usd: None,
                    dedup_key: id,
                    project: String::new(),
                },
            )
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Zed
// ---------------------------------------------------------------------------

/// Zed agent threads: `~/.local/share/zed/threads/threads.db`. `data` is a
/// serialized blob — best effort: parse JSON if possible, else skip.
pub struct Zed;

fn zed_db() -> PathBuf {
    xdg_data().join("zed/threads/threads.db")
}

impl AgentReader for Zed {
    fn id(&self) -> &'static str {
        "zed"
    }
    fn display(&self) -> &'static str {
        "Zed"
    }
    fn detect(&self) -> bool {
        zed_db().is_file()
    }
    fn entries(&self) -> Vec<UsageEntry> {
        let Some(conn) = open_db(&zed_db()) else {
            return vec![];
        };
        let mut stmt = match conn.prepare("SELECT id, data, updated_at FROM threads") {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map(|r| r.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut out = vec![];
        for (id, data, updated) in rows {
            let Ok(text) = String::from_utf8(data) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            collect_usage_recursive(&v, &id, &updated, &mut out);
        }
        out
    }
}

/// Walk a thread's JSON for anything usage-shaped (Zed's schema is not
/// stable enough to query directly).
fn collect_usage_recursive(
    v: &serde_json::Value,
    session: &str,
    updated: &str,
    out: &mut Vec<UsageEntry>,
) {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(u) = map.get("usage").filter(|u| u.is_object()) {
                let tokens = Tokens {
                    input: num(u, "input_tokens").max(num(u, "inputTokens")),
                    output: num(u, "output_tokens").max(num(u, "outputTokens")),
                    reasoning: num(u, "reasoning_tokens").max(num(u, "reasoningTokens")),
                    cache_creation: num(u, "cache_creation_input_tokens")
                        .max(num(u, "cacheCreationTokens")),
                    cache_read: num(u, "cache_read_input_tokens").max(num(u, "cachedReadTokens")),
                };
                if tokens.total_no_reasoning() > 0 {
                    let ts = map
                        .get("timestamp")
                        .and_then(parse_ts)
                        .or_else(|| parse_ts(&serde_json::Value::String(updated.to_string())))
                        .unwrap_or(0);
                    out.push(UsageEntry {
                        agent: "zed",
                        timestamp_ms: ts,
                        session_id: session.to_string(),
                        model: map
                            .get("model")
                            .and_then(|m| m.as_str())
                            .unwrap_or(UNKNOWN_MODEL)
                            .to_string(),
                        tokens,
                        cost_usd: None,
                        dedup_key: format!("{session}:{}", out.len()),
                        project: String::new(),
                    });
                }
            }
            for val in map.values() {
                collect_usage_recursive(val, session, updated, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for val in arr {
                collect_usage_recursive(val, session, updated, out);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Cursor
// ---------------------------------------------------------------------------

/// Cursor agent CLI: `~/.cursor/ai-tracking/ai-code-tracking.db`. Cursor's
/// schema isn't public, so this best-effort reader sniffs for tables that
/// carry token-shaped columns.
pub struct Cursor;

fn cursor_db() -> PathBuf {
    home().join(".cursor/ai-tracking/ai-code-tracking.db")
}

impl AgentReader for Cursor {
    fn id(&self) -> &'static str {
        "cursor"
    }
    fn display(&self) -> &'static str {
        "Cursor"
    }
    fn detect(&self) -> bool {
        cursor_db().is_file()
    }
    fn entries(&self) -> Vec<UsageEntry> {
        let Some(conn) = open_db(&cursor_db()) else {
            return vec![];
        };
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
            let Ok(mut stmt) = conn.prepare(&sql) else {
                continue;
            };
            let colnames: Vec<String> = stmt
                .column_names()
                .into_iter()
                .map(|s| s.to_string())
                .collect();
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
                    i.and_then(|i| row.get::<_, i64>(i).ok())
                        .map(|n| n.max(0) as u64)
                        .unwrap_or(0)
                };
                let gstr = |i: Option<usize>| -> String {
                    i.and_then(|i| row.get::<_, String>(i).ok())
                        .unwrap_or_default()
                };
                let tokens = Tokens {
                    input: gnum(ci),
                    output: gnum(co),
                    reasoning: 0,
                    cache_creation: gnum(ccc),
                    cache_read: gnum(ccr),
                };
                if tokens.is_zero() {
                    continue;
                }
                let ts = cts
                    .and_then(|i| row.get::<_, rusqlite::types::Value>(i).ok())
                    .and_then(|v| match v {
                        rusqlite::types::Value::Integer(n) => Some(epoch_to_ms(n)),
                        rusqlite::types::Value::Text(t) => parse_ts(&serde_json::Value::String(t)),
                        _ => None,
                    })
                    .unwrap_or(0);
                out.push(UsageEntry {
                    agent: "cursor",
                    timestamp_ms: ts,
                    session_id: gstr(csess),
                    model: gstr(cmodel),
                    tokens,
                    cost_usd: None,
                    dedup_key: gstr(cid),
                    project: String::new(),
                });
            }
        }
        out
    }
}
