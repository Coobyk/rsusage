use rustc_hash::FxHashMap;
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
///
/// Strings are `Arc<str>` so the entry cache can intern them — a model name
/// shared by 100k entries costs one allocation, not 100k.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct UsageEntry {
    /// agent id: "claude", "codex", "opencode", "devin", ...
    /// (`'static` str; the entry cache re-attaches it at read time.)
    #[serde(skip)]
    pub agent: &'static str,
    /// unix epoch millis (UTC)
    pub timestamp_ms: i64,
    /// session/conversation identifier
    pub session_id: std::sync::Arc<str>,
    /// model name as reported by the harness
    pub model: std::sync::Arc<str>,
    /// provider id as reported by the harness ("" when the source has none)
    pub provider: std::sync::Arc<str>,
    pub tokens: Tokens,
    /// cost reported by the harness itself, if any (display mode)
    pub cost_usd: Option<f64>,
    /// dedup key (e.g. messageId:requestId); entries with identical keys are skipped
    pub dedup_key: std::sync::Arc<str>,
    /// precomputed `agent:key` dedup hash; nonzero means "trust me, skip hashing
    /// and the empty-key check" — the entry cache stores this so decode never
    /// needs the key string itself
    #[serde(skip)]
    pub dedup_id: u64,
    /// reported total when the source tracks one (codex `total_tokens`) —
    /// sources where reasoning sits inside the counted buckets use this so
    /// `totalTokens` is not inflated by counting reasoning a second time
    #[serde(default)]
    pub total_override: Option<u64>,
    /// codex: the model was a runtime fallback, not the record's pick —
    /// reported as `isFallback` in codex's `models` breakdown
    #[serde(default)]
    pub is_fallback: bool,
    /// working directory / project label, best effort
    pub project: std::sync::Arc<str>,
}

/// Per-model aggregate inside a report row.
#[derive(Debug, Clone, Default)]
pub struct ModelAgg {
    pub tokens: Tokens,
    /// codex only: tokens from requests whose whole context exceeded the
    /// model's long-context threshold — upstream prices the aggregated
    /// bucket with per-request long-context bookkeeping
    pub long: Tokens,
    /// Σ per-day row costs when this aggregate was folded from daily rows —
    /// upstream builds weekly/monthly over daily summaries, so a folded
    /// model's cost is the day-by-day sum, not one flat aggregate
    pub folded_cost: Option<f64>,
    /// harness-reported cost sum (display mode)
    pub display_cost: f64,
    /// sum of calculated costs (entries with a pricing hit)
    pub calc_cost: f64,
    /// sum of per-entry auto cost: stored when present else calculated
    pub auto_cost: f64,
    /// number of entries with a reported cost
    pub cost_count: u64,
    /// any unresolved-cost entry that stays missing in `auto` mode —
    /// upstream marks `missingPricing` per record; a stored cost suppresses
    /// the flag by each adapter's own rule
    pub any_missing_auto: bool,
    /// same for `calculate` mode — only kilo-style adapters (a stored cost
    /// above zero) suppress the flag there
    pub any_missing_calc: bool,
    /// sum of per-entry `total_override` — reported totals the source counted
    /// itself (codex); entries without one contribute `tokens.total()`
    pub total_sum: u64,
    /// Σ per-entry reported input — codex entries contribute non-cached
    /// input (raw minus both cache buckets), others raw input
    pub display_input: u64,
    /// codex `isFallback` — any event whose model was a runtime fallback
    pub is_fallback: bool,
}

/// Mutable accumulator for a group of entries (period, session, block...).
#[derive(Debug, Clone, Default)]
pub struct Acc {
    pub tokens: Tokens,
    /// per-entry cost sums in entry order — upstream's `total_cost`
    /// accumulates `entry.cost`, so a row's `totalCost` is this, not Σ models
    pub cost_display: f64,
    pub cost_calc: f64,
    pub cost_auto: f64,
    /// Σ per-entry reported totals (codex); others add `tokens.total()`
    pub total_sum: u64,
    /// Σ per-entry reported input (codex entries non-cached)
    pub display_input: u64,
    /// model -> ModelAgg (unordered; consumers sort by name for output)
    pub models: FxHashMap<String, ModelAgg>,
    /// first-seen model order (ccusage keeps insertion order in modelsUsed)
    pub model_order: Vec<String>,
    /// agent -> per-agent aggregates (one level deep only)
    pub agents: BTreeMap<&'static str, AgentAcc>,
    /// Σ per-day row costs when folded from daily rows (weekly/monthly)
    pub folded_cost: Option<f64>,
    /// breakdown insertion order after a daily fold — upstream folds each
    /// day's cost-desc-sorted breakdown list at its first-seen position,
    /// which differs from `modelsUsed`' first-seen order; empty on direct
    /// accumulators (they share `model_order`)
    pub breakdown_order: Vec<String>,
    /// merged folds only: each agent's per-day slice costs in date order —
    /// upstream's flat row list arrives agent-major, so a merged period's
    /// total sums every day-slice cost in agent-major-then-date order and
    /// can't be rebuilt from slice subtotals without changing the last ulp
    pub day_costs: BTreeMap<&'static str, Vec<f64>>,
    pub project: String,
    pub last_activity_ms: i64,
    pub first_activity_ms: i64,
    pub entry_count: u64,
}

/// Per-agent slice of an `Acc`: the fields `agent_breakdowns` actually reads.
#[derive(Debug, Clone, Default)]
pub struct AgentAcc {
    pub tokens: Tokens,
    pub models: FxHashMap<String, ModelAgg>,
    /// first-seen model order within the agent slice — merged model
    /// breakdowns order each agent's rows by stable cost-desc over this
    pub model_order: Vec<String>,
    pub entry_count: u64,
    pub total_sum: u64,
    /// Σ per-entry reported input (codex entries non-cached)
    pub display_input: u64,
    /// per-entry cost sums by mode — a merged row's slice cost is the
    /// agent's own entry-order sum, not Σ per-model
    pub cost_display: f64,
    pub cost_calc: f64,
    pub cost_auto: f64,
    /// Σ per-day slice costs when folded from daily rows (weekly/monthly)
    pub folded_cost: Option<f64>,
    /// like `Acc::breakdown_order` — populated only by the daily fold
    pub breakdown_order: Vec<String>,
}

impl Acc {
    /// The order `modelBreakdowns` should iterate — the daily-fold
    /// insertion order when folded, else first-seen order.
    pub fn bd_order(&self) -> &[String] {
        if self.breakdown_order.is_empty() {
            &self.model_order
        } else {
            &self.breakdown_order
        }
    }
}

impl AgentAcc {
    /// Slice-level `bd_order` — same folded-vs-first-seen rule.
    pub fn bd_order(&self) -> &[String] {
        if self.breakdown_order.is_empty() {
            &self.model_order
        } else {
            &self.breakdown_order
        }
    }
}

impl Acc {
    pub fn add(&mut self, e: &UsageEntry, pricing: &crate::pricing::Pricing) {
        self.add_costed(e, pricing.entry_cost(e), true, pricing);
    }

    /// `add` with the catalog cost already resolved (`pricing.entry_cost`)
    /// and agent slices optional — only merged reports read `agents`.
    pub fn add_costed(
        &mut self,
        e: &UsageEntry,
        calc: Option<f64>,
        track_agents: bool,
        pricing: &crate::pricing::Pricing,
    ) {
        // stored-cost semantics per upstream adapters: opencode discards a
        // stored 0 ("not computed" on free-tier records) and recalculates;
        // codex has no stored cost upstream — its reports price every record,
        // so all modes emit the calculated value; every other agent's Auto
        // mode takes a stored cost verbatim
        let stored = if e.agent == "opencode" {
            e.cost_usd.filter(|&c| c > 0.0)
        } else if e.agent == "codex" {
            calc
        } else {
            e.cost_usd
        };
        let auto = stored.or(calc).unwrap_or(0.0);
        // upstream reports non-cached input for codex (`non_cached_input`),
        // raw input for everyone else — tracked per entry so merged rows
        // mix the semantics correctly
        let display_input = if e.agent == "codex" {
            e.tokens
                .input
                .saturating_sub(e.tokens.cache_read.saturating_add(e.tokens.cache_creation))
        } else {
            e.tokens.input
        };
        self.tokens += e.tokens;
        self.display_input += display_input;
        self.cost_display += stored.unwrap_or(0.0);
        self.cost_calc += calc.unwrap_or(0.0);
        self.cost_auto += auto;
        self.total_sum += e.total_override.unwrap_or_else(|| e.tokens.total());
        self.entry_count += 1;
        match self.models.get_mut(e.model.as_ref()) {
            Some(m) => add_costs(m, e, stored, calc, auto),
            None => {
                self.model_order.push(e.model.to_string());
                add_costs(
                    self.models.entry(e.model.to_string()).or_default(),
                    e,
                    stored,
                    calc,
                    auto,
                );
            }
        }
        if track_agents {
            let a = self.agents.entry(e.agent).or_default();
            a.tokens += e.tokens;
            a.display_input += display_input;
            a.total_sum += e.total_override.unwrap_or_else(|| e.tokens.total());
            a.entry_count += 1;
            a.cost_display += stored.unwrap_or(0.0);
            a.cost_calc += calc.unwrap_or(0.0);
            a.cost_auto += auto;
            if a.models.contains_key(e.model.as_ref()) {
                add_costs(
                    a.models.get_mut(e.model.as_ref()).unwrap(),
                    e,
                    stored,
                    calc,
                    auto,
                );
            } else {
                a.model_order.push(e.model.to_string());
                add_costs(
                    a.models.entry(e.model.to_string()).or_default(),
                    e,
                    stored,
                    calc,
                    auto,
                );
            }
        }
        // codex bills whole requests: entries whose raw context crosses the
        // model's tier boundary join the long bucket so report-level costs
        // can price the aggregated buckets like upstream
        if e.agent == "codex" && e.tokens.input > pricing.codex_threshold(&e.model) {
            if let Some(m) = self.models.get_mut(e.model.as_ref()) {
                m.long += e.tokens;
            }
            if let Some(m) = self
                .agents
                .get_mut(e.agent)
                .and_then(|a| a.models.get_mut(e.model.as_ref()))
            {
                m.long += e.tokens;
            }
        }
        if self.project.is_empty() && !e.project.is_empty() {
            self.project = e.project.to_string();
        }
        if self.last_activity_ms == 0 || e.timestamp_ms > self.last_activity_ms {
            self.last_activity_ms = e.timestamp_ms;
        }
        if self.first_activity_ms == 0 || e.timestamp_ms < self.first_activity_ms {
            self.first_activity_ms = e.timestamp_ms;
        }
    }

    pub fn total(&self) -> u64 {
        self.total_sum
    }
}

/// Whether a positive stored cost suppresses `missingPricing` for this
/// adapter: kilo/opencode/hermes treat any `cost_usd > 0` as priced in every
/// non-display mode; the rest only let a stored cost answer in `auto`.
fn stored_suppresses_missing(agent: &str, stored: Option<f64>) -> (bool, bool) {
    let gt_zero = matches!(agent, "kilo" | "opencode" | "hermes");
    (
        if gt_zero {
            stored.is_some_and(|c| c > 0.0)
        } else {
            stored.is_some()
        },
        gt_zero && stored.is_some_and(|c| c > 0.0),
    )
}

fn add_costs(
    m: &mut ModelAgg,
    e: &UsageEntry,
    stored: Option<f64>,
    calc: Option<f64>,
    auto: f64,
) {
    m.tokens += e.tokens;
    m.display_input += if e.agent == "codex" {
        e.tokens
            .input
            .saturating_sub(e.tokens.cache_read.saturating_add(e.tokens.cache_creation))
    } else {
        e.tokens.input
    };
    m.total_sum += e.total_override.unwrap_or_else(|| e.tokens.total());
    m.is_fallback |= e.is_fallback;
    if calc.is_none() && e.tokens.total() > 0 {
        let (auto, calc_m) = stored_suppresses_missing(e.agent, stored);
        m.any_missing_auto |= !auto;
        m.any_missing_calc |= !calc_m;
    }
    if let Some(c) = stored {
        m.display_cost += c;
        m.cost_count += 1;
    }
    if let Some(c) = calc {
        m.calc_cost += c;
    }
    m.auto_cost += auto;
}

/// Upstream `json_float`: integral costs serialize as JSON ints (`0`, not
/// `0.0`). Only `costUSD`-style fields use it; `totalCost`/`cost` stay f64.
pub(crate) fn json_float(v: f64) -> serde_json::Value {
    if v.is_finite() && v.fract() == 0.0 && v >= i64::MIN as f64 && v <= i64::MAX as f64 {
        serde_json::Value::from(v as i64)
    } else {
        serde_json::Value::from(v)
    }
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
    /// Table-only data: upstream renders per-agent sub-rows in merged tables
    /// but its default JSON schema carries no `agentBreakdowns` field.
    #[serde(skip)]
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
            unpriced_models: Vec::new(),
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
    /// reported total (codex `models.<m>.totalTokens`); table rows don't read it
    #[serde(skip)]
    pub total_tokens: u64,
    /// codex `models.<m>.isFallback`
    #[serde(skip)]
    pub is_fallback: bool,
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
    /// Table-only: merged `--breakdown` renders each agent slice's models
    /// under its sub-row
    pub model_breakdowns: Vec<ModelRow>,
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
            unpriced_models: Vec::new(),
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
    /// models flagged `missingPricing` anywhere in the report, sorted+deduped;
    /// absent (not empty) when everything resolved
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unpriced_models: Vec<String>,
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
            provider: "".into(),
            tokens,
            cost_usd: None,
            dedup_key: "".into(),
            dedup_id: 0,
            total_override: None,
            is_fallback: false,
            project: "".into(),
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
