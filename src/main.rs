mod agents;
mod cli;
mod live;
mod pricing;
mod render;
mod report;
mod statusline;
mod types;

use clap::Parser;
use cli::*;
use pricing::Pricing;
use report::*;
use types::*;

fn main() {
    let cli = Cli::parse();
    if let Err(e) = dispatch(&cli) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

enum Kind {
    Daily,
    Weekly,
    Monthly,
    Session,
    Blocks(BlocksArgs),
    Statusline(StatusArgs),
}

fn dispatch(cli: &Cli) -> anyhow::Result<()> {
    match &cli.cmd {
        None => run(Kind::Daily, &cli.default, None),
        Some(Cmd::Daily(a)) => run(Kind::Daily, a, None),
        Some(Cmd::Weekly(a)) => run(Kind::Weekly, a, None),
        Some(Cmd::Monthly(a)) => run(Kind::Monthly, a, None),
        Some(Cmd::Session(a)) => run(Kind::Session, a, None),
        Some(Cmd::Blocks(b)) => run(Kind::Blocks(b.clone()), &b.report, None),
        Some(Cmd::Statusline(s)) => run_statusline(s, None),
        Some(other) => {
            let (id, nested) = agent_parts(other);
            run_nested(id, nested)
        }
    }
}

fn agent_parts(cmd: &Cmd) -> (&'static str, &Nested) {
    match cmd {
        Cmd::Claude(n) => ("claude", n),
        Cmd::Codex(n) => ("codex", n),
        Cmd::Opencode(n) => ("opencode", n),
        Cmd::Kilo(n) => ("kilo", n),
        Cmd::Zcode(n) => ("zcode", n),
        Cmd::Devin(n) => ("devin", n),
        Cmd::Zed(n) => ("zed", n),
        Cmd::Cursor(n) => ("cursor", n),
        Cmd::Grok(n) => ("grok", n),
        Cmd::Amp(n) => ("amp", n),
        Cmd::Gemini(n) => ("gemini", n),
        Cmd::Qwen(n) => ("qwen", n),
        Cmd::Kimi(n) => ("kimi", n),
        Cmd::Goose(n) => ("goose", n),
        Cmd::Copilot(n) => ("copilot", n),
        Cmd::Droid(n) => ("droid", n),
        Cmd::Codebuff(n) => ("codebuff", n),
        Cmd::Hermes(n) => ("hermes", n),
        Cmd::Pi(n) => ("pi", n),
        Cmd::Openclaw(n) => ("openclaw", n),
        Cmd::Antigravity(n) => ("antigravity", n),
        _ => unreachable!(),
    }
}

fn run_nested(agent_id: &'static str, nested: &Nested) -> anyhow::Result<()> {
    let (kind, args) = match &nested.cmd {
        AgentCmd::Daily(a) => (Kind::Daily, a.clone()),
        AgentCmd::Weekly(a) => (Kind::Weekly, a.clone()),
        AgentCmd::Monthly(a) => (Kind::Monthly, a.clone()),
        AgentCmd::Session(a) => (Kind::Session, a.clone()),
        AgentCmd::Blocks(b) => (Kind::Blocks(b.clone()), b.report.clone()),
        AgentCmd::Statusline(s) => (Kind::Statusline(s.clone()), ReportArgs::default()),
    };
    run(kind, &args, Some(agent_id))
}

fn tz_of(name: &Option<String>) -> jiff::tz::TimeZone {
    name.as_deref()
        .and_then(|n| jiff::tz::TimeZone::get(n).ok())
        .unwrap_or_else(jiff::tz::TimeZone::system)
}

fn readers_for(agent_id: Option<&'static str>) -> Vec<Box<dyn agents::AgentReader>> {
    match agent_id {
        Some(id) => agents::by_id(id).into_iter().collect(),
        None => agents::all_agents()
            .into_iter()
            .filter(|a| a.detect())
            .collect(),
    }
}

fn run_statusline(s: &StatusArgs, agent_id: Option<&'static str>) -> anyhow::Result<()> {
    let tz = tz_of(&s.timezone);
    let readers = readers_for(agent_id);
    let pricing = Pricing::load(s.offline);
    statusline::run(&readers, &pricing, &tz, s.json);
    Ok(())
}

fn load_entries(readers: &[Box<dyn agents::AgentReader>]) -> (Vec<UsageEntry>, Vec<String>) {
    use rayon::prelude::*;
    let loaded: Vec<(String, Vec<UsageEntry>)> = readers
        .par_iter()
        .map(|r| (r.display().to_string(), r.entries()))
        .collect();
    let mut detected: Vec<String> = loaded
        .iter()
        .filter(|(_, e)| !e.is_empty())
        .map(|(d, _)| d.clone())
        .collect();
    detected.sort();
    let mut entries: Vec<UsageEntry> =
        loaded.into_iter().flat_map(|(_, e)| e).collect();
    entries = agents::dedup(entries);
    (entries, detected)
}

fn run(kind: Kind, args: &ReportArgs, agent_id: Option<&'static str>) -> anyhow::Result<()> {
    let tz = tz_of(&args.timezone);
    let readers = readers_for(agent_id);
    if readers.is_empty() {
        println!("No usage data found.");
        return Ok(());
    }

    if let Kind::Statusline(s) = &kind {
        return run_statusline(s, agent_id);
    }
    let dbg = std::env::var_os("RSUSAGE_DEBUG").is_some();
    let t0 = std::time::Instant::now();
    if let Kind::Blocks(b) = &kind {
        if b.report.live {
            let pricing = Pricing::load(b.report.offline);
            let title = if agent_id.is_some() {
                "Session Blocks - Live"
            } else {
                "Session Blocks (All Agents) - Live"
            };
            return live::run(&readers, &pricing, &tz, b, title);
        }
    }

    let (mut entries, detected) = load_entries(&readers);
    if dbg { eprintln!("load {} {:?}", entries.len(), t0.elapsed()); }

    let since_ms = args
        .since
        .as_deref()
        .and_then(|s| parse_filter_date(s, &tz, false));
    let until_ms = args
        .until
        .as_deref()
        .and_then(|s| parse_filter_date(s, &tz, true));
    entries.retain(|e| {
        since_ms.is_none_or(|s| e.timestamp_ms >= s)
            && until_ms.is_none_or(|u| e.timestamp_ms <= u)
    });
    if let Some(p) = &args.project {
        entries.retain(|e| e.project.contains(p.as_str()));
    }

    if entries.is_empty() {
        match agent_id {
            Some(_) => println!("No {} usage data found.", readers[0].display()),
            None => println!("No usage data found."),
        }
        return Ok(());
    }

    let pricing = Pricing::load(args.offline || args.no_cost);
    if dbg { eprintln!("pricing {:?}", t0.elapsed()); }
    let mode = match args.mode.as_str() {
        "calculate" => CostMode::Calculate,
        "display" => CostMode::Display,
        _ => CostMode::Auto,
    };
    let merged = agent_id.is_none();
    let ctx = Ctx {
        pricing: &pricing,
        mode,
        tz: tz.clone(),
        merged,
        by_agent: merged,
        no_cost: args.no_cost,
    };

    match kind {
        Kind::Blocks(b) => run_blocks(&entries, &ctx, &b, args, &detected),
        _ => run_period(kind, &entries, &ctx, args, &detected, merged, agent_id),
    }
    Ok(())
}

fn section_name(kind: &Kind) -> &'static str {
    match kind {
        Kind::Daily => "daily",
        Kind::Weekly => "weekly",
        Kind::Monthly => "monthly",
        Kind::Session => "session",
        _ => "daily",
    }
}

fn kind_label(kind: &Kind) -> &'static str {
    match kind {
        Kind::Daily => "Daily",
        Kind::Weekly => "Weekly",
        Kind::Monthly => "Monthly",
        Kind::Session => "Session",
        Kind::Blocks(_) => "Session Blocks",
        Kind::Statusline(_) => "Status",
    }
}

fn title_of(kind: &Kind, merged: bool, agent_id: Option<&'static str>) -> String {
    let label = kind_label(kind);
    match agent_id {
        Some(id) => {
            let disp = agents::by_id(id).map(|r| r.display().to_string()).unwrap_or_else(|| id.into());
            format!("{disp} Usage Report - {label}")
        }
        None => {
            let _ = merged;
            format!("Coding (Agent) CLI Usage Report - {label}")
        }
    }
}

fn run_period(
    kind: Kind,
    entries: &[UsageEntry],
    ctx: &Ctx,
    args: &ReportArgs,
    detected: &[String],
    merged: bool,
    agent_id: Option<&'static str>,
) {
    let sections: Vec<Kind> = match &args.sections {
        Some(list) => list
            .split(',')
            .filter_map(|s| match s.trim() {
                "daily" => Some(Kind::Daily),
                "weekly" => Some(Kind::Weekly),
                "monthly" => Some(Kind::Monthly),
                "session" => Some(Kind::Session),
                _ => None,
            })
            .collect(),
        None => vec![kind],
    };

    let mut json_out = serde_json::Map::new();
    let mut table_out = String::new();
    for (i, k) in sections.iter().enumerate() {
        let mut rows = match k {
            Kind::Daily => group_by(entries, ctx, date_key, false),
            Kind::Weekly => {
                let sow_default = if merged { "monday" } else { "sunday" };
                let sow = sow_index(
                    args.start_of_week.as_deref().unwrap_or(sow_default),
                );
                group_by(entries, ctx, |ms, tz| week_key_sow(ms, tz, sow), false)
            }
            Kind::Monthly => group_by(entries, ctx, month_key, false),
            Kind::Session => group_session(entries, ctx),
            _ => vec![],
        };
        if args.order == "desc" {
            rows.reverse();
        }
        if let Some(n) = args.last {
            let n = n as usize;
            if rows.len() > n {
                rows = rows.split_off(rows.len() - n);
            }
        }
        let totals = Totals {
            input_tokens: rows.iter().map(|r| r.input_tokens).sum(),
            output_tokens: rows.iter().map(|r| r.output_tokens).sum(),
            cache_creation_tokens: rows.iter().map(|r| r.cache_creation_tokens).sum(),
            cache_read_tokens: rows.iter().map(|r| r.cache_read_tokens).sum(),
            total_tokens: rows.iter().map(|r| r.total_tokens).sum(),
            total_cost: rows.iter().map(|r| r.total_cost).sum(),
        };
        if args.json {
            let name = section_name(k);
            let rows_json = if merged {
                serde_json::json!(rows)
            } else {
                let aid = agent_id.unwrap_or("");
                serde_json::json!(
                    rows.iter().map(|r| agent_row_json(r, k, aid)).collect::<Vec<_>>()
                )
            };
            // upstream uses "sessions" for agent-scoped, "session" for merged
            let name = if !merged && name == "session" { "sessions" } else { name };
            json_out.insert(name.into(), rows_json);
            if sections.len() == 1 {
                json_out.insert("totals".into(), serde_json::json!(totals));
            } else {
                json_out.insert(format!("{name}Totals"), serde_json::json!(totals));
            }
        } else {
            if i > 0 {
                table_out.push('\n');
            }
            table_out.push_str(&render::ReportTable {
                title: title_of(k, merged, if merged { None } else { agent_id_of(ctx) }),
                period_header: if matches!(k, Kind::Session) { "Session" } else { "Date" }.into(),
                rows,
                merged,
                no_cost: args.no_cost,
                detected: detected.to_vec(),
            }
            .render());
        }
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Object(json_out)).unwrap()
        );
    } else {
        print!("{table_out}");
    }
}

/// Agent-scoped row serialization. Upstream emits a per-agent schema:
///   all agents except codex: {date|week|month|sessionId, inputTokens,
///     outputTokens, cacheCreationTokens, cacheReadTokens, totalTokens,
///     totalCost, modelsUsed, modelBreakdowns[..., storedCost?]}
///   codex (legacy): {date, inputTokens, outputTokens, reasoningOutputTokens,
///     cacheReadTokens, cacheCreationTokens, totalTokens, costUSD, models:{}}
fn agent_row_json(r: &Row, kind: &Kind, agent_id: &str) -> serde_json::Value {
    use serde_json::json;
    if agent_id == "codex" {
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
                "cost": mb.cost,
            });
            if let Some(sc) = mb.stored_cost {
                v.as_object_mut().unwrap().insert("storedCost".into(), sc.into());
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
        Kind::Session => {
            o.insert("sessionId".into(), r.period.clone().into());
            o.insert("projectPath".into(), r.project.clone().into());
            if let Some(fa) = &r.first_activity {
                o.insert("firstActivity".into(), fa.clone().into());
            }
            if let Some(la) = &r.last_activity {
                o.insert("lastActivity".into(), la.clone().into());
            }
        }
        Kind::Weekly => {
            o.insert("week".into(), r.period.clone().into());
        }
        Kind::Monthly => {
            o.insert("month".into(), r.period.clone().into());
        }
        _ => {
            o.insert("date".into(), r.period.clone().into());
        }
    }
    o.insert("inputTokens".into(), r.input_tokens.into());
    o.insert("outputTokens".into(), r.output_tokens.into());
    o.insert("cacheCreationTokens".into(), r.cache_creation_tokens.into());
    o.insert("cacheReadTokens".into(), r.cache_read_tokens.into());
    o.insert("totalTokens".into(), r.total_tokens.into());
    o.insert("totalCost".into(), r.total_cost.into());
    o.insert("modelsUsed".into(), r.models_used.clone().into());
    o.insert("modelBreakdowns".into(), breakdowns.into());
    serde_json::Value::Object(o)
}

fn agent_id_of(_ctx: &Ctx) -> Option<&'static str> {
    None // display name resolved via ctx.merged only; kept for future per-agent titles
}

fn run_blocks(
    entries: &[UsageEntry],
    ctx: &Ctx,
    b: &BlocksArgs,
    args: &ReportArgs,
    detected: &[String],
) {
    let now = jiff::Timestamp::now().as_millisecond();
    let mut bl = blocks(entries, b.session_length, now, ctx.pricing, &ctx.tz);
    if b.active {
        bl.retain(|x| x.is_active);
    }
    if b.recent {
        let cutoff = now - 3 * 24 * 3_600_000;
        bl.retain(|x| x.end_ms >= cutoff);
    }
    if args.order == "desc" {
        bl.reverse();
    }
    let rows: Vec<BlockRow> = bl.iter().map(|x| block_row(x, ctx, b, now)).collect();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "blocks": rows })).unwrap()
        );
    } else if rows.is_empty() {
        println!("No usage blocks found.");
    } else {
        print!(
            "{}",
            render::blocks_table(&rows, "Session Blocks", if ctx.merged { detected } else { &[] })
        );
    }
}

pub fn block_row(b: &Block, ctx: &Ctx, args: &BlocksArgs, now: i64) -> BlockRow {
    let mut cost = 0.0;
    for (m, agg) in &b.acc.models {
        cost += ctx.cost(m, agg).0;
    }
    // upstream burn rate: entry span (last - first), not wall-clock elapsed
    let span_min = ((b.acc.last_activity_ms - b.acc.first_activity_ms) as f64 / 60_000.0)
        .max(1.0);
    let tokens_per_min = b.acc.total() as f64 / span_min;
    let indicator_per_min =
        (b.acc.input + b.acc.output) as f64 / span_min;
    let cost_per_hour = cost / (span_min / 60.0);
    let remaining_min = ((b.end_ms - now) as f64 / 60_000.0).round().max(0.0);
    let burn = (b.is_active && !b.is_gap).then(|| {
        serde_json::json!({
            "costPerHour": cost_per_hour,
            "tokensPerMinute": tokens_per_min,
            "tokensPerMinuteForIndicator": indicator_per_min,
        })
    });
    let projection = (b.is_active && !b.is_gap).then(|| {
        serde_json::json!({
            "remainingMinutes": remaining_min as u64,
            "totalCost": ((cost + cost_per_hour * remaining_min / 60.0) * 100.0).round() / 100.0,
            "totalTokens": (b.acc.total() as f64 + tokens_per_min * remaining_min).round() as u64,
        })
    });
    let limit_proj = args.token_limit.as_deref().and_then(|t| {
        let limit: u64 = if t == "max" {
            b.acc.total()
        } else {
            t.replace(['_', ','], "").parse().ok()?
        };
        let projected =
            (b.acc.total() as f64 + tokens_per_min * remaining_min).round() as u64;
        Some(serde_json::json!({
            "limit": limit,
            "projectedUsage": projected,
            "percentUsed": if limit > 0 { b.acc.total() as f64 / limit as f64 * 100.0 } else { 0.0 },
            "projectedPercent": if limit > 0 { projected as f64 / limit as f64 * 100.0 } else { 0.0 },
        }))
    });
    BlockRow {
        actual_end_time: (!b.is_gap).then(|| ms_to_iso(b.acc.last_activity_ms)),
        burn_rate: burn,
        cost_usd: cost,
        end_time: ms_to_iso(b.end_ms),
        entries: b.acc.entry_count,
        id: ms_to_iso(b.start_ms),
        is_active: b.is_active,
        is_gap: b.is_gap,
        models: b.acc.model_order.clone(),
        projection,
        start_time: ms_to_iso(b.start_ms),
        token_counts: serde_json::json!({
            "inputTokens": b.acc.input,
            "outputTokens": b.acc.output,
            "cacheCreationInputTokens": b.acc.cache_creation,
            "cacheReadInputTokens": b.acc.cache_read,
        }),
        total_tokens: b.acc.input + b.acc.output + b.acc.cache_creation + b.acc.cache_read,
        usage_limit_projection: limit_proj,
    }
}
