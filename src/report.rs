//! Aggregation: period bucketing, session grouping, 5h blocks, cost modes.

use crate::pricing::Pricing;
use crate::types::*;
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, clap::ValueEnum)]
pub enum CostMode {
    /// stored cost when present, else calculated
    #[default]
    Auto,
    /// always calculate from the price catalog
    Calculate,
    /// always use the harness-reported cost
    Display,
}

/// Which report section to produce. `--sections` parses with `from_name`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Report {
    Daily,
    Weekly,
    Monthly,
    Session,
}

impl Report {
    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim() {
            "daily" => Some(Self::Daily),
            "weekly" => Some(Self::Weekly),
            "monthly" => Some(Self::Monthly),
            "session" => Some(Self::Session),
            _ => None,
        }
    }

    /// JSON section key in merged reports (agent-scoped uses "sessions").
    pub fn name(&self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Session => "session",
        }
    }

    /// Title-case label for the report banner.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Daily => "Daily",
            Self::Weekly => "Weekly",
            Self::Monthly => "Monthly",
            Self::Session => "Session",
        }
    }

    /// First table column header.
    pub fn period_header(&self) -> &'static str {
        match self {
            Self::Session => "Session",
            _ => "Date",
        }
    }

    /// Group entries into this report's rows.
    pub fn rows(&self, entries: &[UsageEntry], ctx: &Ctx) -> Vec<Row> {
        // `session:`-keyed opencode aggregates only feed session reports, and
        // only without a temporal filter — upstream never loads them otherwise
        let filtered;
        let entries = if matches!(self, Self::Session) && !ctx.temporal_filter {
            entries
        } else {
            filtered = entries
                .iter()
                .filter(|e| !is_session_aggregate(e))
                .cloned()
                .collect::<Vec<_>>();
            &filtered
        };
        match self {
            Self::Daily => group_by(entries, ctx, *self, |d| d, |d| d.to_string()),
            Self::Weekly => {
                group_by(entries, ctx, *self, |d| week_start(d, ctx.sow), |d| {
                    d.to_string()
                })
            }
            Self::Monthly => group_by(
                entries,
                ctx,
                *self,
                |d| jiff::civil::date(d.year(), d.month(), 1),
                |d| format!("{}-{:02}", d.year(), d.month()),
            ),
            Self::Session => group_session(entries, ctx),
        }
    }
}

pub struct Ctx<'a> {
    pub pricing: &'a Pricing,
    pub mode: CostMode,
    pub tz: jiff::tz::TimeZone,
    /// merged report over all agents
    pub merged: bool,
    pub no_cost: bool,
    /// week-start weekday index (Sunday=0); resolved by the caller since the
    /// default depends on the agent (upstream: sunday for claude, monday else)
    pub sow: i64,
    /// --since/--until present — upstream disables session-aggregate fallback
    /// whenever a temporal filter is active
    pub temporal_filter: bool,
    /// --order value and whether the user passed it — upstream's merged
    /// session report sorts by cost desc unless `--order asc` was explicit
    pub order: crate::cli::Order,
    pub order_explicit: bool,
    /// agent this report belongs to ("" in merged reports) — gates the
    /// adapter-specific bits: codex's raw-input display and the per-adapter
    /// `include_session_metadata` flag upstream passes per report kind
    pub agent: &'static str,
}

impl<'a> Ctx<'a> {
    pub fn new(
        pricing: &'a Pricing,
        mode: CostMode,
        tz: jiff::tz::TimeZone,
        merged: bool,
        no_cost: bool,
        sow: i64,
        temporal_filter: bool,
        order: crate::cli::Order,
        order_explicit: bool,
        agent: &'static str,
    ) -> Self {
        Self {
            pricing,
            mode,
            tz,
            merged,
            no_cost,
            sow,
            temporal_filter,
            order,
            order_explicit,
            agent,
        }
    }

    /// Codex prices aggregated buckets upstream (`calculate_codex_model_cost`
    /// on the no-`timestamped_usage` path): one `find` per model, bucket cost
    /// over the summed usage — *not* a per-entry sum.
    fn codex_bucket(&self, model: &str, m: &ModelAgg) -> f64 {
        self.pricing.codex_bucket_cost(model, &m.tokens, &m.long)
    }

    /// `cost` specialized per owning agent — codex's bucket semantics differ
    /// from the per-entry sums every other agent uses.
    fn cost_for(&self, agent: &str, model: &str, m: &ModelAgg) -> (f64, bool) {
        let missing = match self.mode {
            CostMode::Display => false,
            CostMode::Auto => m.any_missing_auto,
            CostMode::Calculate => m.any_missing_calc,
        };
        if let Some(f) = m.folded_cost {
            return (f, missing);
        }
        if agent == "codex" {
            (self.codex_bucket(model, m), missing)
        } else {
            let c = match self.mode {
                CostMode::Display => m.display_cost,
                CostMode::Calculate => m.calc_cost,
                CostMode::Auto => m.auto_cost,
            };
            (c, missing)
        }
    }

    /// A merged agent slice's cost: the agent's own row cost — bucket sums
    /// for codex, entry-order sums for everyone else.
    fn agent_slice_cost(&self, id: &str, a: &AgentAcc) -> f64 {
        if let Some(f) = a.folded_cost {
            return f;
        }
        if id == "codex" {
            sorted_models(&a.models)
                .into_iter()
                .map(|(n, m)| self.codex_bucket(n, m))
                .sum()
        } else {
            match self.mode {
                CostMode::Display => a.cost_display,
                CostMode::Calculate => a.cost_calc,
                CostMode::Auto => a.cost_auto,
            }
        }
    }

    /// Cost for a model aggregate, honoring the mode (per-entry semantics).
    /// `missingPricing` is per-mode: auto/calculate use the flags recorded at
    /// accumulation; display mode never flags.
    pub fn cost(&self, _model: &str, m: &ModelAgg) -> (f64, bool) {
        let missing = match self.mode {
            CostMode::Display => false,
            CostMode::Auto => m.any_missing_auto,
            CostMode::Calculate => m.any_missing_calc,
        };
        let c = match self.mode {
            CostMode::Display => m.display_cost,
            CostMode::Calculate => m.calc_cost,
            CostMode::Auto => m.auto_cost,
        };
        (c, missing)
    }

    /// Total cost of a whole accumulator — the per-entry sums upstream
    /// accumulates into `total_cost`, so order-of-addition matches.
    /// `--no-cost` never zeroes this: upstream still sorts by real cost
    /// and only strips the field at output time.
    pub fn acc_cost(&self, acc: &Acc) -> f64 {
        if let Some(f) = acc.folded_cost {
            return f;
        }
        if self.merged {
            // merged totalCost = Σ slice costs — upstream's flat row list is
            // agent-major in `BUILT_IN_AGENT_NAMES` order, not alphabetical,
            // and float sums keep that order to the last ulp
            if !acc.day_costs.is_empty() {
                let mut sum = AGENT_SPEC_ORDER
                    .iter()
                    .filter_map(|id| acc.day_costs.get(id))
                    .flat_map(|v| v.iter())
                    .sum::<f64>();
                for (id, v) in &acc.day_costs {
                    if !AGENT_SPEC_ORDER.contains(id) {
                        sum += v.iter().sum::<f64>();
                    }
                }
                return sum;
            }
            let mut sum = AGENT_SPEC_ORDER
                .iter()
                .filter_map(|id| acc.agents.get(id).map(|a| (*id, a)))
                .map(|(id, a)| self.agent_slice_cost(id, a))
                .sum::<f64>();
            for (id, a) in &acc.agents {
                if !AGENT_SPEC_ORDER.contains(id) {
                    sum += self.agent_slice_cost(id, a);
                }
            }
            return sum;
        }
        if self.agent == "codex" {
            // codex report rows price the aggregated per-model buckets
            // (`group.models` is a BTreeMap — name order)
            return sorted_models(&acc.models)
                .into_iter()
                .map(|(n, m)| self.codex_bucket(n, m))
                .sum();
        }
        match self.mode {
            CostMode::Display => acc.cost_display,
            CostMode::Calculate => acc.cost_calc,
            CostMode::Auto => acc.cost_auto,
        }
    }

    /// Upstream `include_session_metadata`: zcode/grok/others emit
    /// firstActivity/lastActivity/projectPath on session rows; opencode and
    /// kilo never do.
    fn session_meta(&self) -> bool {
        !matches!(self.agent, "opencode" | "kilo")
    }

    /// Adapters whose session summaries accumulate activity (upstream
    /// `SessionAccumulator` users) — this drives the table's `Last
    /// Activity` column and is a different set than `session_meta`'s
    /// JSON gate: opencode populates the field for the table while its
    /// session JSON omits it.
    fn session_activity(&self) -> bool {
        self.merged
            || matches!(
                self.agent,
                "opencode"
                    | "zcode"
                    | "qwen"
                    | "pi"
                    | "openclaw"
                    | "copilot"
                    | "cline"
                    | "antigravity"
                    | "grok"
                    | "codex"
            )
    }

    /// One agent slice's breakdown rows in upstream order: stable cost-desc
    /// over first-seen insertion.
    fn agent_model_rows(&self, models: &FxHashMap<String, ModelAgg>, order: &[String]) -> Vec<ModelRow> {
        self.agent_model_rows_for(self.agent, models, order)
    }

    fn agent_model_rows_for(&self, agent: &str, models: &FxHashMap<String, ModelAgg>, order: &[String]) -> Vec<ModelRow> {
        let mut rows: Vec<ModelRow> = order
            .iter()
            .filter_map(|name| self.model_row_for(agent, name, models.get(name)?))
            .collect();
        rows.sort_by(|a, b| b.cost.total_cmp(&a.cost));
        rows
    }

    fn model_row_for(&self, agent: &str, name: &str, m: &ModelAgg) -> Option<ModelRow> {
        let (cost, missing) = self.cost_for(agent, name, m);
        Some(ModelRow {
            model_name: name.to_string(),
            input_tokens: m.display_input,
            output_tokens: m.tokens.output,
            cache_creation_tokens: m.tokens.cache_creation,
            cache_read_tokens: m.tokens.cache_read,
            cost,
            reasoning_tokens: m.tokens.reasoning,
            total_tokens: m.total_sum,
            is_fallback: m.is_fallback,
            missing_pricing: missing,
        })
    }

    /// Merged `modelBreakdowns`: upstream's `aggregate_model_breakdowns`
    /// folds each agent's already-ordered rows (agents alphabetical) by
    /// model name at first-seen position, then stable-sorts cost desc.
    fn merged_model_rows(&self, acc: &Acc) -> Vec<ModelRow> {
        let mut out: Vec<ModelRow> = Vec::new();
        let mut index: FxHashMap<String, usize> = FxHashMap::default();
        for (id, a) in &acc.agents {
            for r in self.agent_model_rows_for(id, &a.models, a.bd_order()) {
                match index.get(&r.model_name) {
                    Some(&i) => {
                        let t = &mut out[i];
                        t.input_tokens += r.input_tokens;
                        t.output_tokens += r.output_tokens;
                        t.cache_creation_tokens += r.cache_creation_tokens;
                        t.cache_read_tokens += r.cache_read_tokens;
                        t.cost += r.cost;
                        t.reasoning_tokens += r.reasoning_tokens;
                        t.total_tokens += r.total_tokens;
                        t.is_fallback |= r.is_fallback;
                        t.missing_pricing |= r.missing_pricing;
                    }
                    None => {
                        index.insert(r.model_name.clone(), out.len());
                        out.push(r);
                    }
                }
            }
        }
        out.sort_by(|a, b| b.cost.total_cmp(&a.cost));
        out
    }

    pub fn row(&self, kind: Report, period: String, acc: &Acc) -> Row {
        let breakdowns = if self.merged {
            self.merged_model_rows(acc)
        } else {
            self.agent_model_rows(&acc.models, acc.bd_order())
        };
        let agent_breakdowns = if self.merged && !acc.agents.is_empty() {
            let mut rows: Vec<AgentRow> = acc
                .agents
                .iter()
                .map(|(id, a)| {
                    AgentRow {
                        agent: id.to_string(),
                        input_tokens: a.display_input,
                        output_tokens: a.tokens.output,
                        cache_creation_tokens: a.tokens.cache_creation,
                        cache_read_tokens: a.tokens.cache_read,
                        total_tokens: a.total_sum,
                        total_cost: self.agent_slice_cost(id, a),
                        // a slice's modelsUsed keeps first-seen order on
                        // daily; folded weekly/monthly slices union each
                        // day's modelsUsed through a BTreeSet — sorted
                        models_used: if kind == Report::Daily {
                            a.model_order.clone()
                        } else {
                            let mut mu: Vec<String> =
                                a.models.keys().cloned().collect();
                            mu.sort_unstable();
                            mu
                        },
                        model_breakdowns: self.agent_model_rows_for(id, &a.models, a.bd_order()),
                    }
                })
                .collect();
            rows.sort_by(|a, b| a.agent.cmp(&b.agent));
            Some(rows)
        } else {
            None
        };
        let metadata = self.merged.then(|| {
            serde_json::json!({
                "agents": acc.agents.keys().copied().collect::<Vec<_>>(),
            })
        });
        Row {
            agent: self.merged.then(|| "all".into()),
            period,
            input_tokens: acc.display_input,
            output_tokens: acc.tokens.output,
            cache_creation_tokens: acc.tokens.cache_creation,
            cache_read_tokens: acc.tokens.cache_read,
            total_tokens: acc.total_sum,
            total_cost: self.acc_cost(acc),
            reasoning_tokens: acc.tokens.reasoning,
            first_activity: None,
            last_activity: None,
            project: acc.project.clone(),
            models_used: acc.model_order.clone(),
            model_breakdowns: breakdowns,
            metadata,
            agent_breakdowns,
        }
    }

    /// Session rows carry activity metadata (for adapters that opted in via
    /// `session_meta`). Merged session rows serialize activity inside
    /// `metadata` per upstream's `summary_metadata` — lastActivity for every
    /// agent that tracks it (kilo's summaries don't), plus codex's
    /// reasoningOutputTokens and pi's projectPath — and `agent` names the
    /// single contributing agent.
    pub fn session_row(&self, session_id: String, acc: &Acc) -> Row {
        let mut r = self.row(Report::Session, session_id, acc);
        if self.merged {
            // merged session rows are already per (agent, session) — no
            // agent sub-rows in the table (upstream leaves
            // `agent_breakdowns` empty here)
            r.agent_breakdowns = None;
            if let Some(&a) = acc.agents.keys().next() {
                r.agent = Some(a.to_string());
                if a != "kilo" {
                    let mut metadata = serde_json::json!({
                        "lastActivity": ms_to_iso(acc.last_activity_ms),
                    });
                    if a == "codex" {
                        metadata
                            .as_object_mut()
                            .unwrap()
                            .insert("reasoningOutputTokens".into(), acc.tokens.reasoning.into());
                    }
                    if a == "pi" && !acc.project.is_empty() {
                        metadata
                            .as_object_mut()
                            .unwrap()
                            .insert("projectPath".into(), acc.project.clone().into());
                    }
                    r.metadata = Some(metadata);
                } else {
                    r.metadata = None;
                }
            }
            return r;
        }
        // session adapters that don't accumulate activity (kilo, codex, …)
        // have no `last_activity` — no JSON field and no table column
        if self.session_activity() {
            r.first_activity = Some(ms_to_iso(acc.first_activity_ms));
            r.last_activity = Some(ms_to_iso(acc.last_activity_ms));
        }
        r
    }
}

/// Merged-row serialization: `totalCost` is `json_float` (integral costs
/// emit as ints upstream); aggregated rows collect `modelsUsed` in a
/// BTreeSet, but session rows keep the agent's own first-seen order.
pub fn merged_row_json(r: &Row, kind: Report, include_agents: bool) -> serde_json::Value {
    let mut v = serde_json::to_value(r).unwrap();
    let obj = v.as_object_mut().unwrap();
    obj.insert("totalCost".into(), json_float(r.total_cost));
    if kind != Report::Session {
        let mut used = r.models_used.clone();
        used.sort_unstable();
        obj.insert("modelsUsed".into(), used.into());
    }
    if include_agents {
        if let Some(breakdowns) = &r.agent_breakdowns {
            // `--by-agent` appends each agent slice in upstream's
            // `agent_json` field order; `totalCost` is json_float
            let agents: Vec<serde_json::Value> = breakdowns
                .iter()
                .map(|a| {
                    serde_json::json!({
                        "agent": a.agent,
                        "modelsUsed": a.models_used,
                        "inputTokens": a.input_tokens,
                        "outputTokens": a.output_tokens,
                        "cacheCreationTokens": a.cache_creation_tokens,
                        "cacheReadTokens": a.cache_read_tokens,
                        "totalTokens": a.total_tokens,
                        "totalCost": json_float(a.total_cost),
                        "modelBreakdowns": a.model_breakdowns,
                    })
                })
                .collect();
            obj.insert("agents".into(), agents.into());
        }
    }
    v
}

/// Agent-scoped row serialization. Upstream emits a per-agent schema:
///   default: {date|week|month|sessionId, inputTokens,
///     outputTokens, cacheCreationTokens, cacheReadTokens, totalTokens,
///     totalCost, modelsUsed, modelBreakdowns[...]}
///   legacy (codex): {date|week|month|sessionId, inputTokens, outputTokens,
///     reasoningOutputTokens, cacheReadTokens, cacheCreationTokens,
///     totalTokens, costUSD, models:{}, sessionFile?, directory?,
///     lastActivity?}
pub fn agent_row_json(r: &Row, kind: Report, ctx: &Ctx, legacy: bool) -> serde_json::Value {
    use serde_json::json;
    if legacy {
        // `models` is a BTreeMap upstream — keys emit alphabetically
        let models: serde_json::Map<String, serde_json::Value> = r
            .model_breakdowns
            .iter()
            .map(|mb| {
                let mut m = json!({
                    "inputTokens": mb.input_tokens,
                    "cacheCreationTokens": mb.cache_creation_tokens,
                    "cacheReadTokens": mb.cache_read_tokens,
                    "outputTokens": mb.output_tokens,
                    "reasoningOutputTokens": mb.reasoning_tokens,
                    "totalTokens": mb.total_tokens,
                    "isFallback": mb.is_fallback,
                });
                if mb.missing_pricing {
                    m.as_object_mut()
                        .unwrap()
                        .insert("missingPricing".into(), true.into());
                }
                (mb.model_name.clone(), m)
            })
            .collect::<std::collections::BTreeMap<_, _>>()
            .into_iter()
            .collect();
        let key = match kind {
            Report::Daily => "date",
            Report::Weekly => "week",
            Report::Monthly => "month",
            Report::Session => "sessionId",
        };
        let mut row = json!({
            key: r.period,
            "inputTokens": r.input_tokens,
            "cacheCreationTokens": r.cache_creation_tokens,
            "cacheReadTokens": r.cache_read_tokens,
            "outputTokens": r.output_tokens,
            "reasoningOutputTokens": r.reasoning_tokens,
            "totalTokens": r.total_tokens,
            "costUSD": crate::types::json_float(r.total_cost),
            "models": models,
        });
        if kind == Report::Session {
            row["lastActivity"] = r
                .last_activity
                .as_ref()
                .map_or(serde_json::Value::Null, |v| json!(v));
            let sep = r.period.rfind('/');
            row["sessionFile"] = json!(sep.map_or(r.period.as_str(), |i| &r.period[i + 1..]));
            row["directory"] = json!(sep.map_or("", |i| &r.period[..i]));
        }
        return row;
    }
    let breakdowns: Vec<serde_json::Value> = r
        .model_breakdowns
        .iter()
        .map(|mb| {
            let mut v = json!({
                "modelName": mb.model_name,
                "inputTokens": mb.input_tokens,
                "outputTokens": mb.output_tokens,
                "cacheCreationTokens": mb.cache_creation_tokens,
                "cacheReadTokens": mb.cache_read_tokens,
                "cost": mb.cost,
            });
            if mb.missing_pricing {
                v.as_object_mut()
                    .unwrap()
                    .insert("missingPricing".into(), true.into());
            }
            v
        })
        .collect();
    let mut o = serde_json::Map::new();
    match kind {
        Report::Session => {
            o.insert("sessionId".into(), r.period.clone().into());
        }
        Report::Weekly => {
            o.insert("week".into(), r.period.clone().into());
        }
        Report::Monthly => {
            o.insert("month".into(), r.period.clone().into());
        }
        Report::Daily => {
            o.insert("date".into(), r.period.clone().into());
        }
    }
    o.insert("inputTokens".into(), r.input_tokens.into());
    o.insert("outputTokens".into(), r.output_tokens.into());
    o.insert("cacheCreationTokens".into(), r.cache_creation_tokens.into());
    o.insert("cacheReadTokens".into(), r.cache_read_tokens.into());
    o.insert("totalTokens".into(), r.total_tokens.into());
    o.insert(
        "totalCost".into(),
        r.total_cost.into(),
    );
    o.insert("modelsUsed".into(), r.models_used.clone().into());
    o.insert("modelBreakdowns".into(), breakdowns.into());
    // session metadata goes *after* the shared fields, in upstream's
    // lastActivity/firstActivity/projectPath order; nulls when absent
    if kind == Report::Session && ctx.session_meta() {
        o.insert(
            "lastActivity".into(),
            r.last_activity
                .as_ref()
                .map_or(serde_json::Value::Null, |v| v.clone().into()),
        );
        o.insert(
            "firstActivity".into(),
            r.first_activity
                .as_ref()
                .map_or(serde_json::Value::Null, |v| v.clone().into()),
        );
        o.insert("projectPath".into(), r.project.clone().into());
    }
    serde_json::Value::Object(o)
}

/// ISO-8601 with exactly three millisecond digits ("...T00:00:00.260Z").
pub fn ms_to_iso(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_default()
}

pub fn date_key(ms: i64, tz: &jiff::tz::TimeZone) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_zoned(tz.clone()).date().to_string())
        .unwrap_or_default()
}

/// `--last N` is a period window, not a row count: "most recent N periods"
/// where 1 means today / this week (aligned to `sow`) / this month. Session
/// has no upstream counterpart; a day window is the sensible analog.
pub fn last_since(k: &Report, n: u32, ctx: &Ctx) -> i64 {
    let today = jiff::Timestamp::now().to_zoned(ctx.tz.clone()).date();
    let back = i64::from(n.saturating_sub(1));
    let start = match k {
        Report::Daily | Report::Session => {
            today.checked_sub(jiff::SignedDuration::from_hours(24 * back))
        }
        Report::Weekly => {
            let idx = i64::from(today.weekday().to_sunday_zero_offset());
            let sow_back = (idx - ctx.sow).rem_euclid(7);
            today.checked_sub(jiff::SignedDuration::from_hours(24 * (sow_back + 7 * back)))
        }
        Report::Monthly => jiff::civil::date(today.year(), today.month(), 1)
            .checked_sub(jiff::Span::new().months(back)),
    };
    start
        .and_then(|d| {
            d.to_datetime(jiff::civil::Time::midnight())
                .to_zoned(ctx.tz.clone())
        })
        .map(|z| z.timestamp().as_millisecond())
        .unwrap_or(i64::MIN)
}

/// Upstream `BUILT_IN_AGENT_NAMES` — the order merged reports sum agent
/// slices in, which is neither alphabetical nor first-seen.
const AGENT_SPEC_ORDER: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "amp",
    "droid",
    "codebuff",
    "hermes",
    "pi",
    "goose",
    "openclaw",
    "kilo",
    "copilot",
    "gemini",
    "antigravity",
    "kimi",
    "qwen",
    "grok",
    "zcode",
    "cline",
];

/// Model map in name order: `models` is an unordered FxHashMap but every
/// consumer needs the old BTreeMap (alphabetical) sequence — identical f64
/// add order for cost sums, identical tie order for stable-sorted rows.
fn sorted_models(m: &FxHashMap<String, ModelAgg>) -> Vec<(&String, &ModelAgg)> {
    let mut v: Vec<_> = m.iter().collect();
    v.sort_unstable_by(|a, b| a.0.cmp(b.0));
    v
}

/// Week-start canon. `sow` = weekday index (Sunday=0 .. Saturday=6).
fn week_start(d: jiff::civil::Date, sow: i64) -> jiff::civil::Date {
    let back = (i64::from(d.weekday().to_sunday_zero_offset()) - sow).rem_euclid(7);
    d.checked_sub(jiff::SignedDuration::from_hours(24 * back))
        .unwrap_or(d)
}

/// Chronological packing of a civil date (ordering preserved).
fn pack(d: jiff::civil::Date) -> i64 {
    (i64::from(d.year()) << 16) | (i64::from(d.month()) << 8) | i64::from(d.day())
}

/// ms -> local civil-date lookup. Local days partition the timeline into
/// [midnight, next-midnight) intervals; caching them makes a hit a couple
/// of integer compares instead of a tz resolution + date conversion.
struct DayCache {
    tz: jiff::tz::TimeZone,
    days: BTreeMap<i64, (i64, jiff::civil::Date)>,
    last: Option<(i64, i64, jiff::civil::Date)>,
}

impl DayCache {
    fn new(tz: jiff::tz::TimeZone) -> Self {
        Self {
            tz,
            days: BTreeMap::new(),
            last: None,
        }
    }

    /// ms at local midnight for a civil date.
    fn midnight_ms(d: jiff::civil::Date, tz: &jiff::tz::TimeZone) -> Option<i64> {
        d.to_datetime(jiff::civil::Time::midnight())
            .to_zoned(tz.clone())
            .ok()
            .map(|z| z.timestamp().as_millisecond())
    }

    fn date(&mut self, ms: i64) -> Option<jiff::civil::Date> {
        if let Some((lo, hi, d)) = self.last {
            if ms >= lo && ms < hi {
                return Some(d);
            }
        }
        if let Some((&lo, &(hi, d))) = self.days.range(..=ms).next_back() {
            if ms < hi {
                self.last = Some((lo, hi, d));
                return Some(d);
            }
        }
        let d = jiff::Timestamp::from_millisecond(ms)
            .ok()?
            .to_zoned(self.tz.clone())
            .date();
        let next = d.checked_add(jiff::SignedDuration::from_hours(24)).ok();
        if let (Some(lo), Some(hi)) = (
            Self::midnight_ms(d, &self.tz),
            next.and_then(|n| Self::midnight_ms(n, &self.tz)),
        ) {
            if lo < hi {
                self.days.insert(lo, (hi, d));
                self.last = Some((lo, hi, d));
            }
        }
        Some(d)
    }
}

/// Group entries into period buckets and build rows. `canon` maps a local
/// date to its bucket's canonical date; `fmt` renders it into the period
/// key (once per bucket, not per entry).
/// Entries always group into daily accumulators first, then fold into the
/// report's period buckets — upstream builds every period report over daily
/// summaries (`load_kind_for_report` loads Daily for weekly/monthly too), so
/// cost boundaries land at days rather than the whole period.
fn group_by<C, F>(entries: &[UsageEntry], ctx: &Ctx, kind: Report, canon: C, fmt: F) -> Vec<Row>
where
    C: Fn(jiff::civil::Date) -> jiff::civil::Date,
    F: Fn(&jiff::civil::Date) -> String,
{
    let mut days = DayCache::new(ctx.tz.clone());
    let mut day_accs: FxHashMap<i64, (jiff::civil::Date, Acc)> = FxHashMap::default();
    let mut bad = Acc::default();
    for e in entries {
        let calc = ctx.pricing.entry_cost(e);
        let Some(d) = days.date(e.timestamp_ms) else {
            bad.add_costed(e, calc, ctx.merged, ctx.pricing);
            continue;
        };
        let dk = pack(d);
        day_accs
            .entry(dk)
            .or_insert_with(|| (d, Acc::default()))
            .1
            .add_costed(e, calc, ctx.merged, ctx.pricing);
    }
    let mut day_keys: Vec<i64> = day_accs.keys().copied().collect();
    day_keys.sort_unstable();
    let mut idx: FxHashMap<i64, u32> = FxHashMap::default();
    let mut accs: Vec<(jiff::civil::Date, Acc)> = Vec::new();
    for dk in day_keys {
        let (d, dacc) = &day_accs[&dk];
        let cd = canon(*d);
        let i = *idx.entry(pack(cd)).or_insert_with(|| {
            let i = accs.len() as u32;
            accs.push((cd, Acc::default()));
            i
        });
        fold_acc(&mut accs[i as usize].1, dacc, ctx);
    }
    accs.sort_unstable_by_key(|(d, _)| pack(*d));
    let mut rows = Vec::with_capacity(accs.len() + (bad.entry_count > 0) as usize);
    if bad.entry_count > 0 {
        // unconvertible timestamps bucketted under "" — sorts first
        rows.push(ctx.row(kind, String::new(), &bad));
    }
    rows.extend(accs.iter().map(|(d, a)| ctx.row(kind, fmt(d), a)));
    rows
}

/// The model order a daily row's `modelBreakdowns` emits — first-seen order
/// stable-sorted by the day's mode-resolved cost, descending.
fn day_model_order<'x>(
    order: &'x [String],
    models: &FxHashMap<String, ModelAgg>,
    cost: impl Fn(&'x str) -> f64,
) -> Vec<&'x str> {
    let mut v: Vec<(&'x str, f64)> = order.iter().map(|n| (n.as_str(), cost(n))).collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1));
    v.into_iter().map(|(n, _)| n).collect()
}

/// Merge one daily model aggregate into a period's — token fields and the
/// per-mode entry sums add verbatim, while `folded_cost` accumulates the
/// day's resolved row cost so period costs match upstream's daily fold.
fn fold_model(m: &mut ModelAgg, dm: &ModelAgg, cost: f64) {
    m.tokens += dm.tokens;
    m.long += dm.long;
    m.display_input += dm.display_input;
    m.total_sum += dm.total_sum;
    m.cost_count += dm.cost_count;
    m.is_fallback |= dm.is_fallback;
    m.any_missing_auto |= dm.any_missing_auto;
    m.any_missing_calc |= dm.any_missing_calc;
    m.display_cost += dm.display_cost;
    m.calc_cost += dm.calc_cost;
    m.auto_cost += dm.auto_cost;
    *m.folded_cost.get_or_insert(0.0) += cost;
}

/// Upstream `aggregate_summaries`: fold one daily accumulator into a period
/// bucket — tokens sum, `modelsUsed` unions in first-seen order, model
/// breakdowns merge at each daily row's cost-desc-sorted position, and costs
/// are the sum of the daily row costs (which is why codex's per-day bucket
/// pricing survives into monthly rows).
fn fold_acc(dst: &mut Acc, d: &Acc, ctx: &Ctx) {
    dst.tokens += d.tokens;
    dst.display_input += d.display_input;
    dst.total_sum += d.total_sum;
    dst.entry_count += d.entry_count;
    dst.cost_display += d.cost_display;
    dst.cost_calc += d.cost_calc;
    dst.cost_auto += d.cost_auto;
    // merged period costs sum slice-by-slice across all days (agent-major
    // arrival order upstream); single-agent reports fold day-major
    if !ctx.merged {
        *dst.folded_cost.get_or_insert(0.0) += ctx.acc_cost(d);
    }
    if dst.project.is_empty() && !d.project.is_empty() {
        dst.project = d.project.clone();
    }
    if dst.first_activity_ms == 0
        || (d.first_activity_ms != 0 && d.first_activity_ms < dst.first_activity_ms)
    {
        dst.first_activity_ms = d.first_activity_ms;
    }
    if d.last_activity_ms > dst.last_activity_ms {
        dst.last_activity_ms = d.last_activity_ms;
    }
    // modelsUsed unions in each day's first-seen order; breakdown positions
    // follow each day's cost-desc list — upstream tracks the two separately
    for name in &d.model_order {
        if !dst.models.contains_key(name.as_str()) {
            dst.model_order.push(name.clone());
        }
    }
    for name in day_model_order(&d.model_order, &d.models, |n| {
        ctx.cost_for(ctx.agent, n, &d.models[n]).0
    }) {
        let dm = &d.models[name];
        let cost = ctx.cost_for(ctx.agent, name, dm).0;
        if !dst.models.contains_key(name) {
            dst.breakdown_order.push(name.to_string());
        }
        fold_model(dst.models.entry(name.to_string()).or_default(), dm, cost);
    }
    for (id, da) in &d.agents {
        let a = dst.agents.entry(id).or_default();
        a.tokens += da.tokens;
        a.display_input += da.display_input;
        a.total_sum += da.total_sum;
        a.entry_count += da.entry_count;
        a.cost_display += da.cost_display;
        a.cost_calc += da.cost_calc;
        a.cost_auto += da.cost_auto;
        let slice_cost = ctx.agent_slice_cost(id, da);
        *a.folded_cost.get_or_insert(0.0) += slice_cost;
        if ctx.merged {
            dst.day_costs.entry(*id).or_default().push(slice_cost);
        }
        for name in &da.model_order {
            if !a.models.contains_key(name.as_str()) {
                a.model_order.push(name.clone());
            }
        }
        for name in day_model_order(&da.model_order, &da.models, |n| {
            ctx.cost_for(id, n, &da.models[n]).0
        }) {
            let dm = &da.models[name];
            let cost = ctx.cost_for(id, name, dm).0;
            if !a.models.contains_key(name) {
                a.breakdown_order.push(name.to_string());
            }
            fold_model(a.models.entry(name.to_string()).or_default(), dm, cost);
        }
    }
}

/// OpenCode session-aggregate entries carry upstream's `session:<id>` key —
/// they only belong in session reports (and there only unfiltered).
fn is_session_aggregate(e: &UsageEntry) -> bool {
    e.agent == "opencode" && e.dedup_key.starts_with("session:")
}

/// Group by session id; upstream's `sort_summaries` orders rows by the
/// period string — sessionId ascending.
fn group_session(entries: &[UsageEntry], ctx: &Ctx) -> Vec<Row> {
    // merged reports emit one row per (agent, sessionId) upstream — agents
    // sharing an id never merge into a single contributor row
    let mut accs: FxHashMap<(&str, &str), Acc> = FxHashMap::default();
    for e in entries {
        let calc = ctx.pricing.entry_cost(e);
        accs.entry((e.agent, e.session_id.as_ref()))
            .or_default()
            .add_costed(e, calc, ctx.merged, ctx.pricing);
    }
    let mut rows: Vec<Row> = accs
        .iter()
        .map(|((_, id), a)| ctx.session_row((*id).to_string(), a))
        .collect();
    if ctx.merged {
        let asc = ctx.order_explicit && ctx.order == crate::cli::Order::Asc;
        rows.sort_by(|a, b| {
            let cost_order = if asc {
                a.total_cost.total_cmp(&b.total_cost)
            } else {
                b.total_cost.total_cmp(&a.total_cost)
            };
            cost_order
                .then_with(|| a.period.cmp(&b.period))
                .then_with(|| a.agent.cmp(&b.agent))
        });
    } else {
        match ctx.agent {
            // claude's session report forces cost-desc; `--order` is ignored
            "claude" => rows.sort_by(|a, b| b.total_cost.total_cmp(&a.total_cost)),
            // codex emits its BTreeMap group order — session id asc,
            // `--order` is ignored
            "codex" => rows.sort_by(|a, b| a.period.cmp(&b.period)),
            _ => {
                if ctx.order == crate::cli::Order::Desc {
                    rows.sort_by(|a, b| b.period.cmp(&a.period));
                } else {
                    rows.sort_by(|a, b| a.period.cmp(&b.period));
                }
            }
        }
    }
    rows
}

/// Billing blocks: each block starts at the floor-hour of its first entry and
/// spans `block_hours`. An entry at or past the current block's end opens a new
/// block at its own floor-hour. A block is active while `now < end`.
pub fn blocks(
    entries: &[UsageEntry],
    block_hours: f64,
    now_ms: i64,
    pricing: &Pricing,
) -> Vec<Block> {
    let span = (block_hours * 3_600_000.0) as i64;
    let floor_hour = |ms: i64| ms - ms.rem_euclid(3_600_000);

    let mut sorted: Vec<&UsageEntry> = entries.iter().collect();
    sorted.sort_by_key(|e| e.timestamp_ms);

    let mut out: Vec<Block> = Vec::new();
    let mut cur: Option<Block> = None;
    for e in sorted {
        let open_new = match &cur {
            None => true,
            Some(b) => e.timestamp_ms >= b.end_ms,
        };
        if open_new {
            if let Some(b) = cur.take() {
                out.push(b);
            }
            let start = floor_hour(e.timestamp_ms);
            let end = start + span;
            cur = Some(Block {
                start_ms: start,
                end_ms: end,
                acc: Acc::default(),
                is_active: now_ms < end,
            });
        }
        let calc = pricing.entry_cost(e);
        cur.as_mut()
            .unwrap()
            .acc
            .add_costed(e, calc, false, pricing);
    }
    if let Some(b) = cur.take() {
        out.push(b);
    }
    out
}

/// Serialize a block into its report row (burn rate + projections computed
/// the way upstream does: rates over the first→last-entry span, projection
/// into the minutes remaining until block end).
pub fn block_row(b: &Block, ctx: &Ctx, token_limit: Option<&str>, now: i64) -> BlockRow {
    let cost = ctx.acc_cost(&b.acc);
    let span_min = ((b.acc.last_activity_ms - b.acc.first_activity_ms) as f64 / 60_000.0).max(1.0);
    let tokens_per_min = b.acc.total() as f64 / span_min;
    let indicator_per_min = (b.acc.tokens.input + b.acc.tokens.output) as f64 / span_min;
    let cost_per_hour = cost / (span_min / 60.0);
    let remaining_min = ((b.end_ms - now) as f64 / 60_000.0).round().max(0.0);
    let burn = b.is_active.then(|| {
        serde_json::json!({
            "costPerHour": cost_per_hour,
            "tokensPerMinute": tokens_per_min,
            "tokensPerMinuteForIndicator": indicator_per_min,
        })
    });
    let projection = b.is_active.then(|| {
        serde_json::json!({
            "remainingMinutes": remaining_min as u64,
            "totalCost": ((cost + cost_per_hour * remaining_min / 60.0) * 100.0).round() / 100.0,
            "totalTokens": (b.acc.total() as f64 + tokens_per_min * remaining_min).round() as u64,
        })
    });
    let limit_proj = token_limit.and_then(|t| {
        let limit: u64 = if t == "max" {
            b.acc.total()
        } else {
            t.replace(['_', ','], "").parse().ok()?
        };
        let projected = (b.acc.total() as f64 + tokens_per_min * remaining_min).round() as u64;
        Some(serde_json::json!({
            "limit": limit,
            "projectedUsage": projected,
            "percentUsed": if limit > 0 { b.acc.total() as f64 / limit as f64 * 100.0 } else { 0.0 },
            "projectedPercent": if limit > 0 { projected as f64 / limit as f64 * 100.0 } else { 0.0 },
        }))
    });
    BlockRow {
        actual_end_time: Some(ms_to_iso(b.acc.last_activity_ms)),
        burn_rate: burn,
        cost_usd: cost,
        end_time: ms_to_iso(b.end_ms),
        entries: b.acc.entry_count,
        id: ms_to_iso(b.start_ms),
        is_active: b.is_active,
        is_gap: false,
        models: b.acc.model_order.clone(),
        projection,
        start_time: ms_to_iso(b.start_ms),
        token_counts: BlockTokens {
            input_tokens: b.acc.tokens.input,
            output_tokens: b.acc.tokens.output,
            cache_creation_input_tokens: b.acc.tokens.cache_creation,
            cache_read_input_tokens: b.acc.tokens.cache_read,
        },
        total_tokens: b.acc.tokens.total_no_reasoning(),
        usage_limit_projection: limit_proj,
    }
}

/// --since date → ms at local midnight; --until → end of day (inclusive).
pub fn filter_since(s: &str, tz: &jiff::tz::TimeZone) -> Option<i64> {
    filter_date(s, tz, false)
}

pub fn filter_until(s: &str, tz: &jiff::tz::TimeZone) -> Option<i64> {
    filter_date(s, tz, true)
}

fn filter_date(s: &str, tz: &jiff::tz::TimeZone, end_of_day: bool) -> Option<i64> {
    let clean = s.trim().replace('-', "");
    if clean.len() != 8 || !clean.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let d = format!("{}-{}-{}", &clean[0..4], &clean[4..6], &clean[6..8]);
    let date = d.parse::<jiff::civil::Date>().ok()?;
    let time = if end_of_day {
        jiff::civil::Time::constant(23, 59, 59, 999_000_000)
    } else {
        jiff::civil::Time::midnight()
    };
    date.to_datetime(time)
        .to_zoned(tz.clone())
        .ok()
        .map(|z| z.timestamp().as_millisecond())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600_000;

    fn entry_at(ms: i64) -> UsageEntry {
        UsageEntry {
            agent: "claude",
            timestamp_ms: ms,
            session_id: "s".into(),
            model: "m".into(),
            provider: "".into(),
            tokens: Tokens {
                input: 1,
                ..Default::default()
            },
            cost_usd: None,
            dedup_key: "".into(),
            dedup_id: 0,
            total_override: None,
            is_fallback: false,
            project: "".into(),
        }
    }

    #[test]
    fn blocks_start_at_floor_hour_and_roll() {
        // 13:40 and 14:20 fit one 5h block starting 13:00; 19:00 overflows
        // into a new block at its own floor hour (19:00).
        let t13_40 = 13 * HOUR + 40 * 60_000;
        let t14_20 = 14 * HOUR + 20 * 60_000;
        let t19_00 = 19 * HOUR;
        let p = Pricing::default();
        let bl = blocks(
            &[entry_at(t14_20), entry_at(t13_40), entry_at(t19_00)],
            5.0,
            15 * HOUR,
            &p,
        );
        assert_eq!(bl.len(), 2);
        assert_eq!(bl[0].start_ms, 13 * HOUR);
        assert_eq!(bl[0].end_ms, 18 * HOUR);
        assert_eq!(bl[0].acc.entry_count, 2);
        assert_eq!(bl[1].start_ms, 19 * HOUR);
        assert!(bl[0].is_active); // 15:00 < 18:00
                                  // upstream's rule is now < end; a block starting in the future
                                  // counts as active
        assert!(bl[1].is_active);
    }

    #[test]
    fn week_key_respects_start_of_week() {
        // 2026-11-11 is a Wednesday
        let wed = jiff::civil::date(2026, 11, 11);
        // monday-start week containing a Wednesday begins Monday 11-09
        assert_eq!(week_start(wed, 1), jiff::civil::date(2026, 11, 9));
        // sunday-start begins Sunday 11-08
        assert_eq!(week_start(wed, 0), jiff::civil::date(2026, 11, 8));
        let sun = jiff::civil::date(2026, 11, 8);
        // a sunday input belongs to that same sunday week
        assert_eq!(week_start(sun, 0), sun);
        // but to the previous monday week (which ended saturday)
        assert_eq!(week_start(sun, 1), jiff::civil::date(2026, 11, 2));
    }

    #[test]
    fn filter_dates_parse_both_forms() {
        let tz = jiff::tz::TimeZone::UTC;
        let a = filter_since("2026-11-01", &tz).unwrap();
        let b = filter_since("20261101", &tz).unwrap();
        assert_eq!(a, b);
        // until is inclusive end-of-day
        let u = filter_until("2026-11-01", &tz).unwrap();
        assert_eq!(u - a, 86_399_999);
        assert!(filter_since("garbage", &tz).is_none());
        assert!(filter_since("2026-13-99", &tz).is_none());
    }

    #[test]
    fn monthly_rows_bucket_by_year_month() {
        let tz = jiff::tz::TimeZone::UTC;
        let ms = jiff::civil::date(2026, 3, 15)
            .at(12, 0, 0, 0)
            .to_zoned(tz.clone())
            .unwrap()
            .timestamp()
            .as_millisecond();
        let p = Pricing::default();
        let ctx = Ctx::new(&p, CostMode::Auto, tz, false, false, 1, false, crate::cli::Order::Asc, false, "");
        let rows = Report::Monthly.rows(&[entry_at(ms)], &ctx);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].period, "2026-03");
    }
}

#[cfg(test)]
mod last_tests {
    use super::*;

    #[test]
    fn last_since_is_period_window() {
        let tz = jiff::tz::TimeZone::UTC;
        let p = crate::pricing::Pricing::default();
        let ctx = Ctx::new(&p, CostMode::Auto, tz.clone(), false, false, 1, false, crate::cli::Order::Asc, false, "");
        let zoned = jiff::Timestamp::now().to_zoned(tz.clone());
        let today = zoned.date();
        let midnight = today
            .to_datetime(jiff::civil::Time::midnight())
            .to_zoned(tz.clone())
            .unwrap()
            .timestamp()
            .as_millisecond();
        // --last 1 daily => today; --last 2 => yesterday
        assert_eq!(last_since(&Report::Daily, 1, &ctx), midnight);
        assert_eq!(last_since(&Report::Daily, 2, &ctx), midnight - 86_400_000);
        // --last 1 monthly => first of this month
        let first = jiff::civil::date(today.year(), today.month(), 1)
            .to_datetime(jiff::civil::Time::midnight())
            .to_zoned(tz.clone())
            .unwrap()
            .timestamp()
            .as_millisecond();
        assert_eq!(last_since(&Report::Monthly, 1, &ctx), first);
        // weekly (sow=monday) aligns to the most recent monday
        let idx = i64::from(today.weekday().to_sunday_zero_offset());
        let monday = midnight - (idx - 1).rem_euclid(7) * 86_400_000;
        assert_eq!(last_since(&Report::Weekly, 1, &ctx), monday);
        assert_eq!(
            last_since(&Report::Weekly, 2, &ctx),
            monday - 7 * 86_400_000
        );
    }
}
