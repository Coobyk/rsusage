//! Upstream `ccusage-cli-parser`-compatible argument pre-parser.
//!
//! Runs before clap and reproduces the upstream parse pipeline's error
//! surface exactly: global token gates, sequential flag validation with
//! context-scoped option tables, missing/invalid value messages, then
//! post-parse semantic gates. On success it returns argv rewritten so every
//! flag is a canonical `--long` (context-dependent shorts like `-i` are
//! resolved here), which lets clap parse with a permissive superset schema.

use crate::report::Report;

/// Flags that consume the following argument — port of upstream
/// `option_takes_value`, used to skip values while collecting command tokens.
fn option_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-s" | "--since"
            | "-u" | "--until"
            | "--last"
            | "-m" | "--mode"
            | "--debug-samples"
            | "-o" | "--order"
            | "-z" | "--timezone"
            | "-q" | "--jq"
            | "--config"
            | "-p" | "--project"
            | "--project-aliases"
            | "-w" | "--start-of-week"
            | "-i" | "--id"
            | "-t" | "--token-limit"
            | "-n" | "--session-length"
            | "-B" | "--visual-burn-rate"
            | "--cost-source"
            | "--refresh-interval"
            | "--context-low-threshold"
            | "--context-medium-threshold"
            | "--speed"
            | "--pi-path"
            | "--open-claw-path"
            | "--sections"
    )
}

/// Non-flag arguments (value-taking options skipped) — port of upstream
/// `command_tokens`.
fn command_tokens(args: &[String]) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg.starts_with('-') {
            index += if option_takes_value(arg) && !arg.contains('=') {
                2
            } else {
                1
            };
            continue;
        }
        tokens.push(arg.as_str());
        index += 1;
    }
    tokens
}

fn fail(message: &str) -> ! {
    eprintln!("{message}");
    eprintln!("Run 'ccusage --help' for usage.");
    std::process::exit(2);
}

const REPORTS: [&str; 6] = [
    "daily", "weekly", "monthly", "session", "blocks", "statusline",
];

fn report_kind(name: &str) -> Option<Report> {
    Report::from_name(name)
}

/// Reports each agent exposes — upstream gives `weekly` to claude/opencode
/// and `blocks`/`statusline` to claude only; rsusage's extension agents take
/// the claude-minus-blocks/statusline set.
fn report_supported(agent: &str, report: &str) -> bool {
    match agent {
        "claude" => REPORTS.contains(&report),
        "opencode" | "devin" | "zed" | "cursor" => {
            matches!(report, "daily" | "weekly" | "monthly" | "session")
        }
        _ => matches!(report, "daily" | "monthly" | "session"),
    }
}

fn upstream_display(agent: &str) -> &'static str {
    match agent {
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

/// Command tokens upstream recognizes, plus rsusage's extension agents.
fn is_command(arg: &str) -> bool {
    REPORTS.contains(&arg)
        || matches!(
            arg,
            "claude" | "codex" | "opencode" | "amp" | "droid" | "codebuff" | "hermes" | "pi"
                | "goose" | "openclaw" | "kilo" | "copilot" | "gemini" | "antigravity" | "kimi"
                | "qwen" | "grok" | "zcode" | "devin" | "cursor" | "zed"
        )
}

#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    /// Before any command token: shared + unified extras.
    Root,
    /// Unified report tails: shared + --all/--sections/--by-agent (+ --id on
    /// session).
    Unified(Report),
    /// `blocks` and `claude blocks` share the table and the "blocks" scope.
    Blocks,
    /// `statusline` and `claude statusline` share the "statusline" scope.
    Statusline,
    /// `<agent> [<period-report>]` tail — the flag table and error scope
    /// depend on the agent and (for claude) the report.
    Agent { agent: &'static str, report: Report },
}

/// Error scope for unknown options — upstream names the dedicated parser
/// that rejected the flag; parsers built on the shared table produce the
/// generic `Unknown option` wording.
fn scope_name(ctx: Ctx) -> Option<&'static str> {
    match ctx {
        Ctx::Root | Ctx::Unified(Report::Daily | Report::Weekly | Report::Monthly) => None,
        Ctx::Unified(Report::Session) => Some("session"),
        Ctx::Blocks => Some("blocks"),
        Ctx::Statusline => Some("statusline"),
        Ctx::Agent { agent, report } => match agent {
            "claude" => match report {
                Report::Daily => Some("daily"),
                Report::Weekly => Some("weekly"),
                Report::Session => Some("session"),
                Report::Monthly => None,
            },
            "codex" => Some("codex"),
            "pi" => Some("pi"),
            "openclaw" => Some("openclaw"),
            _ => None,
        },
    }
}

/// Short → canonical long, context-sensitive: `-i` is `--instances` under
/// `claude daily` but `--id` under the session parsers, `-a`/`-r`/`-t`/`-n`
/// only exist under blocks, `-p`/`-w` only under specific claude reports.
/// Unknown-to-context shorts pass through unchanged so the table lookup
/// rejects them.
fn canonical_flag<'f>(flag: &'f str, ctx: Ctx) -> &'f str {
    match flag {
        "-s" => "--since",
        "-u" => "--until",
        "-m" => "--mode",
        "-o" => "--order",
        "-z" => "--timezone",
        "-q" => "--jq",
        "-j" => "--json",
        "-d" => "--debug",
        "-b" => "--breakdown",
        "-O" => "--offline",
        "-p" if matches!(ctx, Ctx::Agent { agent: "claude", report: Report::Daily }) => "--project",
        "-w" if matches!(ctx, Ctx::Agent { agent: "claude", report: Report::Weekly }) => {
            "--start-of-week"
        }
        "-t" if matches!(ctx, Ctx::Blocks) => "--token-limit",
        "-n" if matches!(ctx, Ctx::Blocks) => "--session-length",
        "-r" if matches!(ctx, Ctx::Blocks) => "--recent",
        "-a" if matches!(ctx, Ctx::Blocks) => "--active",
        "-B" if matches!(ctx, Ctx::Statusline) => "--visual-burn-rate",
        "-i" => match ctx {
            Ctx::Agent { agent: "claude", report: Report::Daily } => "--instances",
            Ctx::Unified(Report::Session)
            | Ctx::Agent { agent: "claude" | "codex", report: Report::Session } => "--id",
            _ => flag,
        },
        _ => flag,
    }
}

/// Canonical long-name → takes_value, or None when the flag is not valid in
/// this context. Port of each upstream parser's `match flag` dispatch table.
fn flag_takes_value(name: &str, ctx: Ctx) -> Option<bool> {
    // shared report flags — port of upstream `parse_shared_arg`
    const SHARED: &[(&str, bool)] = &[
        ("--since", true),
        ("--until", true),
        ("--last", true),
        ("--json", false),
        ("--mode", true),
        ("--debug", false),
        ("--debug-samples", true),
        ("--order", true),
        ("--breakdown", false),
        ("--offline", false),
        ("--no-offline", false),
        ("--color", false),
        ("--no-color", false),
        ("--timezone", true),
        ("--jq", true),
        ("--config", true),
        ("--compact", false),
        ("--single-thread", false),
        ("--no-cost", false),
    ];
    if let Ctx::Statusline = ctx {
        // upstream statusline has its own narrower flag set — no shared
        // report flags (not even --json)
        return match name {
            "--offline" | "--no-offline" | "--cache" | "--no-cache" | "--debug" => Some(false),
            "--visual-burn-rate" | "--cost-source" | "--refresh-interval"
            | "--context-low-threshold" | "--context-medium-threshold" | "--timezone"
            | "--config" => Some(true),
            _ => None,
        };
    }
    if let Some((_, v)) = SHARED.iter().find(|(n, _)| *n == name) {
        return Some(*v);
    }
    match ctx {
        Ctx::Root | Ctx::Unified(_) => match name {
            "--all" | "--by-agent" => Some(false),
            "--sections" => Some(true),
            "--id" if matches!(ctx, Ctx::Unified(Report::Session)) => Some(true),
            _ => None,
        },
        Ctx::Blocks => match name {
            "--active" | "--recent" | "--live" => Some(false), // --live is an rsusage extension
            "--token-limit" | "--session-length" => Some(true),
            _ => None,
        },
        Ctx::Statusline => unreachable!(),
        Ctx::Agent { agent, report } => {
            let claude: Option<bool> = match (agent, report) {
                ("claude", Report::Daily) => match name {
                    "--instances" => Some(false),
                    "--project" | "--project-aliases" => Some(true),
                    _ => None,
                },
                ("claude", Report::Weekly) => match name {
                    "--start-of-week" => Some(true),
                    _ => None,
                },
                _ => None,
            };
            claude.or(match name {
                "--id"
                    if matches!(report, Report::Session)
                        && matches!(agent, "claude" | "codex") =>
                {
                    Some(true)
                }
                "--speed" if agent == "codex" => Some(true),
                "--pi-path" if agent == "pi" => Some(true),
                "--open-claw-path" if agent == "openclaw" => Some(true),
                _ => None,
            })
        }
    }
}

/// Value validators — ports of upstream's `parse_*` fns; messages are
/// user-visible.
fn validate_value(name: &str, value: &str) -> Result<(), String> {
    match name {
        "--since" | "--until" => {
            if normalize_date_bound(value).is_none() {
                return Err(format!(
                    "Invalid value for {name} '{value}'. Expected YYYY-MM-DD or YYYYMMDD."
                ));
            }
        }
        "--last" => {
            if value.parse::<u32>().ok().filter(|n| *n > 0).is_none() {
                return Err(format!(
                    "Invalid value for --last '{value}'. Expected a whole number of periods, 1 or greater."
                ));
            }
        }
        "--mode" => {
            if !matches!(value, "auto" | "calculate" | "display") {
                return Err(format!("Invalid cost mode '{value}'"));
            }
        }
        "--order" => {
            if !matches!(value, "asc" | "desc") {
                return Err(format!("Invalid sort order '{value}'"));
            }
        }
        "--sections" => {
            // upstream trims, skips empty tokens, then errors on the first
            // unknown name or when nothing valid remains
            let mut any = false;
            for token in value.split(',') {
                let token = token.trim();
                if token.is_empty() {
                    continue;
                }
                if Report::from_name(token).is_none() {
                    return Err(format!(
                        "Invalid --sections value '{token}'. Expected one or more of: daily, weekly, monthly, session."
                    ));
                }
                any = true;
            }
            if !any {
                return Err(format!(
                    "Invalid --sections value '{value}'. Expected one or more of: daily, weekly, monthly, session."
                ));
            }
        }
        "--start-of-week" => {
            if !matches!(
                value,
                "sunday" | "monday" | "tuesday" | "wednesday" | "thursday" | "friday" | "saturday"
            ) {
                return Err(format!("Invalid week day '{value}'"));
            }
        }
        "--speed" => {
            if !matches!(value, "auto" | "standard" | "fast" | "flex") {
                return Err(format!("Invalid speed option '{value}'"));
            }
        }
        "--visual-burn-rate" => {
            if !matches!(value, "off" | "emoji" | "text" | "emoji-text") {
                return Err(format!("Invalid visual burn rate '{value}'"));
            }
        }
        "--cost-source" => {
            if !matches!(value, "auto" | "ccusage" | "cc" | "both") {
                return Err(format!("Invalid cost source '{value}'"));
            }
        }
        "--session-length" => {
            if value.parse::<f64>().is_err() {
                return Err("Invalid value for --session-length".to_string());
            }
        }
        "--token-limit" => {}
        "--refresh-interval" | "--debug-samples" => {
            if value.parse::<u64>().is_err() {
                return Err(format!("Invalid value for {name}"));
            }
        }
        "--context-low-threshold" | "--context-medium-threshold" => {
            if value.parse::<u8>().is_err() {
                return Err(format!("Invalid value for {name}"));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Port of upstream `normalize_date_bound`: YYYYMMDD or YYYY-MM-DD with real
/// calendar validation.
fn normalize_date_bound(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let digits: [u8; 8] = match bytes.len() {
        8 => bytes.try_into().ok()?,
        10 if bytes[4] == b'-' && bytes[7] == b'-' => {
            let mut digits = [0u8; 8];
            digits[..4].copy_from_slice(&bytes[..4]);
            digits[4..6].copy_from_slice(&bytes[5..7]);
            digits[6..].copy_from_slice(&bytes[8..]);
            digits
        }
        _ => return None,
    };
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let number = |slice: &[u8]| {
        slice
            .iter()
            .fold(0u32, |value, digit| value * 10 + u32::from(digit - b'0'))
    };
    let year = number(&digits[..4]) as i32;
    let month = number(&digits[4..6]);
    let day = number(&digits[6..]);
    if day == 0 || day > days_in_month(year, month) {
        return None;
    }
    String::from_utf8(digits.to_vec()).ok()
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

/// Values captured during the sequential pass for the post-parse gates.
#[derive(Default)]
struct Seen {
    last: bool,
    since: Option<String>,
    until: Option<String>,
    sections: bool,
    by_agent: bool,
    id: bool,
    command_ctx: Option<Ctx>,
}

/// Run the upstream-equivalent parse over `std::env::args`, exiting(2) with
/// upstream messages on any error. On success returns argv rewritten to
/// canonical `--long` flags so clap can consume it directly.
pub fn normalize_args() -> Vec<String> {
    let argv: Vec<String> = std::env::args().collect();
    let args = &argv[1..];

    // upstream checks control args before everything — help/version bypass
    // the whole pipeline, so let clap render them unmodified
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "-h" | "--help" | "-V" | "--version"))
    {
        return argv;
    }

    let tokens = command_tokens(args);

    // gate order mirrors upstream parse_from_with_config:
    // report-flag alias → agent-filter → unsupported-report
    if let Some(flag) = args.iter().find(|arg| {
        matches!(
            arg.as_str(),
            "--daily" | "--weekly" | "--monthly" | "--session" | "--blocks" | "--statusline"
        )
    }) {
        fail(&format!(
            "Report flags like {flag} are not supported. Use \"ccusage {}\" instead.",
            flag.trim_start_matches("--")
        ));
    }

    let blocks_ctx = matches!(tokens.as_slice(), ["blocks", ..])
        || matches!(tokens.as_slice(), ["claude", "blocks", ..]);
    if let Some(flag) = args.iter().find_map(|arg| {
        if arg == "--agent" || arg.starts_with("--agent=") {
            return Some("--agent");
        }
        if (arg == "-a" && !blocks_ctx) || arg.starts_with("-a=") {
            return Some("-a");
        }
        None
    }) {
        fail(&format!(
            "Agent filters like {flag} are not supported. Use \"ccusage <agent> <report>\", for example \"ccusage codex daily\"."
        ));
    }

    if tokens.len() >= 2 {
        let (agent, report) = (tokens[0], tokens[1]);
        if is_command(agent)
            && report_kind(agent).is_none()
            && !matches!(agent, "blocks" | "statusline")
            && !report_supported(agent, report)
        {
            let display = upstream_display(agent);
            if matches!(report, "blocks" | "statusline") {
                fail(&format!(
                    "The \"{report}\" report is only available for Claude Code usage.\nUse \"ccusage {agent} daily\" for {display} usage reports."
                ));
            } else {
                fail(&format!(
                    "The \"{report}\" report is not available for {display} usage.\nUse \"ccusage {agent} daily\" for {display} usage reports."
                ));
            }
        }
    }

    // sequential parse — reproduces upstream's in-order error semantics
    let mut out: Vec<String> = vec![argv[0].clone()];
    let mut seen = Seen::default();
    let mut ctx = Ctx::Root;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if ctx == Ctx::Root && is_command(arg) {
            out.push(arg.clone());
            i += 1;
            if let Some(report) = report_kind(arg) {
                ctx = Ctx::Unified(report);
            } else if arg == "blocks" {
                ctx = Ctx::Blocks;
            } else if arg == "statusline" {
                ctx = Ctx::Statusline;
            } else {
                // agent command — the report name is the immediate next
                // token when present; default is daily
                let agent: &'static str = Box::leak(arg.clone().into_boxed_str());
                let next = args.get(i).map(String::as_str);
                match next {
                    Some("blocks") => {
                        ctx = Ctx::Blocks;
                        out.push(args[i].clone());
                        i += 1;
                    }
                    Some("statusline") => {
                        ctx = Ctx::Statusline;
                        out.push(args[i].clone());
                        i += 1;
                    }
                    // the report name must be the immediate next token — a
                    // flag means the default daily report
                    Some(name) if !name.starts_with('-') => {
                        let report = report_kind(name).unwrap_or(Report::Daily);
                        ctx = Ctx::Agent { agent, report };
                        out.push(args[i].clone());
                        i += 1;
                    }
                    _ => {
                        ctx = Ctx::Agent {
                            agent,
                            report: Report::Daily,
                        };
                    }
                }
            }
            seen.command_ctx = Some(ctx);
            continue;
        }
        if !arg.starts_with('-') {
            if ctx == Ctx::Root {
                fail(&format!("Unknown command '{arg}'"));
            }
            fail(&format!("Expected option, got '{arg}'"));
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(f, v)| (f, Some(v)));
        let name = canonical_flag(flag, ctx);
        let Some(takes_value) = flag_takes_value(name, ctx) else {
            match scope_name(ctx) {
                Some(scope) => fail(&format!("Unknown {scope} option '{flag}'")),
                None => fail(&format!("Unknown option '{flag}'")),
            }
        };
        if takes_value {
            let value = match inline {
                Some(v) if !v.is_empty() => v.to_string(),
                Some(_) => fail(&format!("Missing value for {name}")),
                None => match args.get(i + 1) {
                    Some(v) if !v.starts_with('-') => {
                        i += 1;
                        v.clone()
                    }
                    _ => fail(&format!("Missing value for {name}")),
                },
            };
            if let Err(message) = validate_value(name, &value) {
                fail(&message);
            }
            match name {
                "--last" => seen.last = true,
                "--since" => seen.since = normalize_date_bound(&value),
                "--until" => seen.until = normalize_date_bound(&value),
                "--sections" => seen.sections = true,
                "--id" => seen.id = true,
                _ => {}
            }
            out.push(format!("{name}={value}"));
        } else {
            // upstream silently drops inline values on bool flags
            if name == "--by-agent" {
                seen.by_agent = true;
            }
            out.push(name.to_string());
        }
        i += 1;
    }

    // post-parse semantic gates, in upstream order: the session --id
    // combination check lives inside the session parser (before run-level
    // checks), then --last, then the date window
    if seen.id && (seen.sections || seen.by_agent) {
        fail("The --sections and --by-agent options cannot be used with session --id.");
    }
    if seen.last {
        let period_report = matches!(
            seen.command_ctx,
            Some(Ctx::Unified(Report::Daily | Report::Weekly | Report::Monthly))
                | Some(Ctx::Agent {
                    report: Report::Daily | Report::Weekly | Report::Monthly,
                    ..
                })
                | None
        );
        if !period_report {
            fail("The --last option is only available for the daily, weekly, and monthly reports.");
        }
        if seen.since.is_some() || seen.until.is_some() {
            fail("The --last option cannot be combined with --since or --until.");
        }
        if seen.sections {
            fail("The --last option cannot be used with --sections.");
        }
    }
    if let (Some(since), Some(until)) = (&seen.since, &seen.until) {
        if since > until {
            fail(&format!(
                "The --since date '{since}' is later than the --until date '{until}'."
            ));
        }
    }
    out
}
