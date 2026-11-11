use std::collections::BTreeMap;

/// Model name used when a source doesn't record one.
pub const UNKNOWN_MODEL: &str = "<unknown>";

/// The five token channels every reader, accumulator and report shares.
/// `reasoning` is tracked but excluded from `total_no_reasoning`, which is
/// what upstream reports as `totalTokens` in block and codex-legacy rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

impl std::ops::AddAssign for Tokens {
    fn add_assign(&mut self, o: Self) {
        self.input += o.input;
        self.output += o.output;
        self.reasoning += o.reasoning;
        self.cache_creation += o.cache_creation;
        self.cache_read += o.cache_read;
    }
}

impl Tokens {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.reasoning + self.cache_creation + self.cache_read
    }

    /// Upstream's block/codex-row total: everything but reasoning.
    pub fn total_no_reasoning(&self) -> u64 {
        self.input + self.output + self.cache_creation + self.cache_read
    }

    pub fn is_zero(&self) -> bool {
        self.total() == 0
    }
}

/// One billable event: a single model request / turn with token usage.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UsageEntry {
    /// agent id: "claude", "codex", "opencode", "devin", ...
    /// (`'static` str; the entry cache re-attaches it at read time.)
    #[serde(skip)]
    pub agent: &'static str,
    /// unix epoch millis (UTC)
    pub timestamp_ms: i64,
    /// session/conversation identifier
    pub session_id: String,
    /// model name as reported by the harness
    pub model: String,
    pub tokens: Tokens,
    /// cost reported by the harness itself, if any (display mode)
    pub cost_usd: Option<f64>,
    /// dedup key (e.g. messageId:requestId); entries with identical keys are skipped
    pub dedup_key: String,
    /// working directory / project label, best effort
    pub project: String,
}

impl UsageEntry {
    /// Serialize minus `agent` for the entry cache (agent re-attached on read).
    pub fn cache_blob(&self) -> Vec<u8> {
        bincode::serialize(self).unwrap_or_default()
    }

    /// Rebuild from a cache row, attaching the agent the cache belongs to.
    pub fn from_cache_blob(b: &[u8], agent: &'static str) -> Option<Self> {
        let mut e: Self = bincode::deserialize(b).ok()?;
        e.agent = agent;
        Some(e)
    }
}

/// Per-model aggregate inside a report row.
#[derive(Debug, Clone, Default)]
pub struct ModelAgg {
    pub tokens: Tokens,
    /// harness-reported cost sum (display mode)
    pub display_cost: f64,
    /// sum of calculated costs (entries with a pricing hit)
    pub calc_cost: f64,
    /// sum of per-entry auto cost: stored when present else calculated
    pub auto_cost: f64,
    /// number of entries with a reported cost
    pub cost_count: u64,
}

/// Mutable accumulator for a group of entries (period, session, block...).
#[derive(Debug, Clone, Default)]
pub struct Acc {
    pub tokens: Tokens,
    /// model -> ModelAgg
    pub models: BTreeMap<String, ModelAgg>,
    /// first-seen model order (ccusage keeps insertion order in modelsUsed)
    pub model_order: Vec<String>,
    /// agent -> per-agent aggregates (one level deep only)
    pub agents: BTreeMap<&'static str, AgentAcc>,
    pub project: String,
    pub last_activity_ms: i64,
    pub first_activity_ms: i64,
    pub entry_count: u64,
}

/// Per-agent slice of an `Acc`: the fields `agent_breakdowns` actually reads.
#[derive(Debug, Clone, Default)]
pub struct AgentAcc {
    pub tokens: Tokens,
    pub models: BTreeMap<String, ModelAgg>,
    pub entry_count: u64,
}

impl Acc {
    pub fn add(&mut self, e: &UsageEntry, pricing: &crate::pricing::Pricing) {
        // a stored cost of 0 means "not computed" (harnesses write it for
        // free-tier models) — auto mode must fall back to the catalog price
        let stored = e.cost_usd.filter(|&c| c != 0.0);
        let calc = pricing.entry_cost(e);
        let auto = stored.or(calc).unwrap_or(0.0);
        self.tokens += e.tokens;
        self.entry_count += 1;
        if !self.models.contains_key(&e.model) {
            self.model_order.push(e.model.clone());
        }
        let m = self.models.entry(e.model.clone()).or_default();
        add_costs(m, e, stored, calc, auto);
        let a = self.agents.entry(e.agent).or_default();
        a.tokens += e.tokens;
        a.entry_count += 1;
        if self.project.is_empty() && !e.project.is_empty() {
            self.project = e.project.clone();
        }
        if self.last_activity_ms == 0 || e.timestamp_ms > self.last_activity_ms {
            self.last_activity_ms = e.timestamp_ms;
        }
        if self.first_activity_ms == 0 || e.timestamp_ms < self.first_activity_ms {
            self.first_activity_ms = e.timestamp_ms;
        }
        add_costs(
            a.models.entry(e.model.clone()).or_default(),
            e,
            stored,
            calc,
            auto,
        );
    }

    pub fn total(&self) -> u64 {
        self.tokens.total()
    }
}

fn add_costs(m: &mut ModelAgg, e: &UsageEntry, stored: Option<f64>, calc: Option<f64>, auto: f64) {
    m.tokens += e.tokens;
    if let Some(c) = stored {
        m.display_cost += c;
        m.cost_count += 1;
    }
    if let Some(c) = calc {
        m.calc_cost += c;
    }
    m.auto_cost += auto;
}

/// Serializable report row (daily/weekly/monthly/session).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub period: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
    pub total_cost: f64,
    /// kept for legacy-schema compat output; not in the unified schema
    #[serde(skip)]
    pub reasoning_tokens: u64,
    /// session rows only
    #[serde(skip)]
    pub first_activity: Option<String>,
    #[serde(skip)]
    pub last_activity: Option<String>,
    #[serde(skip)]
    pub project: String,
    pub models_used: Vec<String>,
    pub model_breakdowns: Vec<ModelRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_breakdowns: Option<Vec<AgentRow>>,
}

impl Row {
    /// Token+cost totals view shared by the table totals row and JSON totals.
    pub fn totals(&self) -> Totals {
        Totals {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_creation_tokens: self.cache_creation_tokens,
            cache_read_tokens: self.cache_read_tokens,
            total_tokens: self.total_tokens,
            total_cost: self.total_cost,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRow {
    pub model_name: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost: f64,
    #[serde(skip)]
    pub reasoning_tokens: u64,
    #[serde(skip)]
    pub stored_cost: Option<f64>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub missing_pricing: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRow {
    pub agent: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
    pub total_cost: f64,
    pub models_used: Vec<String>,
}

impl AgentRow {
    /// Same totals view as `Row::totals`, so table cells share one layout.
    pub fn totals(&self) -> Totals {
        Totals {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_creation_tokens: self.cache_creation_tokens,
            cache_read_tokens: self.cache_read_tokens,
            total_tokens: self.total_tokens,
            total_cost: self.total_cost,
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
    pub total_cost: f64,
}

impl Totals {
    pub fn sum(rows: &[Row]) -> Self {
        let mut t = Totals::default();
        for r in rows {
            t.input_tokens += r.input_tokens;
            t.output_tokens += r.output_tokens;
            t.cache_creation_tokens += r.cache_creation_tokens;
            t.cache_read_tokens += r.cache_read_tokens;
            t.total_tokens += r.total_tokens;
            t.total_cost += r.total_cost;
        }
        t
    }
}

/// A `block_hours`-long billing block starting at the floor-hour of its
/// first entry.
#[derive(Debug, Clone)]
pub struct Block {
    pub start_ms: i64,
    pub end_ms: i64,
    pub acc: Acc,
    pub is_active: bool,
}

/// Token counts inside a block row — upstream uses `*InputTokens` names here
/// (unlike the `*Tokens` names in period rows).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockTokens {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockRow {
    pub actual_end_time: Option<String>,
    pub burn_rate: Option<serde_json::Value>,
    #[serde(rename = "costUSD")]
    pub cost_usd: f64,
    pub end_time: String,
    pub entries: u64,
    pub id: String,
    pub is_active: bool,
    pub is_gap: bool,
    pub models: Vec<String>,
    pub projection: Option<serde_json::Value>,
    pub start_time: String,
    pub token_counts: BlockTokens,
    pub total_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_limit_projection: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(agent: &'static str, tokens: Tokens) -> UsageEntry {
        UsageEntry {
            agent,
            timestamp_ms: 1_000,
            session_id: "s".into(),
            model: "m".into(),
            tokens,
            cost_usd: None,
            dedup_key: String::new(),
            project: String::new(),
        }
    }

    #[test]
    fn totals() {
        let t = Tokens {
            input: 1,
            output: 2,
            reasoning: 4,
            cache_creation: 8,
            cache_read: 16,
        };
        assert_eq!(t.total(), 31);
        // upstream's block/codex total excludes reasoning
        assert_eq!(t.total_no_reasoning(), 27);
        assert!(!t.is_zero());
        assert!(Tokens::default().is_zero());
    }

    #[test]
    fn cache_blob_roundtrip() {
        let e = entry("devin", Tokens::default());
        let blob = e.cache_blob();
        let back = UsageEntry::from_cache_blob(&blob, "devin").unwrap();
        assert_eq!(back.agent, "devin");
        assert_eq!(back.timestamp_ms, e.timestamp_ms);
        assert_eq!(back.tokens, e.tokens);
        assert_eq!(back.dedup_key, e.dedup_key);
        // agent comes from the cache table, not the blob
        let renamed = UsageEntry::from_cache_blob(&blob, "kilo").unwrap();
        assert_eq!(renamed.agent, "kilo");
    }

    #[test]
    fn acc_tracks_model_and_agent_slices() {
        let p = crate::pricing::Pricing::default();
        let mut acc = Acc::default();
        let t = Tokens {
            input: 10,
            ..Default::default()
        };
        acc.add(&entry("claude", t), &p);
        acc.add(&entry("devin", t), &p);
        acc.add(&entry("devin", t), &p);
        assert_eq!(acc.total(), 30);
        assert_eq!(acc.entry_count, 3);
        assert_eq!(acc.agents.len(), 2);
        assert_eq!(acc.agents["devin"].tokens.input, 20);
        assert_eq!(acc.models["m"].tokens.input, 30);
    }
}
