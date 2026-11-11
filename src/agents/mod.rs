mod jsonl;
mod sqlite;

use crate::types::UsageEntry;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A usage data source for one coding agent.
pub trait AgentReader: Send + Sync {
    fn id(&self) -> &'static str;
    fn display(&self) -> &'static str;
    /// Data directory/file exists on this machine.
    fn detect(&self) -> bool;
    /// All usage entries, unfiltered. Caller dedups globally.
    fn entries(&self) -> Vec<UsageEntry>;
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

pub fn xdg_data() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".local/share"))
}

pub fn xdg_config() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config"))
}

/// Recursively collect files matching a predicate.
pub fn walk(dir: &Path, pred: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>, depth: u8) {
    if depth == 0 || !dir.is_dir() {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let ft = match e.file_type() {
            Ok(f) => f,
            Err(_) => continue,
        };
        if ft.is_dir() {
            walk(&p, pred, out, depth - 1);
        } else if ft.is_file() && pred(&p) {
            out.push(p);
        }
    }
}

pub fn is_jsonl(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "jsonl")
}

/// Parse an ISO-8601/RFC3339 timestamp or epoch seconds/millis into ms.
pub fn parse_ts(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::String(s) => {
            if let Ok(t) = s.trim().parse::<jiff::Timestamp>() {
                Some(t.as_millisecond())
            } else if let Ok(n) = s.trim().parse::<i64>() {
                Some(epoch_to_ms(n))
            } else {
                None
            }
        }
        serde_json::Value::Number(n) => n.as_i64().map(epoch_to_ms),
        _ => None,
    }
}

fn epoch_to_ms(n: i64) -> i64 {
    let a = n.abs();
    if a < 100_000_000_000 {
        n * 1000 // seconds
    } else if a < 100_000_000_000_000 {
        n // millis
    } else if a < 100_000_000_000_000_000 {
        n / 1_000 // micros
    } else {
        n / 1_000_000 // nanos
    }
}

/// Open a SQLite database read-only, tolerating live WAL writers.
pub fn open_db(path: &Path) -> Option<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

pub fn xdg_cache() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".cache"))
        .join("rsusage")
}

/// Incremental entry cache for SQLite readers. Source tables use monotonic
/// rowids, so each run only re-scans rows beyond `watermark - TAIL`.
pub struct EntryCache {
    conn: rusqlite::Connection,
    table: String,
    meta_table: String,
}

pub const CACHE_TAIL: i64 = 2000;

impl EntryCache {
    pub fn open(agent: &str) -> Option<Self> {
        let dir = xdg_cache();
        std::fs::create_dir_all(&dir).ok()?;
        let conn = rusqlite::Connection::open(dir.join("agent-cache.db")).ok()?;
        let table = format!("entries_{agent}");
        let meta_table = "cache_meta".to_string();
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {table} (                src_rowid INTEGER PRIMARY KEY, ts INTEGER, session TEXT, model TEXT,                 input INTEGER, output INTEGER, reasoning INTEGER, cc INTEGER, cr INTEGER,                 cost REAL, dedup TEXT, project TEXT, agent TEXT);             CREATE TABLE IF NOT EXISTS {meta_table} (agent TEXT PRIMARY KEY, wm INTEGER, source TEXT);"
        ))
        .ok()?;
        Some(Self { conn, table, meta_table })
    }

    pub fn watermark(&mut self, source: &str) -> i64 {
        // invalidate when the source file differs from what we cached
        let row: Option<(i64, String)> = self
            .conn
            .query_row(
                &format!("SELECT wm, source FROM {} WHERE agent=?1", self.meta_table),
                [&self.table],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        match row {
            Some((wm, src)) if src == source => wm,
            _ => {
                let _ = self
                    .conn
                    .execute(&format!("DELETE FROM {}", self.table), []);
                0
            }
        }
    }

    pub fn set_watermark(&mut self, wm: i64, source: &str) {
        let _ = self.conn.execute(
            &format!(
                "INSERT INTO {} (agent, wm, source) VALUES (?1,?2,?3)                  ON CONFLICT(agent) DO UPDATE SET wm=excluded.wm, source=excluded.source",
                self.meta_table
            ),
            rusqlite::params![self.table, wm, source],
        );
    }

    /// Delete cached rows in [lo, hi] then insert the fresh (rowid, entry) pairs.
    pub fn refresh(&self, lo: i64, hi: i64, rows: &[(i64, UsageEntry)]) {
        let _ = self.conn.execute_batch("BEGIN IMMEDIATE");
        let _ = self.conn.execute(
            &format!("DELETE FROM {} WHERE src_rowid BETWEEN ?1 AND ?2", self.table),
            rusqlite::params![lo, hi],
        );
        {
            let Ok(mut st) = self.conn.prepare(&format!(
                "INSERT OR REPLACE INTO {} \
                 (src_rowid, ts, session, model, input, output, reasoning, cc, cr, cost, dedup, project, agent) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                self.table
            )) else {
                let _ = self.conn.execute_batch("ROLLBACK");
                return;
            };
            for (rid, e) in rows {
                let _ = st.execute(rusqlite::params![
                    rid, e.timestamp_ms, e.session_id, e.model, e.input, e.output,
                    e.reasoning, e.cache_creation, e.cache_read, e.cost_usd,
                    e.dedup_key, e.project, e.agent,
                ]);
            }
        }
        let _ = self.conn.execute_batch("COMMIT");
    }

    pub fn all(&self, agent: &'static str) -> Vec<UsageEntry> {
        let mut st = match self.conn.prepare(&format!(
            "SELECT ts, session, model, input, output, reasoning, cc, cr, cost, dedup, project, agent FROM {}",
            self.table
        )) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        st.query_map([], |r| {
            Ok(UsageEntry {
                timestamp_ms: r.get(0)?,
                session_id: r.get(1)?,
                model: r.get(2)?,
                input: r.get(3)?,
                output: r.get(4)?,
                reasoning: r.get(5)?,
                cache_creation: r.get(6)?,
                cache_read: r.get(7)?,
                cost_usd: r.get(8)?,
                dedup_key: r.get(9)?,
                project: r.get(10)?,
                agent,
            })
        })
        .map(|r| r.flatten().collect())
        .unwrap_or_default()
    }
}

fn num(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

fn fnum(v: &serde_json::Value, key: &str) -> Option<f64> {
    v.get(key).and_then(|x| x.as_f64())
}

fn strv<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// Globally dedup entries by `agent:dedup_key`. Empty keys always unique (row id).
pub fn dedup(entries: Vec<UsageEntry>) -> Vec<UsageEntry> {
    let mut seen = HashSet::with_capacity(entries.len());
    let mut out = Vec::with_capacity(entries.len());
    let mut anon = 0u64;
    for e in entries {
        let key = if e.dedup_key.is_empty() {
            anon += 1;
            format!("{}:anon:{}", e.agent, anon)
        } else {
            format!("{}:{}", e.agent, e.dedup_key)
        };
        if seen.insert(key) {
            out.push(e);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub fn all_agents() -> Vec<Box<dyn AgentReader>> {
    vec![
        Box::new(jsonl::Claude),
        Box::new(jsonl::Codex),
        Box::new(sqlite::OpenCodeDb {
            id: "opencode",
            display: "OpenCode",
            path: xdg_data().join("opencode/opencode.db"),
        }),
        Box::new(sqlite::OpenCodeDb {
            id: "kilo",
            display: "Kilo",
            path: xdg_data().join("kilo/kilo.db"),
        }),
        Box::new(sqlite::ZCode),
        Box::new(sqlite::Devin),
        Box::new(jsonl::Grok),
        Box::new(sqlite::Zed),
        Box::new(sqlite::Cursor),
        Box::new(jsonl::Gemini),
        Box::new(jsonl::GenericJsonl {
            id: "amp",
            display: "Amp",
            dirs: vec![
                xdg_data().join("amp"),
                xdg_config().join("amp"),
                xdg_config().join("cliamp"),
            ],
        }),
        Box::new(jsonl::GenericJsonl {
            id: "qwen",
            display: "Qwen",
            dirs: vec![home().join(".qwen"), xdg_config().join("qwen")],
        }),
        Box::new(jsonl::GenericJsonl {
            id: "kimi",
            display: "Kimi",
            dirs: vec![home().join(".kimi"), xdg_config().join("kimi")],
        }),
        Box::new(jsonl::GenericJsonl {
            id: "goose",
            display: "Goose",
            dirs: vec![xdg_data().join("goose"), xdg_config().join("goose")],
        }),
        Box::new(jsonl::GenericJsonl {
            id: "copilot",
            display: "Copilot",
            dirs: vec![
                home().join(".copilot"),
                xdg_config().join("github-copilot"),
            ],
        }),
        Box::new(jsonl::GenericJsonl {
            id: "droid",
            display: "Droid",
            dirs: vec![home().join(".factory"), xdg_config().join("droid")],
        }),
        Box::new(jsonl::GenericJsonl {
            id: "codebuff",
            display: "Codebuff",
            dirs: vec![home().join(".codebuff"), xdg_data().join("codebuff")],
        }),
        Box::new(jsonl::GenericJsonl {
            id: "hermes",
            display: "Hermes",
            dirs: vec![home().join(".hermes"), xdg_config().join("hermes")],
        }),
        Box::new(jsonl::GenericJsonl {
            id: "pi",
            display: "Pi",
            dirs: vec![home().join(".pi"), xdg_config().join("pi")],
        }),
        Box::new(jsonl::GenericJsonl {
            id: "openclaw",
            display: "OpenClaw",
            dirs: vec![home().join(".openclaw"), xdg_config().join("openclaw")],
        }),
        Box::new(jsonl::GenericJsonl {
            id: "antigravity",
            display: "Antigravity",
            dirs: vec![home().join(".antigravity"), xdg_config().join("antigravity")],
        }),
    ]
}

pub fn by_id(id: &str) -> Option<Box<dyn AgentReader>> {
    all_agents().into_iter().find(|a| a.id() == id)
}

/// Display name for an agent id, falling back to title-casing.
pub fn display_for(id: &str) -> String {
    by_id(id)
        .map(|r| r.display().to_string())
        .unwrap_or_else(|| {
            let mut c = id.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => id.to_string(),
            }
        })
}
