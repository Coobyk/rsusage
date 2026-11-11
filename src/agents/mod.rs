mod jsonl;
mod sqlite;

use crate::paths;
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

/// Globally dedup entries by `agent:dedup_key`. Empty keys are always unique.
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
        Box::new(sqlite::OpenCodeDb {
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
            tokens: Tokens::default(),
            cost_usd: None,
            dedup_key: key.into(),
            project: String::new(),
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
