//! Aggregation: period bucketing, session grouping, 5h blocks, cost modes.

use crate::pricing::Pricing;
use crate::types::*;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Default, PartialEq, clap::ValueEnum)]
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
#[derive(Clone, Copy, PartialEq)]
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
        match self {
            Self::Daily => group_by(entries, ctx, date_key),
            Self::Weekly => group_by(entries, ctx, |ms, tz| week_key_sow(ms, tz, ctx.sow)),
            Self::Monthly => group_by(entries, ctx, month_key),
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
}

impl<'a> Ctx<'a> {
    pub fn new(
        pricing: &'a Pricing,
        mode: CostMode,
        tz: jiff::tz::TimeZone,
        merged: bool,
        no_cost: bool,
        sow: i64,
    ) -> Self {
        Self {
            pricing,
            mode,
            tz,
            merged,
            no_cost,
            sow,
        }
    }

    /// Cost for a model aggregate, honoring the mode (per-entry semantics).
    /// Upstream flags models whose catalog price is absent or all-zero (and
    /// which have no stored cost to fall back on) as missingPricing.
    pub fn cost(&self, model: &str, m: &ModelAgg) -> (f64, bool) {
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

    /// Total cost of a whole accumulator.
    pub fn acc_cost(&self, acc: &Acc) -> f64 {
        acc.models.iter().map(|(n, m)| self.cost(n, m).0).sum()
    }

    pub fn row(&self, period: String, acc: &Acc) -> Row {
        let mut breakdowns = vec![];
        let mut total_cost = 0.0;
        for (name, m) in &acc.models {
            let (cost, missing) = self.cost(name, m);
            total_cost += cost;
            breakdowns.push(ModelRow {
                model_name: name.clone(),
                input_tokens: m.tokens.input,
                output_tokens: m.tokens.output,
                cache_creation_tokens: m.tokens.cache_creation,
                cache_read_tokens: m.tokens.cache_read,
                cost,
                reasoning_tokens: m.tokens.reasoning,
                stored_cost: (m.cost_count > 0 && m.display_cost != 0.0 && !self.no_cost)
                    .then_some(m.display_cost),
                missing_pricing: missing,
            });
        }
        breakdowns.sort_by(|a, b| {
            b.cost
                .partial_cmp(&a.cost)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let agent_breakdowns = if self.merged && !acc.agents.is_empty() {
            let mut rows: Vec<AgentRow> = acc
                .agents
                .iter()
                .map(|(id, a)| {
                    let mut mu: Vec<String> = a.models.keys().cloned().collect();
                    mu.sort();
                    AgentRow {
                        agent: id.to_string(),
                        input_tokens: a.tokens.input,
                        output_tokens: a.tokens.output,
                        cache_creation_tokens: a.tokens.cache_creation,
                        cache_read_tokens: a.tokens.cache_read,
                        total_tokens: a.tokens.total(),
                        total_cost: a.models.iter().map(|(n, m)| self.cost(n, m).0).sum(),
                        models_used: mu,
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
            input_tokens: acc.tokens.input,
            output_tokens: acc.tokens.output,
            cache_creation_tokens: acc.tokens.cache_creation,
            cache_read_tokens: acc.tokens.cache_read,
            total_tokens: acc.tokens.total(),
            total_cost: if self.no_cost { 0.0 } else { total_cost },
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

    /// Session rows carry activity metadata plus, in merged reports, the
    /// single contributing agent when there is one (upstream behavior).
    pub fn session_row(&self, session_id: String, acc: &Acc) -> Row {
        let mut r = self.row(session_id, acc);
        r.first_activity = Some(ms_to_iso(acc.first_activity_ms));
        r.last_activity = Some(ms_to_iso(acc.last_activity_ms));
        if self.merged && acc.agents.len() == 1 {
            r.agent = acc.agents.keys().next().map(|a| a.to_string());
        }
        if acc.last_activity_ms > 0 {
            let md = r.metadata.get_or_insert_with(|| serde_json::json!({}));
            md.as_object_mut().unwrap().insert(
                "lastActivity".into(),
                serde_json::json!(ms_to_iso(acc.last_activity_ms)),
            );
        }
        r
    }
}

/// Agent-scoped row serialization. Upstream emits a per-agent schema:
///   default: {date|week|month|sessionId, inputTokens,
///     outputTokens, cacheCreationTokens, cacheReadTokens, totalTokens,
///     totalCost, modelsUsed, modelBreakdowns[..., storedCost?]}
///   legacy (codex): {date, inputTokens, outputTokens, reasoningOutputTokens,
///     cacheReadTokens, cacheCreationTokens, totalTokens, costUSD, models:{}}
pub fn agent_row_json(r: &Row, kind: Report, ctx: &Ctx, legacy: bool) -> serde_json::Value {
    use serde_json::json;
    if legacy {
        let models: serde_json::Map<String, serde_json::Value> = r
            .model_breakdowns
            .iter()
            .map(|mb| {
                (
                    mb.model_name.clone(),
                    json!({
                        "inputTokens": mb.input_tokens,
                        "outputTokens": mb.output_tokens,
                        "reasoningOutputTokens": mb.reasoning_tokens,
                        "cacheReadTokens": mb.cache_read_tokens,
                        "cacheCreationTokens": mb.cache_creation_tokens,
                        "totalTokens": mb.input_tokens + mb.output_tokens
                            + mb.cache_creation_tokens + mb.cache_read_tokens
                            + mb.reasoning_tokens,
                        "isFallback": true,
                    }),
                )
            })
            .collect();
        return json!({
            "date": r.period,
            "inputTokens": r.input_tokens,
            "outputTokens": r.output_tokens,
            "reasoningOutputTokens": r.reasoning_tokens,
            "cacheReadTokens": r.cache_read_tokens,
            "cacheCreationTokens": r.cache_creation_tokens,
            // upstream's codex row total excludes reasoning even though the
            // per-model totals above include it
            "totalTokens": r.input_tokens + r.output_tokens
                + r.cache_creation_tokens + r.cache_read_tokens,
            "costUSD": r.total_cost,
            "models": models,
        });
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
                "cost": if ctx.no_cost { 0.0 } else { mb.cost },
            });
            if let Some(sc) = mb.stored_cost {
                v.as_object_mut()
                    .unwrap()
                    .insert("storedCost".into(), sc.into());
            }
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
            o.insert("projectPath".into(), r.project.clone().into());
            if let Some(fa) = &r.first_activity {
                o.insert("firstActivity".into(), fa.clone().into());
            }
            if let Some(la) = &r.last_activity {
                o.insert("lastActivity".into(), la.clone().into());
            }
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
        (if ctx.no_cost { 0.0 } else { r.total_cost }).into(),
    );
    o.insert("modelsUsed".into(), r.models_used.clone().into());
    o.insert("modelBreakdowns".into(), breakdowns.into());
    serde_json::Value::Object(o)
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
            let back = (i64::from(d.weekday().to_sunday_zero_offset()) - sow).rem_euclid(7);
            d.checked_sub(jiff::SignedDuration::from_hours(24 * back))
                .map(|w| w.to_string())
                .unwrap_or_else(|_| d.to_string())
        })
        .unwrap_or_default()
}

pub fn month_key(ms: i64, tz: &jiff::tz::TimeZone) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| {
            let d = t.to_zoned(tz.clone()).date();
            format!("{}-{:02}", d.year(), d.month())
        })
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

/// Group entries into period buckets and build rows.
fn group_by<F>(entries: &[UsageEntry], ctx: &Ctx, key: F) -> Vec<Row>
where
    F: Fn(i64, &jiff::tz::TimeZone) -> String,
{
    let mut accs: BTreeMap<String, Acc> = BTreeMap::new();
    for e in entries {
        accs.entry(key(e.timestamp_ms, &ctx.tz))
            .or_default()
            .add(e, ctx.pricing);
    }
    accs.iter().map(|(k, a)| ctx.row(k.clone(), a)).collect()
}

/// Group by session id; upstream orders sessions by first activity.
fn group_session(entries: &[UsageEntry], ctx: &Ctx) -> Vec<Row> {
    let mut accs: std::collections::HashMap<String, Acc> = Default::default();
    for e in entries {
        accs.entry(e.session_id.clone())
            .or_default()
            .add(e, ctx.pricing);
    }
    let mut rows: Vec<(i64, Row)> = accs
        .iter()
        .map(|(k, a)| (a.first_activity_ms, ctx.session_row(k.clone(), a)))
        .collect();
    rows.sort_by_key(|(t, _)| *t);
    rows.into_iter().map(|(_, r)| r).collect()
}

/// Billing blocks: each block starts at the floor-hour of its first entry and
/// spans `block_hours`. An entry at or past the current block's end opens a new
/// block at its own floor-hour. A block is active while `now < end`.
pub fn blocks(
    entries: &[UsageEntry],
    block_hours: i64,
    now_ms: i64,
    pricing: &Pricing,
) -> Vec<Block> {
    let span = block_hours * 3_600_000;
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
        cur.as_mut().unwrap().acc.add(e, pricing);
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
            tokens: Tokens {
                input: 1,
                ..Default::default()
            },
            cost_usd: None,
            dedup_key: String::new(),
            project: String::new(),
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
            5,
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
        let tz = jiff::tz::TimeZone::UTC;
        let wed = jiff::civil::date(2026, 11, 11)
            .at(12, 0, 0, 0)
            .to_zoned(tz.clone())
            .unwrap()
            .timestamp()
            .as_millisecond();
        // monday-start week containing a Wednesday begins Monday 11-09
        assert_eq!(week_key_sow(wed, &tz, 1), "2026-11-09");
        // sunday-start begins Sunday 11-08
        assert_eq!(week_key_sow(wed, &tz, 0), "2026-11-08");
        // a sunday input belongs to that same sunday week
        let sun = jiff::civil::date(2026, 11, 8)
            .at(12, 0, 0, 0)
            .to_zoned(tz.clone())
            .unwrap()
            .timestamp()
            .as_millisecond();
        assert_eq!(week_key_sow(sun, &tz, 0), "2026-11-08");
        // but to the previous monday week (which ended saturday)
        assert_eq!(week_key_sow(sun, &tz, 1), "2026-11-02");
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
    fn month_key_is_year_month() {
        let tz = jiff::tz::TimeZone::UTC;
        let ms = jiff::civil::date(2026, 3, 15)
            .at(12, 0, 0, 0)
            .to_zoned(tz.clone())
            .unwrap()
            .timestamp()
            .as_millisecond();
        assert_eq!(month_key(ms, &tz), "2026-03");
    }
}

#[cfg(test)]
mod last_tests {
    use super::*;

    #[test]
    fn last_since_is_period_window() {
        let tz = jiff::tz::TimeZone::UTC;
        let p = crate::pricing::Pricing::default();
        let ctx = Ctx::new(&p, CostMode::Auto, tz.clone(), false, false, 1);
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
