use std::collections::BTreeMap;

/// One billable event: a single model request / turn with token usage.
#[derive(Debug, Clone)]
pub struct UsageEntry {
    /// agent id: "claude", "codex", "opencode", "devin", ...
    pub agent: &'static str,
    /// unix epoch millis (UTC)
    pub timestamp_ms: i64,
    /// session/conversation identifier
    pub session_id: String,
    /// model name as reported by the harness
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    /// cost reported by the harness itself, if any (display mode)
    pub cost_usd: Option<f64>,
    /// dedup key (e.g. messageId:requestId); entries with identical keys are skipped
    pub dedup_key: String,
    /// working directory / project label, best effort
    pub project: String,
}

impl UsageEntry {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.reasoning + self.cache_creation + self.cache_read
    }
}

/// Per-model aggregate inside a report row.
#[derive(Debug, Clone, Default)]
pub struct ModelAgg {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
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
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub display_cost: f64,
    pub calc_cost: f64,
    pub auto_cost: f64,
    pub cost_count: u64,
    /// model -> ModelAgg
    pub models: BTreeMap<String, ModelAgg>,
    /// first-seen model order (ccusage keeps insertion order in modelsUsed)
    pub model_order: Vec<String>,
    /// agent -> token totals
    pub agents: BTreeMap<&'static str, Acc>,
    pub project: String,
    pub last_activity_ms: i64,
    pub first_activity_ms: i64,
    pub entry_count: u64,
}

impl Acc {
    pub fn add(&mut self, e: &UsageEntry, pricing: &crate::pricing::Pricing) {
        let calc = pricing.entry_cost(e);
        self.input += e.input;
        self.output += e.output;
        self.reasoning += e.reasoning;
        self.cache_creation += e.cache_creation;
        self.cache_read += e.cache_read;
        self.entry_count += 1;
        if let Some(c) = e.cost_usd {
            self.display_cost += c;
            self.cost_count += 1;
        }
        if let Some(c) = calc {
            self.calc_cost += c;
        }
        self.auto_cost += e.cost_usd.or(calc).unwrap_or(0.0);
        if !self.models.contains_key(&e.model) {
            self.model_order.push(e.model.clone());
        }
        let m = self.models.entry(e.model.clone()).or_default();
        m.input += e.input;
        m.output += e.output;
        m.reasoning += e.reasoning;
        m.cache_creation += e.cache_creation;
        m.cache_read += e.cache_read;
        if let Some(c) = e.cost_usd {
            m.display_cost += c;
            m.cost_count += 1;
        }
        if let Some(c) = calc {
            m.calc_cost += c;
        }
        m.auto_cost += e.cost_usd.or(calc).unwrap_or(0.0);
        let a = self.agents.entry(e.agent).or_default();
        a.input += e.input;
        a.output += e.output;
        a.reasoning += e.reasoning;
        a.cache_creation += e.cache_creation;
        a.cache_read += e.cache_read;
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
        // don't recurse into per-agent models for the nested acc
        let m2 = a.models.entry(e.model.clone()).or_default();
        m2.input += e.input;
        m2.output += e.output;
        m2.reasoning += e.reasoning;
        m2.cache_creation += e.cache_creation;
        m2.cache_read += e.cache_read;
        if let Some(c) = e.cost_usd {
            a.display_cost += c;
            m2.display_cost += c;
        }
        if let Some(c) = calc {
            a.calc_cost += c;
            m2.calc_cost += c;
        }
        a.auto_cost += e.cost_usd.or(calc).unwrap_or(0.0);
        m2.auto_cost += e.cost_usd.or(calc).unwrap_or(0.0);
    }

    pub fn total(&self) -> u64 {
        self.input + self.output + self.reasoning + self.cache_creation + self.cache_read
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_breakdowns: Option<Vec<AgentRow>>,
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

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
    pub total_cost: f64,
}

/// A 5-hour (configurable) billing block.
#[derive(Debug, Clone)]
pub struct Block {
    pub start_ms: i64,
    pub end_ms: i64,
    pub acc: Acc,
    pub is_active: bool,
    pub is_gap: bool,
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
    pub token_counts: serde_json::Value,
    pub total_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_limit_projection: Option<serde_json::Value>,
}
