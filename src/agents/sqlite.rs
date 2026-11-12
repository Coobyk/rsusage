//! SQLite-store readers: OpenCode, Kilo (opencode-fork schema), ZCode, Devin,
//! Zed, Cursor.
use super::{dedup_hash, epoch_to_ms, num, parse_ts, AgentReader};
use crate::paths::{self, home, xdg_data};
use crate::types::{Tokens, UsageEntry, UNKNOWN_MODEL};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::hash::Hasher;
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
/// rowids, so each run only re-scans rows beyond `watermark - CACHE_TAIL`
/// and APPENDS the result as one fixed-stride chunk in the cache's flat
/// blob file (`blobs/{name}.bin`) — instead of rewriting the agent's whole
/// history. Reads slurp the file in one `fs::read`, decode all chunks in
/// parallel and merge last-chunk-wins per src_rowid (the CACHE_TAIL rescan
/// rewrites those rows), sorted by rowid — the same order the single-blob
/// format produced. Watermarks and source signatures live in
/// `agent-cache.db`, which also supplies the cross-process write lock.
/// `agent` is implied by the per-agent file and skipped inside the blob;
/// `dedup_key` is skipped too — its `agent:key` hash is stored instead.
///
/// File readers reuse the same blob files via `sig_scan`: their whole state
/// lives in one chunk under a synthetic index, replaced on any source-file
/// signature change.
///
/// All failures are treated as "cache unavailable" — the source scan is the
/// source of truth, so a broken cache must degrade to a full scan, not an
/// error.
pub struct EntryCache {
    /// metadata + inter-process write lock only — the blobs live in a flat
    /// file, read with one `fs::read` instead of a row-at-a-time blob copy
    conn: rusqlite::Connection,
    name: String,
    path: PathBuf,
}

const CACHE_TAIL: i64 = 2000;

/// Bump when the stored blob layout changes — old blobs are undecodable, so
/// every cached entry, watermark and source signature is dropped.
const CACHE_FORMAT: i64 = 10;

/// Chunk blob layout — hand-rolled, fixed-stride so decode is offset math
/// and can fan out by byte range:
///   u32 MAGIC | u32 n_rows | u32 n_strings | i64 min_rid | i64 max_rid |
///   n_strings × (u32 len + utf8 bytes)   ← intern table for s/m/p
///   n_rows × REC_SIZE records
/// min/max_rid let readers prove chunks are rowid-partitioned without
/// decoding them — the common read path then needs no sort or merge.
const CHUNK_MAGIC: u32 = 0x5253_5547; // "RSUG"
const CHUNK_HDR: usize = 28;
const REC_SIZE: usize = 104;
// record: i64 rowid | i64 ts | u64 dedup_id | u32 s | u32 m | u32 p | u32 prov+1 |
//         u64 tok_in,out,rea,cc,cr | f64 cost (NaN = none) |
//         u64 total_override+1 | u64 flags (bit0 = is_fallback)
// prov+1: string-table index of the provider id plus one; 0 = no provider, so
// blobs written before providers existed decode as provider-less.
// total_override+1 / flags: 0 = none — pre-v8 blobs decode as unflagged.

/// Serialize `(rowid, entry)` rows into one interned chunk blob.
fn encode_chunk(rows: &[(i64, UsageEntry)]) -> Option<Vec<u8>> {
    let mut table: Vec<String> = Vec::new();
    let mut index: FxHashMap<String, u32> = FxHashMap::default();
    let mut intern = |s: &str| -> u32 {
        if let Some(&i) = index.get(s) {
            return i;
        }
        let i = table.len() as u32;
        table.push(s.to_string());
        index.insert(s.to_string(), i);
        i
    };
    let mut recs = Vec::with_capacity(rows.len() * REC_SIZE);
    for (rowid, e) in rows {
        // entries already decoded once carry their hash — re-encoding during
        // compaction must pass it through verbatim, not recompute it
        let did = if e.dedup_id != 0 {
            e.dedup_id
        } else if e.dedup_key.is_empty() {
            0
        } else {
            dedup_hash(e.agent, e.dedup_key.as_bytes())
        };
        for b in [
            &rowid.to_le_bytes()[..],
            &e.timestamp_ms.to_le_bytes(),
            &did.to_le_bytes(),
            &intern(&e.session_id).to_le_bytes(),
            &intern(&e.model).to_le_bytes(),
            &intern(&e.project).to_le_bytes(),
            &(if e.provider.is_empty() {
                0u32
            } else {
                intern(&e.provider) + 1
            })
            .to_le_bytes(),
            &e.tokens.input.to_le_bytes(),
            &e.tokens.output.to_le_bytes(),
            &e.tokens.reasoning.to_le_bytes(),
            &e.tokens.cache_creation.to_le_bytes(),
            &e.tokens.cache_read.to_le_bytes(),
            &e.cost_usd.unwrap_or(f64::NAN).to_le_bytes(),
            &(e.total_override.map_or(0u64, |t| t + 1)).to_le_bytes(),
            &(e.is_fallback as u64).to_le_bytes(),
        ] {
            recs.extend_from_slice(b);
        }
    }
    let table_len: usize = table.iter().map(|s| 4 + s.len()).sum();
    let mut out = Vec::with_capacity(CHUNK_HDR + table_len + recs.len());
    out.extend_from_slice(&CHUNK_MAGIC.to_le_bytes());
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    out.extend_from_slice(&(table.len() as u32).to_le_bytes());
    // chunks are written rowid-sorted — first/last are the range bounds;
    // an empty chunk gets an inverted range so it never counts as overlapping
    out.extend_from_slice(&rows.first().map(|r| r.0).unwrap_or(i64::MAX).to_le_bytes());
    out.extend_from_slice(&rows.last().map(|r| r.0).unwrap_or(i64::MIN).to_le_bytes());
    for s in &table {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    out.extend_from_slice(&recs);
    Some(out)
}

/// Decode a chunk blob. Every offset is bounds-checked — a corrupt blob is
/// `None`, never a panic.
fn decode_chunk(blob: &[u8]) -> Option<Vec<(i64, UsageEntry)>> {
    let (recs, table, n) = split_chunk(blob)?;
    let rows: Option<Vec<Vec<(i64, UsageEntry)>>> = recs
        .par_chunks(REC_SIZE * 4096)
        .map(|blk| decode_records(blk, &table))
        .collect();
    let mut out = Vec::with_capacity(n);
    for chunk in rows? {
        out.extend(chunk);
    }
    Some(out)
}

type ChunkParts<'a> = (&'a [u8], Vec<std::sync::Arc<str>>, usize);

/// Header check + string-table + record region as raw byte ranges — the
/// decoders intern the strings, the truncator just copies the ranges.
fn chunk_regions(blob: &[u8]) -> Option<(&[u8], &[u8], usize)> {
    let u32_at = |o: usize| -> Option<u32> {
        Some(u32::from_le_bytes(blob.get(o..o + 4)?.try_into().ok()?))
    };
    if u32_at(0)? != CHUNK_MAGIC {
        return None;
    }
    let n = u32_at(4)? as usize;
    let n_str = u32_at(8)? as usize;
    let mut cur = CHUNK_HDR;
    for _ in 0..n_str {
        let l = u32_at(cur)? as usize;
        cur += 4;
        blob.get(cur..cur + l)?;
        cur += l;
    }
    let table = blob.get(CHUNK_HDR..cur)?;
    let recs = blob.get(cur..)?;
    if recs.len() != n.checked_mul(REC_SIZE)? {
        return None;
    }
    Some((table, recs, n))
}

/// Header check + string table + record region, shared by both decoders.
fn split_chunk(blob: &[u8]) -> Option<ChunkParts<'_>> {
    let (table_bytes, recs, n) = chunk_regions(blob)?;
    let mut table: Vec<std::sync::Arc<str>> = Vec::new();
    let mut cur = 0usize;
    while cur < table_bytes.len() {
        let l = u32::from_le_bytes(table_bytes.get(cur..cur + 4)?.try_into().ok()?) as usize;
        cur += 4;
        table.push(
            std::str::from_utf8(table_bytes.get(cur..cur + l)?)
                .ok()?
                .into(),
        );
        cur += l;
    }
    (cur == table_bytes.len()).then_some((recs, table, n))
}

/// Rebuild a chunk keeping only records with `rowid < below`. Records are
/// rowid-sorted inside a chunk, so this is a binary search + byte slices —
/// no entry decode, no re-encode. `None` only on a corrupt chunk.
fn truncate_chunk(blob: &[u8], below: i64) -> Option<Vec<u8>> {
    let (table, recs, _n) = chunk_regions(blob)?;
    let (recs, _) = recs.as_chunks::<REC_SIZE>();
    let rowid = |r: &[u8; REC_SIZE]| i64::from_le_bytes(r[..8].try_into().unwrap());
    let keep = recs.partition_point(|r| rowid(r) < below);
    if keep == 0 {
        return Some(vec![]);
    }
    let kept = &recs[..keep];
    let n_str = u32::from_le_bytes(blob[8..12].try_into().ok()?);
    let mut out = Vec::with_capacity(CHUNK_HDR + table.len() + kept.len() * REC_SIZE);
    out.extend_from_slice(&CHUNK_MAGIC.to_le_bytes());
    out.extend_from_slice(&(keep as u32).to_le_bytes());
    out.extend_from_slice(&n_str.to_le_bytes());
    out.extend_from_slice(&rowid(&kept[0]).to_le_bytes());
    out.extend_from_slice(&rowid(&kept[keep - 1]).to_le_bytes());
    out.extend_from_slice(table);
    for r in kept {
        out.extend_from_slice(r);
    }
    Some(out)
}

/// Just the header's `(min_rid, max_rid)` — no string table, no records.
fn chunk_range(blob: &[u8]) -> Option<(i64, i64)> {
    if blob.len() < CHUNK_HDR || u32::from_le_bytes(blob[0..4].try_into().ok()?) != CHUNK_MAGIC {
        return None;
    }
    Some((
        i64::from_le_bytes(blob[12..20].try_into().ok()?),
        i64::from_le_bytes(blob[20..28].try_into().ok()?),
    ))
}

/// Entry-only decode for the partitioned fast path — no rowids, no tuples.
/// Two passes on purpose: parallel blocks emit compact index-only records
/// (no `Arc` touches — interned-string refcounts are heavily contended
/// cache lines), then a single-threaded resolve clones each `Arc` once.
fn decode_chunk_entries(blob: &[u8], agent: &'static str) -> Option<Vec<UsageEntry>> {
    let (recs, table, n) = split_chunk(blob)?;
    let n_str = table.len();
    let compact: Option<Vec<Vec<CRec>>> = recs
        .par_chunks(REC_SIZE * 4096)
        .map(|blk| {
            blk.as_chunks::<REC_SIZE>()
                .0
                .iter()
                .map(|r| {
                    let i64_at = |o: usize| i64::from_le_bytes(r[o..o + 8].try_into().unwrap());
                    let u64_at = |o: usize| u64::from_le_bytes(r[o..o + 8].try_into().unwrap());
                    let u32_at = |o: usize| u32::from_le_bytes(r[o..o + 4].try_into().unwrap());
                    let idx = |o: usize| -> Option<u32> {
                        let i = u32_at(o);
                        ((i as usize) < n_str).then_some(i)
                    };
                    let c = f64::from_le_bytes(r[80..88].try_into().unwrap());
                    Some(CRec {
                        ts: i64_at(8),
                        dedup_id: u64_at(16),
                        s0: idx(24)?,
                        s1: idx(28)?,
                        s2: idx(32)?,
                        s3: u32_at(36),
                        tokens: [
                            u64_at(40),
                            u64_at(48),
                            u64_at(56),
                            u64_at(64),
                            u64_at(72),
                        ],
                        cost: c,
                        total_override: u64_at(88),
                        flags: u64_at(96),
                    })
                })
                .collect()
        })
        .collect();
    let empty: std::sync::Arc<str> = "".into();
    let mut out = Vec::with_capacity(n);
    for blk in compact? {
        out.extend(blk.into_iter().map(|c| UsageEntry {
            agent,
            timestamp_ms: c.ts,
            session_id: table[c.s0 as usize].clone(),
            model: table[c.s1 as usize].clone(),
            provider: c
                .s3
                .checked_sub(1)
                .and_then(|i| table.get(i as usize).cloned())
                .unwrap_or_default(),
            tokens: Tokens {
                input: c.tokens[0],
                output: c.tokens[1],
                reasoning: c.tokens[2],
                cache_creation: c.tokens[3],
                cache_read: c.tokens[4],
            },
            cost_usd: (!c.cost.is_nan()).then_some(c.cost),
            dedup_key: empty.clone(),
            dedup_id: c.dedup_id,
            total_override: c.total_override.checked_sub(1),
            is_fallback: c.flags & 1 != 0,
            project: table[c.s2 as usize].clone(),
        }));
    }
    Some(out)
}

/// Compact decode form: indices only, `Copy` — no `Arc` contention.
struct CRec {
    ts: i64,
    dedup_id: u64,
    s0: u32,
    s1: u32,
    s2: u32,
    s3: u32,
    tokens: [u64; 5],
    cost: f64,
    total_override: u64,
    flags: u64,
}

fn decode_records(blk: &[u8], table: &[std::sync::Arc<str>]) -> Option<Vec<(i64, UsageEntry)>> {
    let i64_at = |r: &[u8], o: usize| i64::from_le_bytes(r[o..o + 8].try_into().unwrap());
    let u64_at = |r: &[u8], o: usize| u64::from_le_bytes(r[o..o + 8].try_into().unwrap());
    let u32_at = |r: &[u8], o: usize| u32::from_le_bytes(r[o..o + 4].try_into().unwrap());
    let empty: std::sync::Arc<str> = "".into();
    blk.as_chunks::<REC_SIZE>()
        .0
        .iter()
        .map(|r| {
            let get = |o: usize| table.get(u32_at(r, o) as usize).cloned();
            let c = f64::from_le_bytes(r[80..88].try_into().unwrap());
            Some((
                i64_at(r, 0),
                UsageEntry {
                    agent: "",
                    timestamp_ms: i64_at(r, 8),
                    session_id: get(24)?,
                    model: get(28)?,
                    provider: {
                        let v = u32_at(r, 36);
                        if v == 0 {
                            empty.clone()
                        } else {
                            table.get(v as usize - 1).cloned()?
                        }
                    },
                    tokens: Tokens {
                        input: u64_at(r, 40),
                        output: u64_at(r, 48),
                        reasoning: u64_at(r, 56),
                        cache_creation: u64_at(r, 64),
                        cache_read: u64_at(r, 72),
                    },
                    cost_usd: (!c.is_nan()).then_some(c),
                    dedup_key: empty.clone(),
                    dedup_id: u64_at(r, 16),
                    total_override: u64_at(r, 88).checked_sub(1),
                    is_fallback: u64_at(r, 96) & 1 != 0,
                    project: get(32)?,
                },
            ))
        })
        .collect()
}

/// Above this many appended chunks `refresh` folds them back into one —
/// bounds read amplification when a source changes on every run.
const CHUNK_LIMIT: i64 = 16;

impl EntryCache {
    fn open(name: &str) -> Option<Self> {
        let dir = paths::xdg_cache();
        std::fs::create_dir_all(&dir).ok()?;
        let conn = rusqlite::Connection::open(dir.join("agent-cache.db")).ok()?;
        // readers run in parallel against this one file; wait rather than
        // fail with SQLITE_BUSY when a sibling holds the write lock
        conn.busy_timeout(std::time::Duration::from_secs(10)).ok()?;
        // meta/schema checks are process-global — after the first opener
        // validates them, every later open on this run skips the queries
        static GLOBAL_OK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        static GLOBAL_ONCE: std::sync::Once = std::sync::Once::new();
        GLOBAL_ONCE.call_once(|| {
            GLOBAL_OK.store(
                Self::ensure_global_schema(&conn, &dir),
                std::sync::atomic::Ordering::SeqCst,
            );
        });
        if !GLOBAL_OK.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        let path = Self::blob_path(&dir, name);
        std::fs::create_dir_all(path.parent()?).ok()?;
        Some(Self {
            conn,
            name: name.to_string(),
            path,
        })
    }

    fn blob_path(dir: &Path, name: &str) -> PathBuf {
        dir.join("blobs").join(format!("{name}.bin"))
    }

    /// cache_meta + format validation — repairs run on whichever connection
    /// won the Once race and commit to the shared file. A format bump also
    /// wipes the blob files: old chunks are undecodable and would only grow.
    fn ensure_global_schema(conn: &rusqlite::Connection, dir: &Path) -> bool {
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
        if conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS cache_meta (
                    name TEXT PRIMARY KEY, wm INTEGER, source TEXT);
                 CREATE TABLE IF NOT EXISTS row_sig (
                    name TEXT, rid INTEGER, len INTEGER,
                    PRIMARY KEY (name, rid));",
            )
            .is_err()
        {
            return false;
        }
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
            let _ = std::fs::remove_dir_all(dir.join("blobs"));
            let _ = conn.execute("DELETE FROM cache_meta", []);
            let _ = conn.execute("DELETE FROM row_sig", []);
            let _ = conn.execute(
                "INSERT INTO cache_meta (name, wm, source) VALUES ('__format', ?1, '')",
                [CACHE_FORMAT],
            );
        }
        true
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
                self.clear();
                0
            }
        }
    }

    /// Drop the stored blob, watermark and signature — the next read is a
    /// full scan.
    fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = self.conn.execute(
            "DELETE FROM cache_meta WHERE name=?1 OR name=?2",
            rusqlite::params![self.name, format!("{}.sig", self.name)],
        );
        let _ = self
            .conn
            .execute("DELETE FROM row_sig WHERE name=?1", [&self.name]);
    }

    /// `(rowid, source-length)` for every row seen in `[lo, ∞)`. SQLite reads
    /// blob lengths from the record header, so filling this costs nothing —
    /// and it lets a rescan tell "filtered row" apart from "never seen".
    fn sig_rows(&self, lo: i64) -> rustc_hash::FxHashMap<i64, u32> {
        self.conn
            .prepare("SELECT rid, len FROM row_sig WHERE name=?1 AND rid>=?2")
            .ok()
            .and_then(|mut s| {
                s.query_map(rusqlite::params![self.name, lo], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u32))
                })
                .ok()
                .map(|it| it.flatten().collect())
            })
            .unwrap_or_default()
    }

    /// Refresh the signature map inside the caller's write lock: upsert rows
    /// this scan observed, drop the ones the source deleted.
    fn apply_sig_delta(&self, seen: &[(i64, u32)], gone: &[i64]) -> Option<()> {
        let mut upsert = self
            .conn
            .prepare("INSERT OR REPLACE INTO row_sig (name, rid, len) VALUES (?1,?2,?3)")
            .ok()?;
        for (rid, len) in seen {
            upsert
                .execute(rusqlite::params![self.name, rid, *len as i64])
                .ok()?;
        }
        let mut del = self
            .conn
            .prepare("DELETE FROM row_sig WHERE name=?1 AND rid=?2")
            .ok()?;
        for rid in gone {
            del.execute(rusqlite::params![self.name, rid]).ok()?;
        }
        Some(())
    }

    fn set_watermark(&self, wm: i64, source: &str) {
        let _ = self.conn.execute(
            "INSERT INTO cache_meta (name, wm, source) VALUES (?1,?2,?3)
             ON CONFLICT(name) DO UPDATE SET wm=excluded.wm, source=excluded.source",
            rusqlite::params![self.name, wm, source],
        );
    }

    /// Source-file signature recorded at last scan (see `src_sig`), kept in a
    /// `{name}.sig` companion row. A matching sig means the source file is
    /// untouched — the source query can be skipped entirely.
    fn source_sig(&self) -> Option<String> {
        self.conn
            .query_row(
                "SELECT source FROM cache_meta WHERE name=?1",
                [format!("{}.sig", self.name)],
                |r| r.get(0),
            )
            .ok()
    }

    fn set_source_sig(&self, sig: &str) {
        let _ = self.conn.execute(
            "INSERT INTO cache_meta (name, wm, source) VALUES (?1,0,?2)
             ON CONFLICT(name) DO UPDATE SET source=excluded.source",
            rusqlite::params![format!("{}.sig", self.name), sig],
        );
    }

    /// File present, parseable, and every chunk header valid — the cheap
    /// brokenness probe for the watermark path (full decode happens later).
    fn blob_file_ok(&self) -> Option<()> {
        let data = std::fs::read(&self.path).ok()?;
        read_blob_file(&data)?
            .iter()
            .all(|c| chunk_range(c).is_some())
            .then_some(())
    }

    /// Append the freshly scanned rows as one new chunk — last-wins on read
    /// replaces the tail rows the scan re-covered, so no delete is needed.
    /// Past CHUNK_LIMIT chunks everything folds back into shards. The
    /// BEGIN IMMEDIATE on the meta db serializes writers across processes;
    /// the file itself is swapped in by rename. Returns false unless the
    /// write committed — callers must not advance the watermark on failure,
    /// or the unscanned range would be lost for good.
    fn refresh(&self, rows: &[(i64, UsageEntry)]) -> bool {
        if self.conn.execute_batch("BEGIN IMMEDIATE").is_err() {
            return false;
        }
        let committed = (|| {
            // under the write lock the file is stable — build the new chunk
            // list entirely in memory before the single atomic rewrite
            let data = std::fs::read(&self.path).unwrap_or_default();
            let chunks = read_blob_file(&data).unwrap_or_default();
            if rows.len() > Self::SHARD_ROWS && chunks.is_empty() {
                // a full rebuild gets parallel-decode shards, not one blob
                let shards = shard_blobs(rows)?;
                let refs: Vec<&[u8]> = shards.iter().map(|s| s.as_slice()).collect();
                return write_blob_file(&self.path, &refs);
            }
            // drop every cached row the tail rescan re-covers — the file
            // stays rowid-partitioned, so reads never have to merge (this
            // also drops rows deleted from the source, which append-only
            // merging used to keep forever)
            let fresh_min = rows.first().map(|r| r.0).unwrap_or(i64::MAX);
            let mut kept: Vec<std::borrow::Cow<'_, [u8]>> = Vec::with_capacity(chunks.len() + 1);
            for c in chunks {
                match chunk_range(c) {
                    Some((_, mx)) if mx < fresh_min => kept.push(c.into()),
                    Some((mn, mx)) if mn < fresh_min && fresh_min <= mx => {
                        // straddles the rescan boundary — byte-slice off the
                        // re-covered tail records, no decode needed
                        let t = truncate_chunk(c, fresh_min)?;
                        if !t.is_empty() {
                            kept.push(t.into());
                        }
                    }
                    _ => {} // entirely re-covered — the fresh rows replace it
                }
            }
            kept.push(encode_chunk(rows)?.into());
            if kept.len() as i64 > CHUNK_LIMIT {
                let refs: Vec<&[u8]> = kept.iter().map(|c| c.as_ref()).collect();
                let merged = decode_merged(&refs)?;
                kept = shard_blobs(&merged)?
                    .into_iter()
                    .map(std::borrow::Cow::Owned)
                    .collect();
            }
            let refs: Vec<&[u8]> = kept.iter().map(|c| c.as_ref()).collect();
            write_blob_file(&self.path, &refs)
        })()
        .is_some();
        self.conn
            .execute_batch(if committed { "COMMIT" } else { "ROLLBACK" })
            .is_ok()
            && committed
    }

    /// Sparse update driven by a `(rowid → blob-length)` probe: `fresh`
    /// replaces same-rowid records inside their containing chunk and fills
    /// coverage gaps, `gone` drops records for rows deleted from the source.
    /// Chunks no `fresh`/`gone` rowid touches keep their bytes untouched —
    /// only affected shards decode. `seen` refreshes the signature map.
    fn refresh_precise(
        &self,
        fresh: &[(i64, UsageEntry)],
        gone: &[i64],
        seen: &[(i64, u32)],
    ) -> bool {
        if self.conn.execute_batch("BEGIN IMMEDIATE").is_err() {
            return false;
        }
        let committed = (|| {
            let data = std::fs::read(&self.path).unwrap_or_default();
            let chunks = read_blob_file(&data).unwrap_or_default();
            let ranges: Option<Vec<(i64, i64)>> = chunks.iter().map(|c| chunk_range(c)).collect();
            let ranges = ranges?;
            let gone_set: rustc_hash::FxHashSet<i64> = gone.iter().copied().collect();

            // Route each fresh row to the chunk whose range contains its
            // rowid; gap rows extend the FOLLOWING chunk's min; rows past
            // every chunk append as tail shards.
            let mut in_chunk: Vec<Vec<(i64, &UsageEntry)>> = vec![vec![]; chunks.len()];
            let mut extra: Vec<(i64, &UsageEntry)> = Vec::new();
            for (r, e) in fresh {
                let idx = ranges.partition_point(|&(mn, _)| mn <= *r);
                if idx > 0 && *r <= ranges[idx - 1].1 {
                    in_chunk[idx - 1].push((*r, e));
                } else if idx < chunks.len() {
                    in_chunk[idx].push((*r, e));
                } else {
                    extra.push((*r, e));
                }
            }

            let mut out: Vec<std::borrow::Cow<'_, [u8]>> = Vec::with_capacity(chunks.len() + 4);
            for (i, c) in chunks.iter().enumerate() {
                let (mn, mx) = ranges[i];
                let touches = !in_chunk[i].is_empty() || gone.iter().any(|r| mn <= *r && *r <= mx);
                if !touches {
                    out.push((*c).into());
                    continue;
                }
                let rows = decode_chunk(c)?;
                // sorted merge: fresh wins a rowid collision, gap rows slot
                // at their position, gone rows drop — everything stays
                // rowid-sorted so the chunk partition is preserved
                let mut merged: Vec<(i64, UsageEntry)> =
                    Vec::with_capacity(rows.len() + in_chunk[i].len());
                let mut fi = 0;
                for (r, e) in rows {
                    if gone_set.contains(&r) {
                        continue;
                    }
                    while fi < in_chunk[i].len() && in_chunk[i][fi].0 < r {
                        merged.push((in_chunk[i][fi].0, in_chunk[i][fi].1.clone()));
                        fi += 1;
                    }
                    if fi < in_chunk[i].len() && in_chunk[i][fi].0 == r {
                        merged.push((r, in_chunk[i][fi].1.clone()));
                        fi += 1;
                    } else {
                        merged.push((r, e));
                    }
                }
                while fi < in_chunk[i].len() {
                    merged.push((in_chunk[i][fi].0, in_chunk[i][fi].1.clone()));
                    fi += 1;
                }
                out.push(encode_chunk(&merged)?.into());
            }
            let extra_owned: Vec<(i64, UsageEntry)> =
                extra.into_iter().map(|(r, e)| (r, e.clone())).collect();
            let shards = shard_blobs(&extra_owned)?;
            out.extend(shards.into_iter().map(std::borrow::Cow::Owned));
            if out.len() as i64 > CHUNK_LIMIT {
                let refs: Vec<&[u8]> = out.iter().map(|c| c.as_ref()).collect();
                let merged = decode_merged(&refs)?;
                out = shard_blobs(&merged)?
                    .into_iter()
                    .map(std::borrow::Cow::Owned)
                    .collect();
            }
            self.apply_sig_delta(seen, gone)?;
            let refs: Vec<&[u8]> = out.iter().map(|c| c.as_ref()).collect();
            write_blob_file(&self.path, &refs)
        })()
        .is_some();
        self.conn
            .execute_batch(if committed { "COMMIT" } else { "ROLLBACK" })
            .is_ok()
            && committed
    }

    /// Rows per shard — chunks decode in parallel, so one giant blob would
    /// serialize the whole decode on a single core.
    const SHARD_ROWS: usize = 16_384;

    /// Rebuild the rowid→length signature map after a full scan — a probe
    /// over the whole range costs ~nothing (blob lengths live in record
    /// headers), and without it the next miss would refetch the whole tail.
    fn set_sig(&self, seen: &[(i64, u32)]) -> bool {
        if self.conn.execute_batch("BEGIN IMMEDIATE").is_err() {
            return false;
        }
        let committed = (|| {
            self.conn
                .execute("DELETE FROM row_sig WHERE name=?1", [&self.name])
                .ok()?;
            let mut upsert = self
                .conn
                .prepare("INSERT OR REPLACE INTO row_sig (name, rid, len) VALUES (?1,?2,?3)")
                .ok()?;
            for (rid, len) in seen {
                upsert
                    .execute(rusqlite::params![self.name, rid, *len as i64])
                    .ok()?;
            }
            Some(())
        })()
        .is_some();
        self.conn
            .execute_batch(if committed { "COMMIT" } else { "ROLLBACK" })
            .is_ok()
            && committed
    }

    /// Store `rows` as the agent's complete cache state — fresh shards
    /// replacing every older chunk — atomically, with the same commit
    /// contract as `refresh`. File readers use this: any signature change
    /// means their whole scan just ran.
    fn replace(&self, rows: &[(i64, UsageEntry)]) -> bool {
        if self.conn.execute_batch("BEGIN IMMEDIATE").is_err() {
            return false;
        }
        let committed = shard_blobs(rows)
            .and_then(|chunks| {
                let refs: Vec<&[u8]> = chunks.iter().map(|c| c.as_slice()).collect();
                write_blob_file(&self.path, &refs)
            })
            .is_some();
        self.conn
            .execute_batch(if committed { "COMMIT" } else { "ROLLBACK" })
            .is_ok()
            && committed
    }

    /// All cached entries, in source-row order, tagged with `agent`.
    fn all(&self, agent: &'static str) -> Option<Vec<UsageEntry>> {
        let data = std::fs::read(&self.path).ok()?;
        let chunks = read_blob_file(&data)?;
        // Partitioned chunks (the steady state) decode straight to entries:
        // no rowids, no sort, no merge. Only the tail that the last rescan
        // overlapped takes the merge path — `k` is the first chunk index
        // whose range a later chunk reaches back into.
        let ranges: Option<Vec<(i64, i64)>> = chunks.iter().map(|c| chunk_range(c)).collect();
        let rs = ranges?;
        let mut min_later = i64::MAX;
        let mut k = rs.len();
        for i in (0..rs.len()).rev() {
            if rs[i].1 >= min_later {
                k = i;
            }
            min_later = min_later.min(rs[i].0);
        }
        let mut parts: Vec<Vec<UsageEntry>> = Vec::with_capacity(rs.len());
        let prefix: Option<Vec<Vec<UsageEntry>>> = chunks[..k]
            .par_iter()
            .map(|c| decode_chunk_entries(c, agent))
            .collect();
        parts.extend(prefix?);
        if k < rs.len() {
            // tail overlap — merge last-chunk-wins, then tag
            let mut merged = decode_merged(&chunks[k..])?;
            for (_, e) in &mut merged {
                e.agent = agent;
            }
            parts.push(merged.into_iter().map(|(_, e)| e).collect());
        }
        let mut out = Vec::with_capacity(parts.iter().map(|p| p.len()).sum());
        for part in parts {
            out.extend(part);
        }
        Some(out)
    }
}

/// Blob-file layout — one flat file per cache name, swapped atomically via
/// tmp+rename so readers never see a torn file:
///   u32 FILE_MAGIC | u32 n_chunks | n_chunks × u64 len | chunk bytes...
/// Chunks use the `CHUNK_MAGIC` record format and stay self-contained —
/// appends never re-encode earlier rows.
const FILE_MAGIC: u32 = 0x5245_424C; // "REBL"

/// Parse a blob file into its raw chunks, borrowed from the file buffer —
/// no copies. `None` for missing, short, or malformed files.
fn read_blob_file(data: &[u8]) -> Option<Vec<&[u8]>> {
    let u32_at = |o: usize| -> Option<u32> {
        Some(u32::from_le_bytes(data.get(o..o + 4)?.try_into().ok()?))
    };
    let u64_at = |o: usize| -> Option<u64> {
        Some(u64::from_le_bytes(data.get(o..o + 8)?.try_into().ok()?))
    };
    if u32_at(0)? != FILE_MAGIC {
        return None;
    }
    let n = u32_at(4)? as usize;
    let mut cur = 8 + n.checked_mul(8)?;
    data.get(8..cur)?;
    let mut chunks = Vec::with_capacity(n);
    for i in 0..n {
        let len = u64_at(8 + i * 8)? as usize;
        chunks.push(data.get(cur..cur.checked_add(len)?)?);
        cur += len;
    }
    (cur == data.len()).then_some(chunks)
}

/// Write all chunks as one tmp+rename — the rename is atomic, so a reader
/// sees either the old file or the new one, never a half-written mix.
fn write_blob_file(path: &Path, chunks: &[&[u8]]) -> Option<()> {
    let total: usize = chunks.iter().map(|c| c.len()).sum();
    let mut out = Vec::with_capacity(8 + 8 * chunks.len() + total);
    out.extend_from_slice(&FILE_MAGIC.to_le_bytes());
    out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
    for c in chunks {
        out.extend_from_slice(&(c.len() as u64).to_le_bytes());
    }
    for c in chunks {
        out.extend_from_slice(c);
    }
    let tmp = path.with_file_name(format!(
        "{}.tmp-{}",
        path.file_name()?.to_str()?,
        std::process::id()
    ));
    std::fs::write(&tmp, &out).ok()?;
    std::fs::rename(&tmp, path).ok()
}

/// Encode `rows` as SHARD_ROWS-sized chunks — an empty source still gets
/// one (empty) chunk, whose existence is what the hot path checks to prove
/// the cache covers the source.
fn shard_blobs(rows: &[(i64, UsageEntry)]) -> Option<Vec<Vec<u8>>> {
    let shards: Vec<&[(i64, UsageEntry)]> = if rows.is_empty() {
        vec![rows]
    } else {
        rows.chunks(EntryCache::SHARD_ROWS).collect()
    };
    shards.iter().map(|s| encode_chunk(s)).collect()
}

/// Decode all chunks in parallel and merge: seq order + a stable rowid sort
/// makes the LAST chunk win every duplicated rowid. One code path for both
/// partitioned shards (sort is a no-op) and tail-overlap appends.
fn decode_merged(chunks: &[&[u8]]) -> Option<Vec<(i64, UsageEntry)>> {
    let decoded: Option<Vec<Vec<(i64, UsageEntry)>>> =
        chunks.par_iter().map(|b| decode_chunk(b)).collect();
    let mut all: Vec<(i64, UsageEntry)> =
        Vec::with_capacity(decoded.as_ref()?.iter().map(|c| c.len()).sum());
    for c in decoded? {
        all.extend(c);
    }
    all.sort_by_key(|(rid, _)| *rid);
    let mut out: Vec<(i64, UsageEntry)> = Vec::with_capacity(all.len());
    for item in all {
        match out.last_mut() {
            Some(last) if last.0 == item.0 => *last = item,
            _ => out.push(item),
        }
    }
    Some(out)
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

/// Source-file signature for skip-if-unchanged: `mtime_ns:len` of the DB
/// plus its `-wal`/`-journal` sidecars (WAL writes don't move the main
/// file's mtime). `None` when the main file can't be stat'ed — never skip.
pub(super) fn src_sig(path: &Path) -> Option<String> {
    let one = |suffix: &str| {
        let mut p = path.as_os_str().to_os_string();
        p.push(suffix);
        std::fs::metadata(&p).and_then(|m| {
            m.modified().map(|t| {
                let ns = t
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                format!("{ns}:{}", m.len())
            })
        })
    };
    let main = one("").ok()?;
    let side = |s: &str| one(s).unwrap_or_else(|_| "-".into());
    Some(format!("{main}|{}|{}", side("-wal"), side("-journal")))
}

/// Read a rowid-monotonic source table through the incremental cache:
/// rows beyond `watermark - CACHE_TAIL` are re-scanned (the tail catches
/// rows whose usage fields fill in late), the cache is refreshed in one
/// transaction, and the full entry set comes back. When the source file's
/// signature hasn't moved since the last scan, the source isn't touched at
/// all — the cached blob is the answer.
///
/// `scan_range` runs per rowid chunk, possibly in parallel — it must open
/// its own connection.
/// Optional tail-rescan upgrade for sources whose payload column is a huge
/// blob: `probe` returns `(rowid, blob_len)` cheaply (SQLite reads blob
/// lengths from record headers, never decoding), `ids` fetches full rows by
/// rowid. Together they let a miss re-read only rows that actually changed.
type ProbeFn<'a> = dyn Fn(i64, i64) -> Vec<(i64, u32)> + Sync + 'a;
type ScanIdsFn<'a> = dyn Fn(&[i64]) -> Vec<(i64, UsageEntry)> + Sync + 'a;

struct TailProbe<'a> {
    probe: &'a ProbeFn<'a>,
    ids: &'a ScanIdsFn<'a>,
}

fn cached_scan(
    path: &Path,
    rowid_col: &str,
    table: &str,
    cache_name: &str,
    agent: &'static str,
    scan_range: &(dyn Fn(i64, i64) -> Vec<(i64, UsageEntry)> + Sync),
    tail_probe: Option<TailProbe<'_>>,
) -> Vec<UsageEntry> {
    let src = format!("{}:{table}", path.to_string_lossy());
    let sig = src_sig(path);
    let cache = EntryCache::open(cache_name);
    // hot path: same source, same file — the cache is complete by definition.
    if let (Some(c), Some(s)) = (&cache, &sig) {
        if c.source_sig().as_deref() == Some(s.as_str()) {
            match c.all(agent) {
                Some(all) => return all,
                // sig claims coverage but the blob is unreadable
                None => c.clear(),
            }
        }
    }
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
        // empty source — record an empty blob + sig so the hot path applies
        if let (Some(c), Some(s)) = (&cache, &sig) {
            if c.replace(&[]) {
                c.set_source_sig(s);
            }
        }
        return vec![];
    }
    let Some(cache) = cache else {
        return id_chunks(min, max)
            .par_iter()
            .flat_map(|&(lo, hi)| scan_range(lo, hi))
            .map(|(_, e)| e)
            .collect();
    };
    let mut wm = cache.watermark(&src).max(min - 1);
    // a positive watermark with an unreadable/missing blob is a broken
    // cache — wipe meta so the whole range gets re-scanned below
    if wm >= min && cache.blob_file_ok().is_none() {
        cache.clear();
        wm = min - 1;
    }
    let full_scan = |cache: &EntryCache| -> Vec<UsageEntry> {
        let mut fresh: Vec<(i64, UsageEntry)> = id_chunks(min, max)
            .par_iter()
            .flat_map(|&(a, b)| scan_range(a, b))
            .collect();
        fresh.sort_unstable_by_key(|(r, _)| *r);
        if cache.replace(&fresh) {
            if let Some(tp) = &tail_probe {
                // fill the signature map so the next miss is precise
                cache.set_sig(&(tp.probe)(min, max));
            }
            cache.set_watermark(max, &src);
            if let Some(s) = &sig {
                cache.set_source_sig(s);
            }
            return cache.all(agent).unwrap_or_default();
        }
        fresh.into_iter().map(|(_, e)| e).collect()
    };
    if max <= wm && tail_probe.is_none() {
        // the sig drifted but the rowid range didn't grow (e.g. a WAL
        // checkpoint rewrote the main file) — record the new sig
        if let Some(s) = &sig {
            cache.set_source_sig(s);
        }
        return cache.all(agent).unwrap_or_default();
    }
    if let Some(tp) = &tail_probe {
        // Probe the tail instead of re-reading it: blob lengths come from
        // record headers, so this diff is ~free even over thousands of rows.
        // A cold watermark takes the parallel range-scan instead.
        if wm < min {
            return full_scan(&cache);
        }
        let lo = (wm - CACHE_TAIL).max(min);
        let probed = (tp.probe)(lo, max);
        if probed.is_empty() {
            // probe failed — don't confuse it with "everything was deleted"
            let mut fresh: Vec<(i64, UsageEntry)> = id_chunks(lo, max)
                .par_iter()
                .flat_map(|&(a, b)| scan_range(a, b))
                .collect();
            fresh.sort_unstable_by_key(|(r, _)| *r);
            if cache.refresh(&fresh) {
                cache.set_watermark(max, &src);
                if let Some(s) = &sig {
                    cache.set_source_sig(s);
                }
            }
            return cache.all(agent).unwrap_or_default();
        }
        let probed_set: rustc_hash::FxHashMap<i64, u32> = probed.iter().copied().collect();
        let known = cache.sig_rows(lo);
        let mut gone: Vec<i64> = known
            .keys()
            .copied()
            .filter(|r| !probed_set.contains_key(r))
            .collect();
        gone.sort_unstable();
        // rows never seen or whose blob length changed → refetch; the last
        // few rows always refetch (in-flight writes can mutate in place
        // without changing length)
        let mut fetch: Vec<i64> = probed
            .iter()
            .filter(|(r, l)| known.get(r).copied() != Some(*l))
            .map(|(r, _)| *r)
            .collect();
        fetch.extend(probed.iter().rev().take(64).map(|(r, _)| *r));
        fetch.sort_unstable();
        fetch.dedup();
        if !fetch.is_empty() || !gone.is_empty() {
            let mut fresh = (tp.ids)(&fetch);
            fresh.sort_unstable_by_key(|(r, _)| *r);
            // fetched rows that now yield no entry (e.g. metrics removed)
            // are deletions too — the range-rescan used to drop them
            let got: rustc_hash::FxHashSet<i64> = fresh.iter().map(|(r, _)| *r).collect();
            gone.extend(fetch.iter().copied().filter(|r| !got.contains(r)));
            gone.sort_unstable();
            gone.dedup();
            // only advance coverage when the refresh commit succeeded —
            // same contract as the range-scan path
            if cache.refresh_precise(&fresh, &gone, &probed) {
                cache.set_watermark(max, &src);
                if let Some(s) = &sig {
                    cache.set_source_sig(s);
                }
            }
        } else {
            cache.set_watermark(max, &src);
            if let Some(s) = &sig {
                cache.set_source_sig(s);
            }
        }
        return match cache.all(agent) {
            Some(all) => all,
            None => {
                cache.clear();
                id_chunks(min, max)
                    .par_iter()
                    .flat_map(|&(lo, hi)| scan_range(lo, hi))
                    .map(|(_, e)| e)
                    .collect()
            }
        };
    }
    let lo = (wm - CACHE_TAIL).max(min);
    // sort before append so chunks (and therefore single-chunk reads) are
    // stored in rowid order
    let mut fresh: Vec<(i64, UsageEntry)> = id_chunks(lo, max)
        .par_iter()
        .flat_map(|&(a, b)| scan_range(a, b))
        .collect();
    fresh.sort_unstable_by_key(|(r, _)| *r);
    // advancing the watermark without a committed refresh would drop the
    // range permanently — only set it when the cache actually updated
    if cache.refresh(&fresh) {
        cache.set_watermark(max, &src);
        if let Some(s) = &sig {
            cache.set_source_sig(s);
        }
    }
    match cache.all(agent) {
        Some(all) => all,
        // file-level checks passed but a chunk still wouldn't decode —
        // fall back to the uncached full scan rather than return empty
        None => {
            cache.clear();
            id_chunks(min, max)
                .par_iter()
                .flat_map(|&(lo, hi)| scan_range(lo, hi))
                .map(|(_, e)| e)
                .collect()
        }
    }
}

/// Source-file signature for file readers: an FxHash over every source
/// file's (path, mtime_ns, len). `None` when any file can't be stat'ed —
/// never skip, never store. `files` must cover every file the scan could
/// read; order is normalized here so callers can pass the raw walk order.
pub(super) fn file_sig(files: &[PathBuf]) -> Option<String> {
    let mut sorted: Vec<&PathBuf> = files.iter().collect();
    sorted.sort();
    let metas: Option<Vec<(u64, u64)>> = sorted
        .par_iter()
        .map(|p| {
            let m = std::fs::metadata(p.as_path()).ok()?;
            let ns = m
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            Some((ns, m.len()))
        })
        .collect();
    let mut h = rustc_hash::FxHasher::default();
    for (p, (ns, len)) in sorted.iter().zip(metas?) {
        h.write(p.as_os_str().as_encoded_bytes());
        h.write_u64(ns);
        h.write_u64(len);
    }
    Some(format!("{:016x}:{}", h.finish(), files.len()))
}

/// Run `scan` behind the signature cache shared by file readers: a matching
/// `sig` returns the stored entries without touching a source file; a drift
/// (or an unreadable cache) runs the real scan and atomically replaces the
/// agent's chunks with the full fresh state. `sig=None` disables skipping —
/// the source state couldn't be fingerprinted — and skips the write too.
pub(super) fn sig_scan(
    cache_name: &str,
    agent: &'static str,
    sig: Option<String>,
    scan: impl FnOnce() -> Vec<UsageEntry>,
) -> Vec<UsageEntry> {
    let Some(cache) = EntryCache::open(cache_name) else {
        return scan();
    };
    if let Some(s) = &sig {
        if cache.source_sig().as_deref() == Some(s.as_str()) {
            match cache.all(agent) {
                Some(all) => return all,
                // sig claims coverage but the chunks are unreadable
                None => cache.clear(),
            }
        }
    }
    let entries = scan();
    let Some(s) = &sig else {
        return entries;
    };
    // the stored order index doubles as the synthetic rowid — reads return
    // entries in the same order the scan produced them
    let rows: Vec<(i64, UsageEntry)> = entries
        .into_iter()
        .enumerate()
        .map(|(i, e)| (i as i64, e))
        .collect();
    if cache.replace(&rows) {
        cache.set_source_sig(s);
    }
    rows.into_iter().map(|(_, e)| e).collect()
}

// ---------------------------------------------------------------------------
// OpenCode family
// ---------------------------------------------------------------------------

/// Which generation of the opencode message schema to read. `V2` stores usage
/// on `session_message` rows; `V1` stores it on `message` rows. Upstream reads
/// v1 first, then v2, then `storage/message/**/*.json` files, deduplicated by
/// message id in that order; compaction-type rows never produce entries —
/// their usage only surfaces through `session_v2`/`session` aggregates, which
/// fill sessions that have no message entries at all.
#[derive(Clone, Copy, PartialEq)]
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

/// OpenCode's `~/.local/share/opencode/opencode.db`. Model and provider are
/// both required for an entry: v2 reads `model.id`/`model.providerID` falling
/// back to the top-level `modelID`/`providerID`; v1 and message files read the
/// top-level fields only.
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
        self.path.is_file() || self.opencode_dir().join("storage/message").is_dir()
    }
    fn cache_key(&self) -> Option<String> {
        Some(format!("{}|{}", src_sig(&self.path).unwrap_or_default(), self.files_sig()))
    }

    fn entries(&self) -> Vec<UsageEntry> {
        let (mut v1, v2) = rayon::join(
            || self.entries_table(OcSchema::V1),
            || self.entries_table(OcSchema::V2),
        );
        v1.extend(v2);
        v1.extend(self.entries_files(&v1));
        v1.extend(self.entries_aggregates(&v1));
        v1
    }
}

impl OpenCodeDb {
    fn opencode_dir(&self) -> &Path {
        self.path.parent().unwrap_or(self.path.as_path())
    }

    /// Manifest over `storage/message/**/*.json` for cache invalidation —
    /// count + (mtime, len) of each file, no contents.
    fn files_sig(&self) -> String {
        let mut files = Vec::new();
        super::walk(
            &self.opencode_dir().join("storage/message"),
            &|p| p.extension().is_some_and(|e| e == "json"),
            &mut files,
            8,
        );
        file_sig(&files).unwrap_or_default()
    }

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
            None,
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
        // One extraction order for both generations; the decoder coalesces the
        // model/provider spellings per generation.
        let json = |d: &str| {
            format!(
                "json_extract({d}, \
                 '$.tokens.input', '$.tokens.output', '$.tokens.reasoning', \
                 '$.tokens.cache.read', '$.tokens.cache.write', '$.tokens.total', \
                 '$.model.id', '$.model.modelID', '$.modelID', \
                 '$.model.providerID', '$.providerID', \
                 '$.time.created', '$.cost')"
            )
        };
        let sql = match schema {
            // upstream counts assistant rows only — compaction usage is
            // reported through the session aggregates instead
            OcSchema::V2 => format!(
                "SELECT m.rowid, m.id, m.session_id, m.time_created, {} \
                 FROM session_message m \
                 WHERE m.rowid >= ?1 AND m.rowid <= ?2 \
                   AND m.type = 'assistant' \
                   AND json_extract(m.data, '$.tokens') IS NOT NULL \
                   AND COALESCE(json_extract(m.data,'$.model.id'), \
                                json_extract(m.data,'$.model.modelID'), \
                                json_extract(m.data,'$.modelID')) IS NOT NULL \
                   AND COALESCE(json_extract(m.data,'$.model.providerID'), \
                                json_extract(m.data,'$.providerID')) IS NOT NULL",
                json("m.data")
            ),
            OcSchema::V1 => format!(
                "SELECT rowid, id, session_id, time_created, {} \
                 FROM message \
                 WHERE rowid >= ?1 AND rowid <= ?2 \
                   AND json_extract(data, '$.tokens') IS NOT NULL \
                   AND json_extract(data, '$.modelID') IS NOT NULL \
                   AND json_extract(data, '$.modelID') != '' \
                   AND json_extract(data, '$.providerID') IS NOT NULL \
                   AND json_extract(data, '$.providerID') != ''",
                json("data")
            ),
        };
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let rows = stmt
            .query_map([lo, hi], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })
            .map(|r| r.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut out = Vec::with_capacity(rows.len());
        for (rowid, id, session, created, arr) in rows {
            let Ok(d) = serde_json::from_str::<serde_json::Value>(&arr) else {
                continue;
            };
            let (model, provider) = match schema {
                OcSchema::V2 => (
                    d[6].as_str()
                        .or_else(|| d[7].as_str())
                        .or_else(|| d[8].as_str()),
                    d[9].as_str().or_else(|| d[10].as_str()),
                ),
                OcSchema::V1 => (d[8].as_str(), d[10].as_str()),
            };
            let (Some(model), Some(provider)) = (model, provider) else {
                continue;
            };
            if model.is_empty() || provider.is_empty() {
                continue;
            }
            let mut tokens = Tokens {
                input: d[0].as_u64().unwrap_or(0),
                output: d[1].as_u64().unwrap_or(0),
                reasoning: d[2].as_u64().unwrap_or(0),
                cache_creation: d[4].as_u64().unwrap_or(0),
                cache_read: d[3].as_u64().unwrap_or(0),
            };
            fold_total_into_extra(&mut tokens, d[5].as_u64().unwrap_or(0));
            if tokens.is_zero() {
                continue;
            }
            // v2 falls back to the row's time_created only when the payload
            // lacks time.created; v1 has no column fallback. A nonpositive
            // created means epoch, matching upstream's >0 filter.
            let ts = match schema {
                OcSchema::V2 => d[11].as_i64().or(created).unwrap_or(0).max(0),
                OcSchema::V1 => d[11].as_i64().unwrap_or(0).max(0),
            };
            out.push((
                rowid,
                UsageEntry {
                    agent,
                    timestamp_ms: ts,
                    session_id: session.into(),
                    model: model.into(),
                    provider: provider.into(),
                    tokens,
                    cost_usd: d[12].as_f64(),
                    dedup_key: id.into(),
                    dedup_id: 0,
                total_override: None,
                is_fallback: false,
                    project: agent.into(),
                },
            ));
        }
        out
    }

    /// Legacy `storage/message/<session>/<id>.json` files. Upstream skips a
    /// file whose stem is already a known message id; survivors dedup on the
    /// payload's `id` downstream like every other entry.
    fn entries_files(&self, prior: &[UsageEntry]) -> Vec<UsageEntry> {
        let dir = self.opencode_dir().join("storage/message");
        if !dir.is_dir() {
            return vec![];
        }
        let seen: rustc_hash::FxHashSet<&str> =
            prior.iter().map(|e| e.dedup_key.as_ref()).collect();
        let mut files = Vec::new();
        super::walk(
            &dir,
            &|p| p.extension().is_some_and(|e| e == "json"),
            &mut files,
            8,
        );
        files.retain(|f| {
            f.file_stem()
                .and_then(|s| s.to_str())
                .is_none_or(|s| !seen.contains(s))
        });
        let id = self.id;
        files
            .par_iter()
            .filter_map(|f| Self::read_message_file(f, id))
            .collect()
    }

    fn read_message_file(path: &Path, agent: &'static str) -> Option<UsageEntry> {
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        let t = v.get("tokens")?;
        let model = v.get("modelID").and_then(|m| m.as_str())?;
        let provider = v.get("providerID").and_then(|m| m.as_str())?;
        if model.is_empty() || provider.is_empty() {
            return None;
        }
        let mut tokens = Tokens {
            input: num(t, "input"),
            output: num(t, "output"),
            reasoning: num(t, "reasoning"),
            cache_creation: t
                .get("cache")
                .and_then(|c| c.get("write"))
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
            cache_read: t
                .get("cache")
                .and_then(|c| c.get("read"))
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
        };
        fold_total_into_extra(
            &mut tokens,
            t.get("total").and_then(|x| x.as_u64()).unwrap_or(0),
        );
        if tokens.is_zero() {
            return None;
        }
        Some(UsageEntry {
            agent,
            timestamp_ms: v
                .get("time")
                .and_then(|t| t.get("created"))
                .and_then(|c| c.as_i64())
                .unwrap_or(0)
                .max(0),
            session_id: v
                .get("sessionID")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown")
                .into(),
            model: model.into(),
            provider: provider.into(),
            tokens,
            cost_usd: v.get("cost").and_then(|c| c.as_f64()),
            dedup_key: v.get("id").and_then(|s| s.as_str()).unwrap_or("").into(),
            dedup_id: 0,
                total_override: None,
                is_fallback: false,
            project: agent.into(),
        })
    }

    /// `session_v2`/`session` counters for sessions that produced no message
    /// entries — the only place compaction usage surfaces. Entries carry the
    /// upstream `session:<id>` key so reports can scope them to session rows.
    fn entries_aggregates(&self, prior: &[UsageEntry]) -> Vec<UsageEntry> {
        let covered: rustc_hash::FxHashSet<&str> =
            prior.iter().map(|e| e.session_id.as_ref()).collect();
        let Some(conn) = open_db(&self.path) else {
            return vec![];
        };
        let has_v2_msgs = table_exists(&conn, "session_message");
        let mut out = Vec::new();
        for table in ["session_v2", "session"] {
            if table == "session" && !has_v2_msgs {
                continue;
            }
            if !table_exists(&conn, table) {
                continue;
            }
            let sql = format!(
                "SELECT id, time_created, cost, tokens_input, tokens_output, \
                        tokens_cache_read, tokens_cache_write, \
                        COALESCE(tokens_reasoning, 0), model FROM {table}"
            );
            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<f64>>(1)?,
                        r.get::<_, Option<f64>>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, Option<i64>>(5)?,
                        r.get::<_, Option<i64>>(6)?,
                        r.get::<_, Option<i64>>(7)?,
                        r.get::<_, Option<String>>(8)?,
                    ))
                })
                .map(|r| r.flatten().collect::<Vec<_>>())
                .unwrap_or_default();
            for (sid, created, cost, i, o, cr, cw, re, model_raw) in rows {
                if covered.contains(sid.as_str()) {
                    continue;
                }
                let Some(created) = created.filter(|c| c.is_finite()) else {
                    continue;
                };
                let (model, provider) = parse_session_model(model_raw.as_deref());
                let tokens = Tokens {
                    input: i.unwrap_or(0).max(0) as u64,
                    output: o.unwrap_or(0).max(0) as u64,
                    reasoning: re.unwrap_or(0).max(0) as u64,
                    cache_creation: cw.unwrap_or(0).max(0) as u64,
                    cache_read: cr.unwrap_or(0).max(0) as u64,
                };
                if tokens.is_zero() && cost.unwrap_or(0.0) <= 0.0 {
                    continue;
                }
                out.push(UsageEntry {
                    agent: self.id,
                    timestamp_ms: created.max(0.0) as i64,
                    session_id: sid.clone().into(),
                    model: model.into(),
                    provider: provider.into(),
                    tokens,
                    cost_usd: cost,
                    dedup_key: format!("session:{sid}").into(),
                    dedup_id: 0,
                    total_override: None,
                    is_fallback: false,
                    project: self.id.into(),
                });
            }
        }
        out
    }
}

/// `session.model` is either a JSON object (`{"id":..,"providerID":..}`), a
/// bare string, or absent — upstream's `parse_session_model`.
fn parse_session_model(value: Option<&str>) -> (String, String) {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return ("unknown".into(), "unknown".into());
    };
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(value) {
        if let Some(m) = v.as_str().map(str::trim).filter(|m| !m.is_empty()) {
            return (m.to_string(), "unknown".into());
        }
        if let Some(o) = v.as_object() {
            let model = o
                .get("id")
                .and_then(|x| x.as_str())
                .or_else(|| o.get("modelID").and_then(|x| x.as_str()))
                .map(str::trim)
                .filter(|m| !m.is_empty());
            let provider = o
                .get("providerID")
                .and_then(|x| x.as_str())
                .or_else(|| o.get("provider").and_then(|x| x.as_str()))
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .unwrap_or("unknown");
            if let Some(model) = model {
                return (model.to_string(), provider.to_string());
            }
        }
        return ("unknown".into(), "unknown".into());
    }
    (value.to_string(), "unknown".into())
}

/// Upstream `apply_total_token_fallback`: `reasoning` is the recorded extra;
/// a `total` claiming more than every bucket adds the gap to output when
/// output is empty, else to the extra (reasoning) bucket.
fn fold_total_into_extra(tokens: &mut Tokens, total: u64) {
    let missing = total.saturating_sub(tokens.total());
    if missing == 0 {
        return;
    }
    if tokens.output == 0 {
        tokens.output = missing;
    } else {
        tokens.reasoning += missing;
    }
}

fn table_exists(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.prepare(&format!("SELECT 1 FROM {table} LIMIT 0")).is_ok()
}

/// Kilo (`kilo.db`): an opencode fork that keeps the v1 `message` schema.
/// Upstream reads only `message`, requires tokens + `modelID` + a usable
/// `time.created` (seconds are normalized to ms), and treats `providerID`,
/// `id`, and `sessionID` as optional.
pub struct KiloDb {
    pub id: &'static str,
    pub display: &'static str,
    pub path: PathBuf,
}

impl AgentReader for KiloDb {
    fn id(&self) -> &'static str {
        self.id
    }
    fn display(&self) -> &'static str {
        self.display
    }
    fn detect(&self) -> bool {
        self.path.is_file()
    }
    fn cache_key(&self) -> Option<String> {
        src_sig(&self.path)
    }
    fn entries(&self) -> Vec<UsageEntry> {
        let path = self.path.clone();
        let id = self.id;
        cached_scan(
            &self.path,
            "rowid",
            "message",
            self.id,
            id,
            &move |lo, hi| Self::scan_range(&path, id, lo, hi),
            None,
        )
    }
}

impl KiloDb {
    fn scan_range(path: &Path, agent: &'static str, lo: i64, hi: i64) -> Vec<(i64, UsageEntry)> {
        let Some(conn) = open_db(path) else {
            return vec![];
        };
        let mut stmt = match conn.prepare(
            "SELECT rowid, id, session_id, json_extract(data, \
                '$.tokens.input', '$.tokens.output', '$.tokens.reasoning', \
                '$.tokens.cache.read', '$.tokens.cache.write', '$.tokens.total', \
                '$.modelID', '$.providerID', '$.time.created', '$.cost', \
                '$.id', '$.sessionID') \
             FROM message \
             WHERE rowid >= ?1 AND rowid <= ?2 \
               AND json_extract(data, '$.tokens') IS NOT NULL \
               AND json_extract(data, '$.modelID') IS NOT NULL \
               AND json_extract(data, '$.modelID') != ''",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let rows = stmt
            .query_map([lo, hi], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map(|r| r.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut out = Vec::with_capacity(rows.len());
        for (rowid, id, session, arr) in rows {
            let Ok(d) = serde_json::from_str::<serde_json::Value>(&arr) else {
                continue;
            };
            let Some(model) = d[6].as_str().filter(|m| !m.is_empty()) else {
                continue;
            };
            // upstream requires a payload timestamp and normalizes seconds
            let Some(created) = d[8].as_i64().filter(|c| *c > 0) else {
                continue;
            };
            let ts = if created < 1_000_000_000_000 {
                created.saturating_mul(1000)
            } else {
                created
            };
            let mut tokens = Tokens {
                input: d[0].as_u64().unwrap_or(0),
                output: d[1].as_u64().unwrap_or(0),
                reasoning: d[2].as_u64().unwrap_or(0),
                cache_creation: d[4].as_u64().unwrap_or(0),
                cache_read: d[3].as_u64().unwrap_or(0),
            };
            fold_total_into_extra(&mut tokens, d[5].as_u64().unwrap_or(0));
            if tokens.is_zero() {
                continue;
            }
            out.push((
                rowid,
                UsageEntry {
                    agent,
                    timestamp_ms: ts,
                    session_id: d[11].as_str().unwrap_or(session.as_str()).into(),
                    model: model.into(),
                    provider: d[7].as_str().unwrap_or("").into(),
                    tokens,
                    cost_usd: d[9].as_f64(),
                    dedup_key: d[10]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("{}:{id}", path.display()))
                        .into(),
                    dedup_id: 0,
                    total_override: None,
                    is_fallback: false,
                    project: agent.into(),
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
    fn cache_key(&self) -> Option<String> {
        src_sig(&devin_db())
    }

    fn entries(&self) -> Vec<UsageEntry> {
        let path = devin_db();
        let scan_path = path.clone();
        let probe_path = path.clone();
        let ids_path = path.clone();
        cached_scan(
            &path,
            "row_id",
            "message_nodes",
            "devin",
            "devin",
            &move |lo, hi| Self::scan_range(&scan_path, lo, hi),
            Some(TailProbe {
                probe: &move |lo, hi| Self::probe_range(&probe_path, lo, hi),
                ids: &move |ids| Self::scan_ids(&ids_path, ids),
            }),
        )
    }
}

type DevinRow = (i64, String, i64, String, Option<String>, Option<String>);

impl Devin {
    const SELECT: &'static str = "SELECT n.row_id, n.session_id, n.created_at, \
            json_extract(n.chat_message, \
                '$.metadata.metrics', '$.metadata.request_id', \
                '$.metadata.created_at', '$.metadata.cost'), \
            s.model, s.working_directory \
        FROM message_nodes n JOIN sessions s ON s.id = n.session_id";

    fn rows(conn: &rusqlite::Connection, sql: &str, params: &[i64]) -> Vec<DevinRow> {
        let Ok(mut stmt) = conn.prepare(sql) else {
            return vec![];
        };
        stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })
        .map(|r| r.flatten().collect())
        .unwrap_or_default()
    }

    /// `(row_id, blob_len)` — SQLite resolves `length()` from record
    /// headers, so this never decodes the ~25KB payloads.
    fn probe_range(path: &Path, lo: i64, hi: i64) -> Vec<(i64, u32)> {
        let Some(conn) = open_db(path) else {
            return vec![];
        };
        conn.prepare(
            "SELECT row_id, length(chat_message) FROM message_nodes \
             WHERE row_id >= ?1 AND row_id <= ?2",
        )
        .ok()
        .and_then(|mut s| {
            s.query_map([lo, hi], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u32))
            })
            .ok()
            .map(|it| it.flatten().collect())
        })
        .unwrap_or_default()
    }

    /// Re-fetch specific rows — used by the probe-driven rescan, which
    /// computes the exact set of new/changed rowids itself.
    fn scan_ids(path: &Path, ids: &[i64]) -> Vec<(i64, UsageEntry)> {
        let Some(conn) = open_db(path) else {
            return vec![];
        };
        let mut out = Vec::new();
        for chunk in ids.chunks(500) {
            let ph = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "{} WHERE n.row_id IN ({ph}) \
                 AND instr(n.chat_message, '\"input_tokens\"') > 0",
                Self::SELECT
            );
            out.extend(Self::rows_to_entries(Self::rows(&conn, &sql, chunk)));
        }
        out
    }

    fn scan_range(path: &Path, lo: i64, hi: i64) -> Vec<(i64, UsageEntry)> {
        let Some(conn) = open_db(path) else {
            return vec![];
        };
        let sql = format!(
            "{} WHERE n.row_id >= ?1 AND n.row_id <= ?2 \
             AND instr(n.chat_message, '\"input_tokens\"') > 0",
            Self::SELECT
        );
        Self::rows_to_entries(Self::rows(&conn, &sql, &[lo, hi]))
    }

    fn rows_to_entries(rows: Vec<DevinRow>) -> Vec<(i64, UsageEntry)> {
        let mut out = Vec::with_capacity(rows.len());
        for (row_id, session, created, arr, model, cwd) in rows {
            let Ok(d) = serde_json::from_str::<serde_json::Value>(&arr) else {
                continue;
            };
            let met = &d[0];
            // instr() prefilter can overmatch — keep the original
            // "metrics.input_tokens IS NOT NULL" semantics exactly
            if !met.get("input_tokens").is_some_and(|v| !v.is_null()) {
                continue;
            }
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
                    session_id: session.into(),
                    model: model.unwrap_or_else(|| UNKNOWN_MODEL.into()).into(),
                    provider: "".into(),
                    tokens,
                    cost_usd: cost,
                    dedup_key: d[1]
                        .as_str()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| row_id.to_string())
                        .into(),
                    dedup_id: 0,
                total_override: None,
                is_fallback: false,
                    project: cwd.unwrap_or_default().into(),
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
    fn cache_key(&self) -> Option<String> {
        src_sig(&zcode_db())
    }
    fn entries(&self) -> Vec<UsageEntry> {
        sig_scan("zcode", "zcode", src_sig(&zcode_db()), || self.scan())
    }
}

impl ZCode {
    fn scan(&self) -> Vec<UsageEntry> {
        let Some(conn) = open_db(&zcode_db()) else {
            return vec![];
        };
        // upstream: completed rows only, joined to session for the project dir
        let mut stmt = match conn.prepare(
            "SELECT m.id, m.session_id, m.started_at, m.model_id, m.provider_id, \
                    m.input_tokens, m.output_tokens, \
                    m.cache_creation_input_tokens, m.cache_read_input_tokens, \
                    m.computed_total_tokens, s.directory \
             FROM model_usage m LEFT JOIN session s ON s.id = m.session_id \
             WHERE m.status = 'completed'",
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
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, i64>(9)?,
                    r.get::<_, Option<String>>(10)?,
                ))
            })
            .map(|r| r.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        rows.into_iter()
            .filter(|(id, session, started, model, ..)| {
                !id.is_empty() && !session.is_empty() && *started > 0 && !model.is_empty()
            })
            .map(
                |(id, session, started, model, provider, i, o, cc, cr, total, directory)| {
                    // zcode input_tokens include cache reads + writes; upstream
                    // caps each at what input actually contains
                    let cr = cr.clamp(0, i);
                    let cc = cc.clamp(0, i - cr);
                    let input = (i - cr - cc).max(0) as u64;
                    let mut output = o.max(0) as u64;
                    // extra total: tokens the record's computed total accounts
                    // for beyond the reported buckets (upstream folds a gap
                    // into output when output is 0, else counts it as extra)
                    let mut extra = 0u64;
                    let missing = (total.max(0) as u64)
                        .saturating_sub(input + output + cc.max(0) as u64 + cr.max(0) as u64);
                    if missing > 0 {
                        if output == 0 {
                            output = missing;
                        } else {
                            extra = missing;
                        }
                    }
                    UsageEntry {
                        agent: "zcode",
                        timestamp_ms: started,
                        session_id: session.into(),
                        model: model.into(),
                        provider: provider.unwrap_or_default().into(),
                        tokens: Tokens {
                            input,
                            output,
                            reasoning: extra,
                            cache_creation: cc.max(0) as u64,
                            cache_read: cr.max(0) as u64,
                        },
                        cost_usd: None,
                        dedup_key: id.into(),
                        dedup_id: 0,
                        total_override: None,
                        is_fallback: false,
                        project: directory.unwrap_or_default().into(),
                    }
                },
            )
            .filter(|e| e.tokens.total() > 0)
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
    fn cache_key(&self) -> Option<String> {
        src_sig(&zed_db())
    }
    fn entries(&self) -> Vec<UsageEntry> {
        let path = zed_db();
        let sig = src_sig(&path);
        sig_scan("zed", "zed", sig, move || Self::scan(&path))
    }
}

impl Zed {
    fn scan(path: &Path) -> Vec<UsageEntry> {
        let Some(conn) = open_db(path) else {
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
                        session_id: session.to_string().into(),
                        model: map
                            .get("model")
                            .and_then(|m| m.as_str())
                            .unwrap_or(UNKNOWN_MODEL)
                            .into(),
                        provider: "".into(),
                        tokens,
                        cost_usd: None,
                        dedup_key: format!("{session}:{}", out.len()).into(),
                        dedup_id: 0,
                        total_override: None,
                        is_fallback: false,
                        project: "".into(),
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
    fn cache_key(&self) -> Option<String> {
        src_sig(&cursor_db())
    }
    fn entries(&self) -> Vec<UsageEntry> {
        let path = cursor_db();
        let sig = src_sig(&path);
        sig_scan("cursor", "cursor", sig, move || Self::scan(&path))
    }
}

impl Cursor {
    fn scan(path: &Path) -> Vec<UsageEntry> {
        let Some(conn) = open_db(path) else {
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
                    session_id: gstr(csess).into(),
                    model: gstr(cmodel).into(),
                    provider: "".into(),
                    tokens,
                    cost_usd: None,
                    dedup_key: gstr(cid).into(),
                    dedup_id: 0,
                total_override: None,
                is_fallback: false,
                    project: "".into(),
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(tag: &str) -> UsageEntry {
        UsageEntry {
            agent: "x",
            timestamp_ms: 0,
            session_id: "s".into(),
            model: "m".into(),
            provider: "".into(),
            tokens: Tokens::default(),
            cost_usd: None,
            dedup_key: tag.into(),
            dedup_id: 0,
                total_override: None,
                is_fallback: false,
            project: "".into(),
        }
    }

    #[test]
    fn chunk_roundtrip_preserves_fields_and_hashes_keys() {
        let mut e = entry("k1");
        e.tokens.input = 7;
        e.cost_usd = Some(1.5);
        e.project = "proj".into();
        let blob = encode_chunk(&[(42, e)]).unwrap();
        let rows = decode_chunk(&blob).unwrap();
        assert_eq!(rows.len(), 1);
        let (rid, e) = &rows[0];
        assert_eq!(*rid, 42);
        assert_eq!(e.tokens.input, 7);
        assert_eq!(e.cost_usd, Some(1.5));
        assert_eq!(&*e.project, "proj");
        // the key string is gone; its agent:key hash survives instead
        assert_eq!(e.dedup_id, dedup_hash("x", b"k1"));
        assert!(decode_chunk(&blob[..blob.len() - 3]).is_none());
        assert!(decode_chunk(b"junk").is_none());
    }

    #[test]
    fn file_sig_is_order_insensitive_and_tracks_content() {
        let dir = std::env::temp_dir().join(format!("rsusage-sig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.jsonl");
        let b = dir.join("b.jsonl");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"yy").unwrap();
        assert_eq!(
            file_sig(&[a.clone(), b.clone()]),
            file_sig(&[b.clone(), a.clone()])
        );
        std::fs::write(&a, b"xyz").unwrap(); // len change -> new sig
        assert_ne!(
            file_sig(&[a.clone(), b.clone()]),
            file_sig(&[a.clone()]) // different file set -> different sig
        );
        let missing = dir.join("gone.jsonl");
        assert_eq!(file_sig(&[a.clone(), missing]), None); // unstatable -> None
        std::fs::remove_dir_all(&dir).ok();
    }

    fn tmp_cache(tag: &str) -> (std::path::PathBuf, EntryCache) {
        let dir = std::env::temp_dir().join(format!("rsusage-prec-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = rusqlite::Connection::open(dir.join("meta.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE row_sig (name TEXT, rid INTEGER, len INTEGER, \
             PRIMARY KEY(name,rid));",
        )
        .unwrap();
        (
            dir.clone(),
            EntryCache {
                conn,
                name: "t".into(),
                path: dir.join("t.bin"),
            },
        )
    }

    fn e_tag(rid: i64, tag: &str) -> (i64, UsageEntry) {
        let mut e = entry(tag);
        e.tokens.input = rid as u64;
        (rid, e)
    }

    #[test]
    fn truncate_chunk_keeps_prefix_rows() {
        let rows: Vec<_> = (0..10).map(|r| e_tag(r, "a")).collect();
        let blob = encode_chunk(&rows).unwrap();
        let t = truncate_chunk(&blob, 5).unwrap();
        let back = decode_chunk(&t).unwrap();
        assert_eq!(
            back.iter().map(|(r, _)| *r).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert_eq!(back[4].1.tokens.input, 4);
        assert!(truncate_chunk(&blob, 0).unwrap().is_empty());
    }

    #[test]
    fn refresh_precise_updates_gaps_appends_and_deletes() {
        let (dir, cache) = tmp_cache("upd");
        // chunks [0..9] and [20..29] — a real gap between them
        let a: Vec<_> = (0..10).map(|r| e_tag(r, "a")).collect();
        let b: Vec<_> = (20..30).map(|r| e_tag(r, "b")).collect();
        let ca = encode_chunk(&a).unwrap();
        let cb = encode_chunk(&b).unwrap();
        write_blob_file(&cache.path, &[&ca, &cb]).unwrap();

        let fresh = vec![e_tag(5, "new"), e_tag(15, "gap"), e_tag(40, "tail")];
        let seen: Vec<(i64, u32)> = (0..50).map(|r| (r, 7)).filter(|(r, _)| *r != 3).collect();
        assert!(cache.refresh_precise(&fresh, &[3], &seen));

        let all = cache.all("x").unwrap();
        // row3 gone, row5 replaced, 15 in chunk B, 40 appended; total 9+11+1
        assert_eq!(all.len(), 9 + 11 + 1);
        assert!(all.iter().all(|e| e.agent == "x"));
        // order: chunk A rows, then B (+15 first), then tail
        let tags: Vec<String> = all.iter().map(|e| e.dedup_key.to_string()).collect();
        assert_eq!(tags.iter().filter(|t| t.as_str() == "new").count(), 0); // keys not stored
        let inputs: Vec<u64> = all.iter().map(|e| e.tokens.input).collect();
        assert_eq!(
            inputs,
            [0, 1, 2, 4, 5, 6, 7, 8, 9, 15, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 40]
        );
        // sig: rows 0..=49 minus 3
        let sig = cache.sig_rows(0);
        assert_eq!(sig.len(), 49);
        assert!(!sig.contains_key(&3));
        std::fs::remove_dir_all(&dir).ok();
    }
}
