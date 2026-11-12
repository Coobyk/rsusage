mod jsonl;
mod sqlite;

use crate::paths;
use crate::types::UsageEntry;
use rayon::prelude::*;
use std::collections::HashSet;
use std::hash::{BuildHasherDefault, Hasher};
use std::path::{Path, PathBuf};

/// A usage data source for one coding agent.
pub trait AgentReader: Send + Sync {
    fn id(&self) -> &'static str;
    fn display(&self) -> &'static str;
    /// Data directory/file exists on this machine.
    fn detect(&self) -> bool;
    /// All usage entries, unfiltered. Caller dedups globally.
    fn entries(&self) -> Vec<UsageEntry>;
    /// A signature covering everything `entries` could read — the same
    /// stats the entry cache trusts. `None` means the source can't be
    /// signed, so output can't be memoized.
    fn cache_key(&self) -> Option<String> {
        None
    }
}

/// Recursively collect files matching a predicate.
pub fn walk(dir: &Path, pred: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>, depth: u8) {
    if depth == 0 || !dir.is_dir() {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else {
            continue;
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

/// Parse an ISO-8601/RFC3339 timestamp or epoch seconds/millis/micros/nanos.
pub fn parse_ts(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::String(s) => {
            if let Ok(t) = s.trim().parse::<jiff::Timestamp>() {
                Some(t.as_millisecond())
            } else {
                s.trim().parse::<i64>().ok().map(epoch_to_ms)
            }
        }
        serde_json::Value::Number(n) => n.as_i64().map(epoch_to_ms),
        _ => None,
    }
}

pub fn epoch_to_ms(n: i64) -> i64 {
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

fn num(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

fn fnum(v: &serde_json::Value, key: &str) -> Option<f64> {
    v.get(key).and_then(|x| x.as_f64())
}

fn strv<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// FxHash (rustc's old hasher): rotate-xor-multiply per 8-byte chunk.
/// SipHash is overkill for in-process dedup keys.
#[derive(Default)]
struct FxHasher(u64);

impl Hasher for FxHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for c in bytes.chunks(8) {
            let mut b = [0u8; 8];
            b[..c.len()].copy_from_slice(c);
            self.write_u64(u64::from_le_bytes(b));
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x517cc1b727220a95);
    }
    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
}

/// Hash the dedup identity — the same logical key as `format!("{agent}:{key}")`
/// (lengths mixed in so field boundaries can't shift), as a u64.
/// `nz` remaps the (2^-64) 0 result to 1 so 0 can mean "no precomputed id".
pub(crate) fn dedup_hash(agent: &str, key: &[u8]) -> u64 {
    let mut h = FxHasher::default();
    h.write_usize(agent.len());
    h.write(agent.as_bytes());
    h.write_usize(key.len());
    h.write(key);
    match h.finish() {
        0 => 1,
        h => h,
    }
}

/// Globally dedup entries by `agent:dedup_key`. Empty keys are always unique.
/// Hashing the string keys is the bulk of the work, so that runs in
/// parallel; the u64 set pass below is identical to the old sequential one —
/// same first-occurrence-wins, same output order.
pub fn dedup(entries: Vec<UsageEntry>) -> Vec<UsageEntry> {
    let hashes: Vec<u64> = entries
        .par_iter()
        .enumerate()
        .map(|(i, e)| {
            // cached entries carry their hash; only fresh scans need the
            // string hashed — and "empty key" can only ever come from them
            if e.dedup_id != 0 {
                e.dedup_id
            } else if e.dedup_key.is_empty() {
                // index-keyed — unique per entry, same as the old `anon`
                // counter (values differ, uniqueness is what matters)
                dedup_hash(e.agent, format!("anon:{i}").as_bytes())
            } else {
                dedup_hash(e.agent, e.dedup_key.as_bytes())
            }
        })
        .collect();
    let mut seen: HashSet<u64, BuildHasherDefault<FxHasher>> =
        HashSet::with_capacity_and_hasher(hashes.len(), Default::default());
    let mut out = Vec::with_capacity(hashes.len());
    for (e, h) in entries.into_iter().zip(hashes) {
        if seen.insert(h) {
            out.push(e);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Registry — agent identity lives here, once. `by_id`/`agent_ids`/
// `display_for`/`legacy_json` all read these tables without constructing
// readers.
// ---------------------------------------------------------------------------

/// (id, display, legacy_json_schema, make)
struct Bespoke(
    &'static str,
    &'static str,
    bool,
    fn() -> Box<dyn AgentReader>,
);

/// Agents with bespoke readers. `legacy_json` marks agents whose agent-scoped
/// JSON uses upstream's codex-era schema (`costUSD`, `models` map).
const BESPOKE: &[Bespoke] = &[
    Bespoke("claude", "Claude", false, || Box::new(jsonl::Claude)),
    Bespoke("codex", "Codex", true, || Box::new(jsonl::Codex)),
    Bespoke("opencode", "OpenCode", false, || {
        Box::new(sqlite::OpenCodeDb {
            id: "opencode",
            display: "OpenCode",
            path: paths::xdg_data().join("opencode/opencode.db"),
        })
    }),
    Bespoke("kilo", "Kilo", false, || {
        Box::new(sqlite::KiloDb {
            id: "kilo",
            display: "Kilo",
            path: paths::xdg_data().join("kilo/kilo.db"),
        })
    }),
    Bespoke("zcode", "ZCode", false, || Box::new(sqlite::ZCode)),
    Bespoke("devin", "Devin", false, || Box::new(sqlite::Devin)),
    Bespoke("grok", "Grok", false, || Box::new(jsonl::Grok)),
    Bespoke("zed", "Zed", false, || Box::new(sqlite::Zed)),
    Bespoke("cursor", "Cursor", false, || Box::new(sqlite::Cursor)),
    Bespoke("gemini", "Gemini", false, || Box::new(jsonl::Gemini)),
];

/// Where a generic-reader directory lives.
enum Base {
    Home,
    Data,
    Config,
}

/// (id, display, [(base, subdir)])
type AgentSpec = (&'static str, &'static str, &'static [(Base, &'static str)]);

/// Agents that need no bespoke reader: scan their dirs for usage-shaped
/// JSONL via `GenericJsonl`.
const GENERIC: &[AgentSpec] = &[
    (
        "amp",
        "Amp",
        &[
            (Base::Data, "amp"),
            (Base::Config, "amp"),
            (Base::Config, "cliamp"),
        ],
    ),
    (
        "qwen",
        "Qwen",
        &[(Base::Home, ".qwen"), (Base::Config, "qwen")],
    ),
    (
        "kimi",
        "Kimi",
        &[(Base::Home, ".kimi"), (Base::Config, "kimi")],
    ),
    (
        "goose",
        "Goose",
        &[(Base::Data, "goose"), (Base::Config, "goose")],
    ),
    (
        "copilot",
        "Copilot",
        &[(Base::Home, ".copilot"), (Base::Config, "github-copilot")],
    ),
    (
        "droid",
        "Droid",
        &[(Base::Home, ".factory"), (Base::Config, "droid")],
    ),
    (
        "codebuff",
        "Codebuff",
        &[(Base::Home, ".codebuff"), (Base::Data, "codebuff")],
    ),
    (
        "hermes",
        "Hermes",
        &[(Base::Home, ".hermes"), (Base::Config, "hermes")],
    ),
    ("pi", "Pi", &[(Base::Home, ".pi"), (Base::Config, "pi")]),
    (
        "openclaw",
        "OpenClaw",
        &[(Base::Home, ".openclaw"), (Base::Config, "openclaw")],
    ),
    (
        "antigravity",
        "Antigravity",
        &[(Base::Home, ".antigravity"), (Base::Config, "antigravity")],
    ),
];

fn base_dir(b: &Base) -> PathBuf {
    match b {
        Base::Home => paths::home(),
        Base::Data => paths::xdg_data(),
        Base::Config => paths::xdg_config(),
    }
}

fn generic_reader(spec: &'static AgentSpec) -> Box<dyn AgentReader> {
    Box::new(jsonl::GenericJsonl {
        id: spec.0,
        display: spec.1,
        dirs: spec
            .2
            .iter()
            .map(|(b, sub)| base_dir(b).join(sub))
            .collect(),
    })
}

pub fn all_agents() -> Vec<Box<dyn AgentReader>> {
    BESPOKE
        .iter()
        .map(|b| (b.3)())
        .chain(GENERIC.iter().map(generic_reader))
        .collect()
}

/// (display, legacy_json) for a known id.
fn meta(id: &str) -> Option<(&'static str, bool)> {
    if let Some(b) = BESPOKE.iter().find(|b| b.0 == id) {
        Some((b.1, b.2))
    } else {
        GENERIC.iter().find(|g| g.0 == id).map(|g| (g.1, false))
    }
}

/// Known agent ids, for help text and error messages.
pub fn agent_ids() -> Vec<&'static str> {
    BESPOKE
        .iter()
        .map(|b| b.0)
        .chain(GENERIC.iter().map(|g| g.0))
        .collect()
}

pub fn by_id(id: &str) -> Option<Box<dyn AgentReader>> {
    if let Some(b) = BESPOKE.iter().find(|b| b.0 == id) {
        Some((b.3)())
    } else {
        GENERIC.iter().find(|g| g.0 == id).map(generic_reader)
    }
}

/// Whether agent-scoped JSON uses the legacy (codex-era) schema.
pub fn legacy_json(id: &str) -> bool {
    meta(id).map(|(_, l)| l).unwrap_or(false)
}

/// Display name for an agent id (`None` for unknown ids).
pub fn display_for(id: &str) -> Option<&'static str> {
    meta(id).map(|(d, _)| d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Tokens;

    fn entry(agent: &'static str, key: &str) -> UsageEntry {
        UsageEntry {
            agent,
            timestamp_ms: 0,
            session_id: "s".into(),
            model: "m".into(),
            provider: "".into(),
            tokens: Tokens::default(),
            cost_usd: None,
            dedup_key: key.into(),
            dedup_id: 0,
                total_override: None,
                is_fallback: false,
            project: "".into(),
        }
    }

    #[test]
    fn dedup_drops_repeat_keys_keeps_distinct() {
        let out = dedup(vec![
            entry("a", "k1"),
            entry("a", "k1"), // exact dup -> dropped
            entry("a", "k2"),
            entry("b", "k1"), // same key, other agent -> kept
            entry("a", ""),   // empty keys never dedup
            entry("a", ""),
        ]);
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn registry_answers_without_readers() {
        assert!(agent_ids().contains(&"devin"));
        assert_eq!(display_for("zcode"), Some("ZCode"));
        assert_eq!(display_for("nonexistent"), None);
        assert!(legacy_json("codex"));
        assert!(!legacy_json("claude"));
        // every registered id constructs a reader with the same identity
        for id in agent_ids() {
            let r = by_id(id).unwrap();
            assert_eq!(r.id(), id);
        }
    }
}
