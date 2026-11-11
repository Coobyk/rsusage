//! Command orchestration: pick readers, load+filter entries, render reports.
use crate::agents::{self, AgentReader};
use crate::cli::{
    AgentCli, AgentCmd, BlocksArgs, FilterArgs, Order, ReportArgs, StatusArgs, Weekday,
};
use crate::live;
use crate::pricing::Pricing;
use crate::render;
use crate::report::{self, Ctx, Report};
use crate::statusline;
use crate::types::*;
use clap::Parser;
use rayon::prelude::*;

fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
    if std::env::var_os("RSUSAGE_TIMING").is_none() {
        return f();
    }
    let t = std::time::Instant::now();
    let r = f();
    eprintln!("{label}: {:?}", t.elapsed());
    r
}

/// Dispatch an agent-scoped invocation like `rsusage opencode daily --json`.
pub fn run_agent(argv: &[String]) -> anyhow::Result<()> {
    let (id, rest) = argv.split_first().unwrap();
    let Some(reader) = agents::by_id(id) else {
        anyhow::bail!(
            "unknown agent '{id}' (known: {})",
            agents::agent_ids().join(", ")
        );
    };
    let agent_id = reader.id();
    let parsed =
        AgentCli::try_parse_from(std::iter::once(agent_id.to_string()).chain(rest.iter().cloned()));
    let cli = match parsed {
        Ok(c) => c,
        Err(e) => e.exit(),
    };
    match &cli.cmd {
        None => run_report(Report::Daily, &cli.default, Some(agent_id)),
        Some(AgentCmd::Daily(a)) => run_report(Report::Daily, a, Some(agent_id)),
        Some(AgentCmd::Weekly(a)) => run_report(Report::Weekly, a, Some(agent_id)),
        Some(AgentCmd::Monthly(a)) => run_report(Report::Monthly, a, Some(agent_id)),
        Some(AgentCmd::Session(a)) => run_report(Report::Session, a, Some(agent_id)),
        Some(AgentCmd::Blocks(b)) => run_blocks(b, Some(agent_id)),
        Some(AgentCmd::Statusline(s)) => run_statusline(s, Some(agent_id)),
    }
}

fn tz_of(name: &Option<String>) -> jiff::tz::TimeZone {
    match name.as_deref() {
        Some(n) => match jiff::tz::TimeZone::get(n) {
            Ok(t) => t,
            Err(_) => {
                eprintln!("warning: unknown timezone '{n}', using system timezone");
                jiff::tz::TimeZone::system()
            }
        },
        None => jiff::tz::TimeZone::system(),
    }
}

fn readers_for(agent_id: Option<&'static str>) -> Vec<Box<dyn AgentReader>> {
    match agent_id {
        Some(id) => agents::by_id(id).into_iter().collect(),
        None => agents::all_agents()
            .into_iter()
            .filter(|a| a.detect())
            .collect(),
    }
}

/// Load entries across readers in parallel, dedup globally.
pub(crate) fn load_entries(readers: &[Box<dyn AgentReader>]) -> Vec<UsageEntry> {
    let loaded: Vec<Vec<UsageEntry>> = readers
        .par_iter()
        .map(|r| timed(&format!("reader:{}", r.id()), || r.entries()))
        .collect();
    agents::dedup(loaded.into_iter().flatten().collect())
}

/// Display names of the agents that produced entries (for "Detected:" banners).
pub(crate) fn detected_names(entries: &[UsageEntry]) -> Vec<&'static str> {
    let mut seen = rustc_hash::FxHashSet::default();
    let mut names: Vec<&'static str> = entries
        .iter()
        .filter(|e| seen.insert(e.agent))
        .filter_map(|e| agents::display_for(e.agent))
        .collect();
    names.sort_unstable();
    names
}

/// The one `Ctx` construction — mode/sow come from the args, `merged` from
/// whether the invocation is agent-scoped.
fn ctx_for<'a>(
    pricing: &'a Pricing,
    filter: &FilterArgs,
    tz: jiff::tz::TimeZone,
    agent_id: Option<&'static str>,
    sow: Option<Weekday>,
) -> Ctx<'a> {
    // upstream default: claude weeks start Sunday (billing), all else Monday
    let sow = sow
        .map(Weekday::index)
        .unwrap_or(if agent_id == Some("claude") { 0 } else { 1 });
    Ctx::new(
        pricing,
        filter.mode,
        tz,
        agent_id.is_none(),
        filter.no_cost,
        sow,
    )
}

/// --since/--until/--project filtering, shared by reports, blocks and live.
pub(crate) fn apply_filters(
    entries: &mut Vec<UsageEntry>,
    f: &FilterArgs,
    tz: &jiff::tz::TimeZone,
) {
    let parse = |flag: &str, s: &Option<String>, end: bool| {
        s.as_deref().and_then(|v| {
            let ms = if end {
                report::filter_until(v, tz)
            } else {
                report::filter_since(v, tz)
            };
            if ms.is_none() {
                eprintln!("warning: ignoring unparseable {flag} '{v}'");
            }
            ms
        })
    };
    let since_ms = parse("--since", &f.since, false);
    let until_ms = parse("--until", &f.until, true);
    entries.retain(|e| {
        since_ms.is_none_or(|s| e.timestamp_ms >= s) && until_ms.is_none_or(|u| e.timestamp_ms <= u)
    });
    if let Some(p) = &f.project {
        entries.retain(|e| e.project.contains(p.as_str()));
    }
}

/// Load + filter entries shared by period reports and blocks. Returns None
/// when nothing is usable (message already printed).
fn prepare(
    readers: &[Box<dyn AgentReader>],
    agent_id: Option<&'static str>,
    f: &FilterArgs,
    tz: &jiff::tz::TimeZone,
) -> Option<Vec<UsageEntry>> {
    let mut entries = timed("load_entries", || load_entries(readers));
    timed("filters", || apply_filters(&mut entries, f, tz));
    if entries.is_empty() {
        match agent_id {
            Some(_) => println!("No {} usage data found.", readers[0].display()),
            None => println!("No usage data found."),
        }
        return None;
    }
    Some(entries)
}

/// Signature of the pricing catalogs on disk — the same metadata the
/// binary pricing cache validates against, so it tracks whatever
/// `Pricing::load` resolved (including a just-fetched refresh).
fn pricing_sig() -> String {
    let dir = crate::paths::xdg_cache();
    ["modelsdev_prices.json", "litellm_prices.json", "prices-bin-v1.cache"]
        .iter()
        .map(|f| {
            std::fs::metadata(dir.join(f))
                .ok()
                .map(|m| {
                    let ns = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    format!("{}:{ns}", m.len())
                })
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Hash everything a report's bytes depend on: the command+args, the
/// agent scope, the calendar bucket, the pricing catalogs and every
/// reader's source signature. `None` when any reader can't sign its
/// sources — unsigned sources make memoization unsafe.
fn out_key(
    tag: &str,
    args_dbg: String,
    agent_id: Option<&'static str>,
    readers: &[Box<dyn AgentReader>],
    bucket: &str,
) -> Option<u64> {
    let sigs: Option<Vec<String>> = readers.par_iter().map(|r| r.cache_key()).collect();
    let mut h = rustc_hash::FxHasher::default();
    use std::hash::Hasher;
    h.write(tag.as_bytes());
    h.write(args_dbg.as_bytes());
    h.write(agent_id.unwrap_or("").as_bytes());
    h.write(bucket.as_bytes());
    h.write(pricing_sig().as_bytes());
    h.write(std::env::var("TZ").unwrap_or_default().as_bytes());
    for (r, s) in readers.iter().zip(sigs?) {
        h.write(r.id().as_bytes());
        h.write(s.as_bytes());
    }
    Some(h.finish())
}

/// Replay a previous run's exact bytes when every input signature still
/// matches — the memo sits ABOVE the entry cache, so a hit skips
/// decode+dedup+report+render entirely.
fn emit_cached(key: Option<u64>, render: impl FnOnce() -> String) {
    let path = key.map(|k| {
        crate::paths::xdg_cache()
            .join("out")
            .join(format!("{k:016x}.bin"))
    });
    if let Some(p) = &path {
        if let Ok(bytes) = std::fs::read(p) {
            print!("{}", String::from_utf8_lossy(&bytes));
            return;
        }
    }
    let s = render();
    if let Some(p) = path {
        let dir = p.parent().unwrap();
        if std::fs::create_dir_all(dir).is_ok() {
            let tmp = p.with_extension("tmp");
            if std::fs::write(&tmp, &s).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
            // bound the dir: keys change whenever a source does, so stale
            // files are never replayed — prune opportunistically on store
            let cutoff = std::time::SystemTime::now()
                - std::time::Duration::from_secs(24 * 3600);
            if let Ok(files) = std::fs::read_dir(dir) {
                for f in files.flatten() {
                    let stale = f
                        .metadata()
                        .and_then(|m| m.modified())
                        .is_ok_and(|t| t < cutoff);
                    if stale {
                        let _ = std::fs::remove_file(f.path());
                    }
                }
            }
        }
    }
    print!("{s}");
}

pub fn run_report(
    kind: Report,
    args: &ReportArgs,
    agent_id: Option<&'static str>,
) -> anyhow::Result<()> {
    let f = &args.filter;
    let offline = f.offline || f.no_cost;
    let readers = readers_for(agent_id);
    if readers.is_empty() {
        println!("No usage data found.");
        return Ok(());
    }
    let tz = tz_of(&f.timezone);
    let pricing = timed("pricing_load", || Pricing::load(offline));
    let day = jiff::Zoned::now()
        .with_time_zone(tz.clone())
        .date()
        .to_string();
    let key = out_key(
        &format!("{kind:?}"),
        format!("{args:?}"),
        agent_id,
        &readers,
        &day,
    );
    emit_cached(key, || {
        let Some(entries) = prepare(&readers, agent_id, f, &tz) else {
            return match agent_id {
                Some(_) => format!("No {} usage data found.\n", readers[0].display()),
                None => "No usage data found.\n".to_string(),
            };
        };
        let ctx = ctx_for(&pricing, f, tz.clone(), agent_id, args.start_of_week);
        timed("render", || render_report(kind, &entries, &ctx, args, agent_id))
    });
    Ok(())
}

fn title_of(kind: Report, agent_id: Option<&'static str>) -> String {
    match agent_id {
        Some(id) => format!(
            "{} Usage Report - {}",
            agents::display_for(id).unwrap_or(id),
            kind.label()
        ),
        None => format!("Coding (Agent) CLI Usage Report - {}", kind.label()),
    }
}

fn render_report(
    kind: Report,
    entries: &[UsageEntry],
    ctx: &Ctx,
    args: &ReportArgs,
    agent_id: Option<&'static str>,
) -> String {
    let f = &args.filter;
    let sections: Vec<Report> = match &args.sections {
        Some(list) => list
            .split(',')
            .filter_map(|name| {
                let r = Report::from_name(name);
                if r.is_none() {
                    eprintln!("warning: ignoring unknown section '{name}'");
                }
                r
            })
            .collect(),
        None => vec![kind],
    };
    let legacy = agent_id.is_some_and(agents::legacy_json);
    let detected = detected_names(entries);

    let mut json_out = serde_json::Map::new();
    let mut table_out = String::new();
    for (i, k) in sections.iter().enumerate() {
        // --last N keeps the most recent N periods as a date window
        // ("1 is today/this week/this month"), not a row slice
        let windowed;
        let slice = if let Some(n) = args.last {
            let since = report::last_since(k, n, ctx);
            windowed = entries
                .iter()
                .filter(|e| e.timestamp_ms >= since)
                .cloned()
                .collect::<Vec<_>>();
            &windowed
        } else {
            entries
        };
        let mut rows = timed("rows", || k.rows(slice, ctx));
        if f.order == Order::Desc {
            rows.reverse();
        }
        let totals = Totals::sum(&rows);
        if f.json {
            let name = if !ctx.merged && *k == Report::Session {
                "sessions"
            } else {
                k.name()
            };
            let rows_json = if ctx.merged {
                serde_json::json!(rows)
            } else {
                serde_json::json!(rows
                    .iter()
                    .map(|r| report::agent_row_json(r, *k, ctx, legacy))
                    .collect::<Vec<_>>())
            };
            json_out.insert(name.into(), rows_json);
            // upstream's totals shape depends on the schema: codex-legacy rows
            // get costUSD + reasoningOutputTokens (and exclude reasoning from
            // totalTokens); standard totals add unpricedModels when non-empty
            let totals_json = if legacy {
                let reasoning: u64 = rows.iter().map(|r| r.reasoning_tokens).sum();
                serde_json::json!({
                    "inputTokens": totals.input_tokens,
                    "outputTokens": totals.output_tokens,
                    "reasoningOutputTokens": reasoning,
                    "cacheReadTokens": totals.cache_read_tokens,
                    "cacheCreationTokens": totals.cache_creation_tokens,
                    "totalTokens": totals.input_tokens + totals.output_tokens
                        + totals.cache_creation_tokens + totals.cache_read_tokens,
                    "costUSD": totals.total_cost,
                })
            } else {
                let mut t = serde_json::to_value(&totals).unwrap();
                let mut unpriced: Vec<&str> = rows
                    .iter()
                    .flat_map(|r| r.model_breakdowns.iter())
                    .filter(|m| m.missing_pricing)
                    .map(|m| m.model_name.as_str())
                    .collect();
                unpriced.sort_unstable();
                unpriced.dedup();
                if !unpriced.is_empty() {
                    t.as_object_mut()
                        .unwrap()
                        .insert("unpricedModels".into(), unpriced.into());
                }
                t
            };
            let key = if sections.len() == 1 {
                "totals".to_string()
            } else {
                format!("{name}Totals")
            };
            json_out.insert(key, totals_json);
        } else {
            if i > 0 {
                table_out.push('\n');
            }
            table_out.push_str(
                &render::ReportTable {
                    title: title_of(*k, agent_id),
                    period_header: k.period_header().into(),
                    rows,
                    merged: ctx.merged,
                    no_cost: f.no_cost,
                    detected: detected.clone(),
                }
                .render(),
            );
        }
    }
    if f.json {
        format!(
            "{}\n",
            serde_json::to_string_pretty(&serde_json::Value::Object(json_out)).unwrap()
        )
    } else {
        table_out
    }
}

fn blocks_title(agent_id: Option<&'static str>, live: bool) -> &'static str {
    match (agent_id.is_some(), live) {
        (true, true) => "Session Blocks - Live",
        (false, true) => "Session Blocks (All Agents) - Live",
        _ => "Session Blocks",
    }
}

pub fn run_blocks(b: &BlocksArgs, agent_id: Option<&'static str>) -> anyhow::Result<()> {
    let f = &b.filter;
    let offline = f.offline || f.no_cost;
    let pricing_handle = std::thread::spawn(move || Pricing::load(offline));
    if b.live {
        let readers = readers_for(agent_id);
        if readers.is_empty() {
            println!("No usage data found.");
            return Ok(());
        }
        let tz = tz_of(&f.timezone);
        let pricing = pricing_handle.join().unwrap();
        let ctx = ctx_for(&pricing, f, tz.clone(), agent_id, None);
        let title = blocks_title(agent_id, true);
        return live::run(&readers, &ctx, b, title);
    }
    let readers = readers_for(agent_id);
    if readers.is_empty() {
        println!("No usage data found.");
        return Ok(());
    }
    let tz = tz_of(&f.timezone);
    let pricing = pricing_handle.join().unwrap();
    // blocks render now-derived fields (elapsed, projections) — bucket the
    // memo key at 60s so a replay is never more than a minute stale
    let now = jiff::Timestamp::now().as_millisecond();
    let minute = (now / 60_000).to_string();
    let key = out_key(
        "blocks",
        format!("{b:?}"),
        agent_id,
        &readers,
        &minute,
    );
    emit_cached(key, || {
        let Some(entries) = prepare(&readers, agent_id, f, &tz) else {
            return match agent_id {
                Some(_) => format!("No {} usage data found.\n", readers[0].display()),
                None => "No usage data found.\n".to_string(),
            };
        };
        let ctx = ctx_for(&pricing, f, tz.clone(), agent_id, None);
        let rows = block_rows(&entries, b, &ctx, now);
        if f.json {
            format!(
                "{}\n",
                serde_json::to_string_pretty(&serde_json::json!({ "blocks": rows })).unwrap()
            )
        } else if rows.is_empty() {
            "No usage blocks found.\n".to_string()
        } else {
            render::blocks_table(
                &rows,
                blocks_title(agent_id, false),
                ctx.merged,
                &detected_names(&entries),
            )
        }
    });
    Ok(())
}

/// Block pipeline shared by `blocks` and `blocks --live`: window into 5h-ish
/// blocks, apply --active/--recent/--order, serialize with burn-rate math.
pub(crate) fn block_rows(
    entries: &[UsageEntry],
    b: &BlocksArgs,
    ctx: &Ctx,
    now: i64,
) -> Vec<BlockRow> {
    let mut bl = report::blocks(entries, b.session_length, now, ctx.pricing);
    if b.active {
        bl.retain(|x| x.is_active);
    }
    if b.recent {
        let cutoff = now - 3 * 24 * 3_600_000;
        bl.retain(|x| x.end_ms >= cutoff);
    }
    if b.filter.order == Order::Desc {
        bl.reverse();
    }
    bl.iter()
        .map(|x| report::block_row(x, ctx, b.token_limit.as_deref(), now))
        .collect()
}

pub fn run_statusline(s: &StatusArgs, agent_id: Option<&'static str>) -> anyhow::Result<()> {
    let offline = s.offline;
    let pricing_handle = std::thread::spawn(move || Pricing::load(offline));
    let readers = readers_for(agent_id);
    let pricing = pricing_handle.join().unwrap();
    statusline::run(&readers, &pricing, &tz_of(&s.timezone), s.json);
    Ok(())
}
