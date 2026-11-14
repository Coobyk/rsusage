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
pub fn run_agent(argv: &[String], root: &ReportArgs) -> anyhow::Result<()> {
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
    // upstream `agent_report_supported`: weekly only exists for claude and
    // opencode; blocks/statusline are claude-only. Everything else exits 2
    // with the same three-line stderr (the upstream text names `ccusage`
    // verbatim)
    if let Some(report) = unsupported_report(agent_id, &cli.cmd) {
        let display = upstream_display(agent_id);
        if matches!(report, "blocks" | "statusline") {
            eprintln!(
                "The \"{report}\" report is only available for Claude Code usage.\nUse \"ccusage {agent_id} daily\" for {display} usage reports.\nRun 'ccusage --help' for usage."
            );
        } else {
            eprintln!(
                "The \"{report}\" report is not available for {display} usage.\nUse \"ccusage {agent_id} daily\" for {display} usage reports.\nRun 'ccusage --help' for usage."
            );
        }
        std::process::exit(2);
    }
    match &cli.cmd {
        None => run_report(Report::Daily, &root.merged(&cli.default), Some(agent_id)),
        Some(AgentCmd::Daily(a)) => run_report(Report::Daily, &root.merged(a), Some(agent_id)),
        Some(AgentCmd::Weekly(a)) => run_report(Report::Weekly, &root.merged(a), Some(agent_id)),
        Some(AgentCmd::Monthly(a)) => run_report(Report::Monthly, &root.merged(a), Some(agent_id)),
        Some(AgentCmd::Session(a)) => run_report(Report::Session, &root.merged(a), Some(agent_id)),
        Some(AgentCmd::Blocks(b)) => run_blocks(&b.merged(root), Some(agent_id)),
        Some(AgentCmd::Statusline(s)) => run_statusline(&s.merged(root), Some(agent_id)),
    }
}

/// `agent_report_supported` upstream: weekly for claude/opencode only,
/// blocks/statusline for claude only; devin/zed/cursor are rsusage
/// extensions so they keep the full set.
fn unsupported_report(agent_id: &str, cmd: &Option<AgentCmd>) -> Option<&'static str> {
    let ccu_known = !matches!(agent_id, "devin" | "zed" | "cursor");
    match cmd {
        Some(AgentCmd::Weekly(_)) if agent_id != "claude" && agent_id != "opencode" && ccu_known => {
            Some("weekly")
        }
        Some(AgentCmd::Blocks(_)) if agent_id != "claude" => Some("blocks"),
        Some(AgentCmd::Statusline(_)) if agent_id != "claude" => Some("statusline"),
        _ => None,
    }
}

/// Display names as upstream's `agent_display_name` table writes them into
/// error text — these differ from the report-banner names for a few agents.
fn upstream_display(agent_id: &str) -> &'static str {
    match agent_id {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "opencode" => "OpenCode",
        "amp" => "Amp",
        "droid" => "Droid",
        "codebuff" => "Codebuff",
        "hermes" => "Hermes",
        "pi" => "pi-agent",
        "goose" => "Goose",
        "kilo" => "Kilo",
        "copilot" => "GitHub Copilot CLI",
        "gemini" => "Gemini CLI",
        "antigravity" => "Antigravity",
        "kimi" => "Kimi",
        "qwen" => "Qwen",
        "openclaw" => "OpenClaw",
        "grok" => "Grok",
        "zcode" => "ZCode",
        "devin" => "Devin",
        "zed" => "Zed",
        "cursor" => "Cursor",
        _ => "Agent",
    }
}

/// Upstream validates `--timezone` at parse time: `local` means the system
/// zone, an unknown IANA name is a usage error (exit 2).
fn tz_of(name: &Option<String>) -> jiff::tz::TimeZone {
    match name.as_deref() {
        Some("local") | None => jiff::tz::TimeZone::system(),
        Some(n) => jiff::tz::TimeZone::get(n).unwrap_or_else(|_| {
            eprintln!(
                "Invalid value for --timezone '{n}'. Expected an IANA timezone name such as UTC or America/New_York, or 'local' for the system timezone."
            );
            eprintln!("Run 'ccusage --help' for usage.");
            std::process::exit(2);
        }),
    }
}

fn readers_for(agent_id: Option<&'static str>, all: bool) -> Vec<Box<dyn AgentReader>> {
    match agent_id {
        Some(id) => agents::by_id(id).into_iter().collect(),
        // merged reports cover exactly upstream's agent set — devin/zed/cursor
        // are rsusage extensions, reachable via `rsusage <agent>` or `--all`
        None => agents::all_agents()
            .into_iter()
            .filter(|a| {
                a.detect() && (all || !matches!(a.id(), "devin" | "zed" | "cursor"))
            })
            .collect(),
    }
}

/// Load entries across readers in parallel, dedup globally.
pub(crate) fn load_entries(readers: &[Box<dyn AgentReader>]) -> Vec<UsageEntry> {
    let loaded: Vec<Vec<UsageEntry>> = readers
        .par_iter()
        .map(|r| {
            let mut es = timed(&format!("reader:{}", r.id()), || r.entries());
            // upstream sorts inside each loader — every one but codex —
            // so a global sort would reshuffle codex's file order and
            // perturb f64 cost summation order in reports
            if r.id() != "codex" {
                es.sort_by_key(|e| e.timestamp_ms);
            }
            es
        })
        .collect();
    agents::dedup(loaded.into_iter().flatten().collect())
}

/// Display names of the agents that produced entries (for "Detected:" banners).
pub(crate) fn detected_names(entries: &[UsageEntry]) -> Vec<&'static str> {
    let mut ids: Vec<&'static str> = entries.iter().map(|e| e.agent).collect();
    ids.sort_unstable();
    ids.dedup();
    ids.iter()
        .filter_map(|id| agents::display_for(id))
        .collect()
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
        filter.since.is_some() || filter.until.is_some(),
        filter.order.unwrap_or_default(),
        filter.order.is_some(),
        agent_id.unwrap_or(""),
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

/// Load + filter entries shared by period reports and blocks; callers
/// decide how to report an empty result (message vs. empty JSON schema).
fn prepare(
    readers: &[Box<dyn AgentReader>],
    f: &FilterArgs,
    tz: &jiff::tz::TimeZone,
) -> (Vec<UsageEntry>, Vec<&'static str>) {
    let mut entries = timed("load_entries", || load_entries(readers));
    let mut detected: Vec<&'static str> = entries.iter().map(|e| e.agent).collect();
    detected.sort_unstable();
    detected.dedup();
    timed("filters", || apply_filters(&mut entries, f, tz));
    (entries, detected)
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
/// agent scope, the calendar bucket, the pricing catalogs, this binary's
/// build fingerprint and every reader's source signature. `None` when any
/// reader can't sign its sources — unsigned sources make memoization unsafe.
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
    // a rebuilt binary must never replay bytes rendered by an older one —
    // the entry cache versions its format separately, but this memo sits
    // above it
    if let Ok(meta) = std::env::current_exe().and_then(|p| p.metadata()) {
        h.write_u64(meta.len());
        if let Some(t) = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        {
            h.write_u64(t.as_nanos() as u64);
        }
    } else {
        return None;
    }
    h.write(pricing_sig().as_bytes());
    h.write(std::env::var("TZ").unwrap_or_default().as_bytes());
    for (r, s) in readers.iter().zip(sigs?) {
        h.write(r.id().as_bytes());
        h.write(s.as_bytes());
    }
    Some(h.finish())
}

/// Pipe a rendered JSON document through the `jq` binary — upstream shells
/// out rather than embedding a filter engine.
fn run_jq(json_text: &str, filter: &str) -> ! {
    let spawn = std::process::Command::new("jq")
        .arg(filter)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::inherit())
        .spawn();
    let mut child = match spawn {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: CliError({:?})", format!("failed to run jq: {e}"));
            std::process::exit(1);
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(json_text.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    match child.wait() {
        Ok(status) if status.success() => std::process::exit(0),
        _ => {
            eprintln!("Error: CliError(\"jq failed\")");
            std::process::exit(1);
        }
    }
}

/// Replay a previous run's exact bytes when every input signature still
/// matches — the memo sits ABOVE the entry cache, so a hit skips
/// decode+dedup+report+render entirely. `jq` pipes the rendered JSON through
/// the jq binary instead of printing it.
fn emit_cached(
    key: Option<u64>,
    jq: Option<&str>,
    render: impl FnOnce() -> Vec<(String, String)>,
) {
    let path = key.map(|k| {
        crate::paths::xdg_cache()
            .join("out")
            .join(format!("{k:016x}.bin"))
    });
    let finish = |pairs: Vec<(String, String)>| {
        // each pair's stderr follows its stdout, so multi-section reports
        // interleave warnings the way upstream prints them
        for (out, err) in pairs {
            if let Some(filter) = jq {
                eprint!("{err}");
                run_jq(&out, filter);
            }
            print!("{out}");
            eprint!("{err}");
        }
    };
    if let Some(p) = &path {
        if let Ok(bytes) = std::fs::read(p) {
            if let Ok(pairs) = bincode::deserialize::<Vec<(String, String)>>(&bytes) {
                finish(pairs);
                return;
            }
        }
    }
    let pairs = render();
    if let Some(p) = path {
        let dir = p.parent().unwrap();
        if std::fs::create_dir_all(dir).is_ok() {
            let blob = bincode::serialize(&pairs).unwrap_or_default();
            let tmp = p.with_extension("tmp");
            if std::fs::write(&tmp, &blob).is_ok() {
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
    finish(pairs);
}

pub fn run_report(
    kind: Report,
    args: &ReportArgs,
    agent_id: Option<&'static str>,
) -> anyhow::Result<()> {
    let f = &args.filter;
    // flag scoping and the --last/sections/date-window gates are enforced by
    // the cliparse pre-parser before dispatch
    if kind == Report::Session {
        if let Some(id) = &args.id {
            return run_session_detail(id, args, agent_id);
        }
    }
    // --no-cost only hides the cost — pricing and sorting still apply
    let offline = f.offline;
    let readers = readers_for(agent_id, f.all);
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
    emit_cached(key, f.jq.as_deref(), || {
        let (entries, detected) = prepare(&readers, f, &tz);
        let ctx = ctx_for(&pricing, f, tz.clone(), agent_id, args.start_of_week);
        timed("render", || {
            render_report(kind, &entries, &ctx, args, agent_id, &detected)
        })
    });
    Ok(())
}

/// `session --id`, `claude session -i`, `codex session -i`: single-session
/// detail — claude emits entry-level detail JSON or a titled summary table;
/// codex resolves partial ids and emits the group detail or a filtered
/// session table. Bare `session -i` is the claude path upstream.
fn run_session_detail(
    id: &str,
    args: &ReportArgs,
    agent_id: Option<&'static str>,
) -> anyhow::Result<()> {
    let f = &args.filter;
    let agent = agent_id.unwrap_or("claude");
    let wants_json = f.json || f.jq.is_some();
    let readers = readers_for(Some(agent), false);
    let tz = tz_of(&f.timezone);
    let pricing = timed("pricing_load", || Pricing::load(f.offline));
    let (entries, detected) = prepare(&readers, f, &tz);
    let ctx = ctx_for(&pricing, f, tz.clone(), Some(agent), args.start_of_week);

    if agent == "codex" {
        // upstream strips the transport prefix/suffixes, then exact or
        // basename or `-uuid` suffix match — exactly one session must win
        let raw = id.strip_prefix("codex://threads/").unwrap_or(id);
        let req = raw.strip_suffix(".zst").unwrap_or(raw);
        let req = req.strip_suffix(".jsonl").unwrap_or(req);
        let sessions: std::collections::BTreeSet<&str> =
            entries.iter().map(|e| e.session_id.as_ref()).collect();
        let matches: Vec<&str> = sessions
            .into_iter()
            .filter(|c| codex_session_id_matches(c, req))
            .collect();
        let sid = match matches.as_slice() {
            [s] => *s,
            [] => anyhow::bail!("No Codex session found with ID: {id}"),
            _ => anyhow::bail!(
                "Codex session ID '{id}' is ambiguous and matches {} sessions.",
                matches.len()
            ),
        };
        let filtered: Vec<UsageEntry> = entries
            .iter()
            .filter(|e| e.session_id.as_ref() == sid)
            .cloned()
            .collect();
        if wants_json {
            // detail = the codex session-group JSON with costUSD → totalCost
            let rows = Report::Session.rows(&filtered, &ctx);
            let mut value = report::agent_row_json(&rows[0], Report::Session, &ctx, true);
            if let Some(obj) = value.as_object_mut() {
                if let Some(cost) = obj.remove("costUSD") {
                    obj.insert("totalCost".into(), cost);
                }
            }
            if f.no_cost {
                strip_cost_fields(&mut value);
            }
            return emit_detail_json(value, f.jq.as_deref());
        }
        for (out, err) in render_report(
            Report::Session,
            &filtered,
            &ctx,
            args,
            Some("codex"),
            &detected,
        ) {
            print!("{out}");
            eprint!("{err}");
        }
        return Ok(());
    }

    // claude: exact session-id match over the loaded entries
    let mut session_entries: Vec<UsageEntry> = entries
        .iter()
        .filter(|e| e.session_id.as_ref() == id)
        .cloned()
        .collect();
    session_entries.sort_by_key(|e| e.timestamp_ms);
    if session_entries.is_empty() {
        if wants_json {
            println!("null");
        } else {
            eprintln!("No session found with ID: {id}");
        }
        return Ok(());
    }
    let rows = Report::Session.rows(&session_entries, &ctx);
    let row = &rows[0];
    if wants_json {
        let breakdowns: Vec<serde_json::Value> = row
            .model_breakdowns
            .iter()
            .map(|mb| {
                let mut v = serde_json::json!({
                    "modelName": mb.model_name,
                    "inputTokens": mb.input_tokens,
                    "outputTokens": mb.output_tokens,
                    "cacheCreationTokens": mb.cache_creation_tokens,
                    "cacheReadTokens": mb.cache_read_tokens,
                    "cost": mb.cost,
                });
                if mb.missing_pricing {
                    v["missingPricing"] = true.into();
                }
                v
            })
            .collect();
        let entries_json: Vec<serde_json::Value> = session_entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "timestamp": report::ms_to_iso(e.timestamp_ms),
                    "inputTokens": e.tokens.input,
                    "outputTokens": e.tokens.output,
                    "cacheCreationTokens": e.tokens.cache_creation,
                    "cacheReadTokens": e.tokens.cache_read,
                    "model": if e.model.is_empty() { "unknown" } else { e.model.as_ref() },
                    "costUSD": e.cost_usd.unwrap_or(0.0),
                })
            })
            .collect();
        let mut value = serde_json::json!({
            "sessionId": id,
            "totalCost": row.total_cost,
            "totalTokens": row.total_tokens,
            "modelsUsed": row.models_used,
            "modelBreakdowns": breakdowns,
            "entries": entries_json,
        });
        if f.no_cost {
            strip_cost_fields(&mut value);
        }
        emit_detail_json(value, f.jq.as_deref())
    } else {
        let title = format!("Claude Code Session Usage - {id}");
        let (out, err) = render::focused_table(&title, Report::Session, &rows, &ctx, f, true);
        print!("{out}");
        eprint!("{err}");
        Ok(())
    }
}

/// Pretty-print a detail JSON document, or pipe it through `jq`.
fn emit_detail_json(value: serde_json::Value, jq: Option<&str>) -> anyhow::Result<()> {
    let text = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
    match jq {
        Some(filter) => run_jq(&text, filter),
        None => {
            print!("{text}");
            Ok(())
        }
    }
}

/// Upstream codex session-id matching: the candidate (`.jsonl` stripped)
/// wins on exact match, basename match, or a `-{uuid}` suffix match.
fn codex_session_id_matches(candidate: &str, requested: &str) -> bool {
    let candidate = candidate.strip_suffix(".jsonl").unwrap_or(candidate);
    if candidate == requested
        || candidate
            .rsplit_once('/')
            .map_or(candidate, |(_, file)| file)
            == requested
    {
        return true;
    }
    let is_uuid = requested.len() == 36
        && requested.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        });
    is_uuid && candidate.strip_suffix(requested).is_some_and(|p| p.ends_with('-'))
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
    detected: &[&'static str],
) -> Vec<(String, String)> {
    let f = &args.filter;
    let sections: Vec<Report> = match &args.sections {
        // the invoked report renders first, then each extra section in
        // canonical report order — invalid values are a usage error
        Some(list) => {
            let mut extra = Vec::new();
            for name in list.split(',') {
                match Report::from_name(name) {
                    Some(r) => extra.push(r),
                    None => {
                        eprintln!(
                            "Invalid --sections value '{name}'. Expected one or more of: daily, weekly, monthly, session."
                        );
                        eprintln!("Run 'ccusage --help' for usage.");
                        std::process::exit(2);
                    }
                }
            }
            let mut v = vec![kind];
            for k in [
                Report::Daily,
                Report::Weekly,
                Report::Monthly,
                Report::Session,
            ] {
                if extra.contains(&k) && !v.contains(&k) {
                    v.push(k);
                }
            }
            v
        }
        None => vec![kind],
    };
    let legacy = agent_id.is_some_and(agents::legacy_json);

    let mut json_out: Vec<(String, serde_json::Value)> = Vec::new();
    // multi-section JSON carries a single `totals` built from the invoked
    // report's rows, emitted after every section array
    let mut totals_json = None;
    let mut pairs: Vec<(String, String)> = Vec::new();
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
        // session ordering is fully resolved inside `group_session` —
        // merged sorts cost-desc, claude forces cost-desc, codex keeps id
        // order and the generic adapters sort by period+order
        if f.order == Some(Order::Desc) && *k != Report::Session {
            rows.reverse();
        }
        let mut totals = Totals::sum(&rows);
        // upstream's empty report carries a negative-zero cost
        if rows.is_empty() {
            totals.total_cost = -0.0;
        }
        if f.json || f.jq.is_some() {
            let name = if !ctx.merged && *k == Report::Session {
                "sessions"
            } else {
                k.name()
            };
            let rows_json = if ctx.merged {
                serde_json::json!(rows
                    .iter()
                    .map(|r| report::merged_row_json(r, *k, f.by_agent))
                    .collect::<Vec<_>>())
            } else {
                serde_json::json!(rows
                    .iter()
                    .map(|r| report::agent_row_json(r, *k, ctx, legacy))
                    .collect::<Vec<_>>())
            };
            json_out.push((name.into(), rows_json));
            if *k != kind {
                continue;
            }
            // upstream's totals shape depends on the schema: codex-legacy rows
            // get costUSD + reasoningOutputTokens (and exclude reasoning from
            // totalTokens); standard totals add unpricedModels when non-empty
            let t = if legacy {
                let reasoning: u64 = rows.iter().map(|r| r.reasoning_tokens).sum();
                serde_json::json!({
                    "inputTokens": totals.input_tokens,
                    "outputTokens": totals.output_tokens,
                    "reasoningOutputTokens": reasoning,
                    "cacheReadTokens": totals.cache_read_tokens,
                    "cacheCreationTokens": totals.cache_creation_tokens,
                    "totalTokens": totals.total_tokens,
                    "costUSD": crate::types::json_float(totals.total_cost),
                })
            } else if agent_id == Some("qwen") && rows.is_empty() {
                // upstream's qwen adapter emits a null totals on empty
                // reports instead of the zeroed shape
                serde_json::Value::Null
            } else {
                let mut t = serde_json::to_value(&totals).unwrap();
                if ctx.merged {
                    t.as_object_mut().unwrap().insert(
                        "totalCost".into(),
                        crate::types::json_float(totals.total_cost),
                    );
                }
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
            totals_json = Some(t);
        } else {
            let (out, err) = table_section(*k, &rows, ctx, f, agent_id, detected);
            // upstream renders each section's missing-pricing warnings right
            // after that section's table — keeping per-section (stdout,
            // stderr) pairs reproduces the interleave on a merged stream
            pairs.push((out, err));
        }
    }
    if f.json || f.jq.is_some() {
        if let Some(t) = totals_json {
            json_out.push(("totals".into(), t));
        }
        // --no-cost drops every cost field from the JSON schema but leaves
        // token counts, missing-pricing flags and ordering untouched
        if f.no_cost {
            for (_, val) in &mut json_out {
                strip_cost_fields(val);
            }
            for key in ["totalCost", "cost", "costUSD", "storedCost"] {
                json_out.retain(|(k, _)| k != key);
            }
        }
        // single-section docs are plain sorted maps upstream; multi-section
        // docs keep invocation order via OrderedJsonMap
        let body = if json_out.len() == 2 {
            let m: serde_json::Map<String, serde_json::Value> = json_out.into_iter().collect();
            serde_json::to_string_pretty(&serde_json::Value::Object(m)).unwrap()
        } else {
            serde_json::to_string_pretty(&OrderedJson(json_out)).unwrap()
        };
        vec![(format!("{body}\n"), String::new())]
    } else {
        pairs
    }
}

/// Top-level report document: field order is insertion order (upstream's
/// `OrderedJsonMap`) while every nested object keeps `serde_json`'s
/// BTreeMap-sorted keys.
struct OrderedJson(Vec<(String, serde_json::Value)>);

impl serde::Serialize for OrderedJson {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

fn strip_cost_fields(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(o) => {
            for key in ["totalCost", "cost", "costUSD", "storedCost"] {
                o.remove(key);
            }
            for val in o.values_mut() {
                strip_cost_fields(val);
            }
        }
        serde_json::Value::Array(a) => {
            for val in a.iter_mut() {
                strip_cost_fields(val);
            }
        }
        _ => {}
    }
}

/// Upstream's per-agent empty-table messages (all go to stderr).
fn empty_table_message(agent_id: Option<&'static str>) -> String {
    match agent_id {
        Some("copilot") => {
            concat!(
                "No GitHub Copilot CLI usage data found.\n",
                "Session-state events are read from ${COPILOT_HOME:-~/.copilot}/session-state/<session-id>/events.jsonl; OpenTelemetry file export remains supported.\n",
                "See https://ccusage.com/guide/copilot/#data-source\n"
            )
            .to_string()
        }
        Some("codex") => "No Codex usage data found.\n".to_string(),
        Some(a @ ("amp" | "codebuff" | "goose")) => {
            format!(
                "No {} usage data found.\n",
                upstream_display(a)
            )
        }
        // rsusage extensions keep their adapter-specific names
        Some(a @ ("devin" | "zed" | "cursor")) => {
            format!("No {} usage data found.\n", agents::display_for(a).unwrap_or(a))
        }
        _ => "No usage data found.\n".to_string(),
    }
}

/// Render one report section to (stdout, stderr), dispatching on the
/// adapter's table shape exactly as upstream does.
fn table_section(
    kind: Report,
    rows: &[Row],
    ctx: &Ctx,
    f: &FilterArgs,
    agent_id: Option<&'static str>,
    detected: &[&'static str],
) -> (String, String) {
    if ctx.merged {
        let labels: Vec<String> = detected
            .iter()
            .map(|id| render::agent_label(id).to_string())
            .collect();
        if rows.is_empty() {
            let mut lines = crate::table::box_title_lines(
                &render::all_title(kind, &labels),
                crate::table::TerminalStyle {
                    color: f.color,
                    log_level: None,
                    no_color: f.no_color,
                },
            );
            return (lines.join("\n") + "\n", "No usage data found.\n".to_string());
        }
        return render::all_table(kind, rows, ctx, f, &labels);
    }
    if rows.is_empty() {
        return (String::new(), empty_table_message(agent_id));
    }
    match agent_id {
        Some("codex") => render::codex_table(kind, rows, ctx, f),
        Some(a @ ("amp" | "codebuff" | "goose")) => render::credits_table(
            upstream_display(a),
            kind,
            rows,
            ctx,
            f,
        ),
        Some(a) => {
            // grok/zcode only show Cache Create when some row uses it
            let show_cc = !matches!(a, "grok" | "zcode")
                || rows.iter().any(|r| r.cache_creation_tokens > 0);
            let name = upstream_display(a);
            render::focused_table(
                &format!("{name} Token Usage Report"),
                kind,
                rows,
                ctx,
                f,
                show_cc,
            )
        }
        None => (String::new(), String::new()),
    }
}

fn blocks_title(agent_id: Option<&'static str>, live: bool) -> &'static str {
    match (agent_id.is_some(), live) {
        (true, true) => "Session Blocks - Live",
        (false, true) => "Session Blocks (All Agents) - Live",
        _ => "Session Blocks",
    }
}

pub fn run_blocks(b: &BlocksArgs, _agent_id: Option<&'static str>) -> anyhow::Result<()> {
    // upstream `blocks` is the Claude report — even bare, non-claude readers
    // never feed it (non-claude `<agent> blocks` was rejected upstream).
    // `--all` is an rsusage extension that widens it to every detected agent.
    let agent_id = if b.filter.all { None } else { Some("claude") };
    let f = &b.filter;
    if b.session_length <= 0.0 {
        anyhow::bail!("Session length must be a positive number");
    }
    let offline = f.offline;
    let pricing_handle = std::thread::spawn(move || Pricing::load(offline));
    if b.live {
        let readers = readers_for(agent_id, b.filter.all);
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
    let readers = readers_for(agent_id, b.filter.all);
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
    emit_cached(key, f.jq.as_deref(), || {
        let (entries, _detected) = prepare(&readers, f, &tz);
        if entries.is_empty() && !(f.json || f.jq.is_some()) {
            // --active reports "no active block" on stdout even when the
            // source is empty; the plain report's empty line goes to stderr
            if b.active {
                return vec![("No active session block found.\n".to_string(), String::new())];
            }
            let msg = if agent_id.is_some() {
                "No Claude usage data found.\n"
            } else {
                "No usage data found.\n"
            };
            return vec![(String::new(), msg.to_string())];
        }
        let ctx = ctx_for(&pricing, f, tz.clone(), agent_id, None);
        let rows = block_rows(&entries, b, &ctx, now);
        let out = if f.json || f.jq.is_some() {
            format!(
                "{}\n",
                serde_json::to_string_pretty(&serde_json::json!({ "blocks": rows })).unwrap()
            )
        } else if rows.is_empty() {
            if b.active {
                "No active session block found.\n".to_string()
            } else {
                "No usage blocks found.\n".to_string()
            }
        } else {
            render::blocks_table(
                &rows,
                blocks_title(agent_id, false),
                ctx.merged,
                &detected_names(&entries),
            )
        };
        vec![(out, String::new())]
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
    if b.filter.order == Some(Order::Desc) {
        bl.reverse();
    }
    bl.iter()
        .map(|x| report::block_row(x, ctx, b.token_limit.as_deref(), now))
        .collect()
}

pub fn run_statusline(s: &StatusArgs, _agent_id: Option<&'static str>) -> anyhow::Result<()> {
    // upstream statusline is the Claude Code hook report — claude only
    let agent_id = Some("claude");
    let offline = s.offline && !s.no_offline;
    let pricing_handle = std::thread::spawn(move || Pricing::load(offline));
    let readers = readers_for(agent_id, false);
    let pricing = pricing_handle.join().unwrap();
    statusline::run(&readers, &pricing, &tz_of(&s.timezone), s)
}
