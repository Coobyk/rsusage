//! Aggregation: period bucketing, session grouping, 5h blocks, cost modes.
use crate::pricing::Pricing;
use crate::types::*;
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq)]
pub enum CostMode {
    Auto,
    Calculate,
    Display,
}

pub struct Ctx<'a> {
    pub pricing: &'a Pricing,
    pub mode: CostMode,
    pub tz: jiff::tz::TimeZone,
    /// merged report over all agents
    pub merged: bool,
    pub by_agent: bool,
    pub no_cost: bool,
}

impl<'a> Ctx<'a> {
    /// Cost for a model aggregate, honoring the mode (per-entry semantics).
    pub fn cost(&self, model: &str, m: &ModelAgg) -> (f64, bool) {
        // upstream flags models whose catalog price is absent or all-zero
        // (and which have no stored cost to fall back on) as missingPricing
        let missing = match self.pricing.get(model) {
            None => m.display_cost == 0.0,
            Some(p) => p.is_free() && m.display_cost == 0.0,
        };
        let c = match self.mode {
            CostMode::Display => m.display_cost,
            CostMode::Calculate => m.calc_cost,
            CostMode::Auto => m.auto_cost,
        };
        (if self.no_cost { 0.0 } else { c }, missing)
    }

    pub fn row(&self, period: String, acc: &Acc, session_meta: bool) -> Row {
        let models_used = acc.model_order.clone();
        let _ = session_meta;
        let mut breakdowns = vec![];
        let mut total_cost = 0.0;
        for (name, m) in &acc.models {
            let (cost, missing) = self.cost(name, m);
            total_cost += cost;
            breakdowns.push(ModelRow {
                model_name: name.clone(),
                input_tokens: m.input,
                output_tokens: m.output,
                cache_creation_tokens: m.cache_creation,
                cache_read_tokens: m.cache_read,
                cost,
                reasoning_tokens: m.reasoning,
                stored_cost: (m.cost_count > 0 && m.display_cost != 0.0).then_some(m.display_cost),
                missing_pricing: missing,
            });
        }
        breakdowns.sort_by(|a, b| {
            b.cost
                .partial_cmp(&a.cost)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let agent_breakdowns = if self.merged && self.by_agent && !acc.agents.is_empty() {
            let mut rows: Vec<AgentRow> = acc
                .agents
                .iter()
                .map(|(id, a)| {
                    let mut c = 0.0;
                    let mut mu: Vec<String> = a.models.keys().cloned().collect();
                    mu.sort();
                    for (name, m) in &a.models {
                        c += self.cost(name, m).0;
                    }
                    AgentRow {
                        agent: id.to_string(),
                        input_tokens: a.input,
                        output_tokens: a.output,
                        cache_creation_tokens: a.cache_creation,
                        cache_read_tokens: a.cache_read,
                        total_tokens: a.total(),
                        total_cost: c,
                        models_used: mu,
                    }
                })
                .collect();
            rows.sort_by(|a, b| a.agent.cmp(&b.agent));
            Some(rows)
        } else {
            None
        };
        let agent_field = if self.merged {
            if session_meta && acc.agents.len() == 1 {
                acc.agents.keys().next().map(|a| a.to_string())
            } else {
                Some("all".into())
            }
        } else {
            None
        };
        let metadata = if self.merged || session_meta {
            let mut md = serde_json::Map::new();
            if self.merged {
                md.insert(
                    "agents".into(),
                    serde_json::json!(acc.agents.keys().copied().collect::<Vec<_>>()),
                );
            }
            if session_meta && acc.last_activity_ms > 0 {
                md.insert(
                    "lastActivity".into(),
                    serde_json::json!(ms_to_iso(acc.last_activity_ms)),
                );
            }
            if md.is_empty() {
                None
            } else {
                Some(serde_json::Value::Object(md))
            }
        } else {
            None
        };
        Row {
            agent: agent_field,
            period,
            input_tokens: acc.input,
            output_tokens: acc.output,
            cache_creation_tokens: acc.cache_creation,
            cache_read_tokens: acc.cache_read,
            total_tokens: acc.total(),
            total_cost: if self.no_cost { 0.0 } else { total_cost },
            reasoning_tokens: acc.reasoning,
            first_activity: session_meta
                .then(|| ms_to_iso(acc.first_activity_ms)),
            last_activity: session_meta.then(|| ms_to_iso(acc.last_activity_ms)),
            project: acc.project.clone(),
            models_used,
            model_breakdowns: breakdowns,
            metadata,
            agent_breakdowns,
        }
    }
}

/// ISO-8601 with millisecond precision, matching ccusage ("...T00:00:00.000Z").
pub fn ms_to_iso(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| {
            let s = t.to_string();
            if s.contains('.') {
                s
            } else {
                s.trim_end_matches('Z').to_string() + ".000Z"
            }
        })
        .unwrap_or_else(|_| String::new())
}

pub fn date_key(ms: i64, tz: &jiff::tz::TimeZone) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_zoned(tz.clone()).date().to_string())
        .unwrap_or_default()
}

/// Week-start key. `sow` = weekday index (Sunday=0 .. Saturday=6).
pub fn week_key_sow(ms: i64, tz: &jiff::tz::TimeZone, sow: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| {
            let d = t.to_zoned(tz.clone()).date();
            let idx = match d.weekday() {
                jiff::civil::Weekday::Sunday => 0,
                jiff::civil::Weekday::Monday => 1,
                jiff::civil::Weekday::Tuesday => 2,
                jiff::civil::Weekday::Wednesday => 3,
                jiff::civil::Weekday::Thursday => 4,
                jiff::civil::Weekday::Friday => 5,
                jiff::civil::Weekday::Saturday => 6,
            };
            let back = (idx - sow + 7) % 7;
            d.checked_sub(jiff::SignedDuration::from_hours(24 * back))
                .map(|w| w.to_string())
                .unwrap_or_else(|_| d.to_string())
        })
        .unwrap_or_default()
}

pub fn sow_index(name: &str) -> i64 {
    match name {
        "sunday" => 0,
        "monday" => 1,
        "tuesday" => 2,
        "wednesday" => 3,
        "thursday" => 4,
        "friday" => 5,
        "saturday" => 6,
        _ => 0,
    }
}

pub fn month_key(ms: i64, tz: &jiff::tz::TimeZone) -> String {
    let d = date_key(ms, tz);
    d.chars().take(7).collect()
}

/// Group entries into period buckets and build rows.
pub fn group_by<F>(entries: &[UsageEntry], ctx: &Ctx, key: F, session_meta: bool) -> Vec<Row>
where
    F: Fn(i64, &jiff::tz::TimeZone) -> String,
{
    let mut accs: BTreeMap<String, Acc> = BTreeMap::new();
    for e in entries {
        accs.entry(key(e.timestamp_ms, &ctx.tz))
            .or_default()
            .add(e, ctx.pricing);
    }
    accs.iter().map(|(k, a)| ctx.row(k.clone(), a, session_meta)).collect()
}

/// Group by session id, preserving first-seen (data) order like ccusage.
pub fn group_session(entries: &[UsageEntry], ctx: &Ctx) -> Vec<Row> {
    let mut order: Vec<String> = vec![];
    let mut accs: std::collections::HashMap<String, Acc> = Default::default();
    for e in entries {
        let a = accs.entry(e.session_id.clone()).or_insert_with(|| {
            order.push(e.session_id.clone());
            Acc::default()
        });
        a.add(e, ctx.pricing);
    }
    let mut rows: Vec<(i64, Row)> = order
        .iter()
        .map(|k| (accs[k].first_activity_ms, ctx.row(k.clone(), &accs[k], true)))
        .collect();
    // upstream orders sessions by first activity
    rows.sort_by_key(|(t, _)| *t);
    rows.into_iter().map(|(_, r)| r).collect()
}

/// Billing blocks: clock-aligned windows of `block_hours` (floored to span
/// boundaries like ccusage's 5h Claude blocks).
pub fn blocks(
    entries: &[UsageEntry],
    block_hours: i64,
    now_ms: i64,
    pricing: &Pricing,
    tz: &jiff::tz::TimeZone,
) -> Vec<Block> {
    let _ = tz;
    let span = block_hours * 3_600_000;
    let floor_hour = |ms: i64| ms - ms.rem_euclid(3_600_000);

    let mut sorted: Vec<&UsageEntry> = entries.iter().collect();
    sorted.sort_by_key(|e| e.timestamp_ms);

    // Upstream: a block starts at the floor-hour of its first entry and spans
    // `block_hours`. Entries past the block end open a new block at their own
    // floor-hour; disjoint windows get an is_gap block in between.
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
            cur = Some(Block {
                start_ms: start,
                end_ms: start + span,
                acc: Acc::default(),
                is_active: now_ms < start + span,
                is_gap: false,
            });
        }
        cur.as_mut().unwrap().acc.add(e, pricing);
    }
    if let Some(b) = cur.take() {
        out.push(b);
    }
    out
}

/// ms since/until dates (inclusive) from --since/--until "YYYYMMDD|YYYY-MM-DD".
pub fn parse_filter_date(s: &str, tz: &jiff::tz::TimeZone, end_of_day: bool) -> Option<i64> {
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
