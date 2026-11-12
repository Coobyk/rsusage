//! `statusline` — the Claude Code hook consumer upstream renders as
//! `🤖 Model | 💰 $s session / $t today / $b block (Xh Ym left)[| 🔥 …] | 🧠 …`.
//! Reads the hook payload from stdin and caches the rendered line per session
//! under `$TMP/ccusage-semaphore/<session>.lock`.
use crate::agents::AgentReader;
use crate::cli::{StatusArgs, DEFAULT_SESSION_HOURS};
use crate::pricing::Pricing;
use crate::report;
use crate::table::{color, format_currency, format_number, Color, TerminalStyle};
use crate::types::*;
use serde::Deserialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Deserialize)]
struct Hook {
    session_id: String,
    transcript_path: String,
    model: HookModel,
    cost: Option<HookCost>,
    context_window: Option<HookContext>,
    effort: Option<HookEffort>,
}
#[derive(Debug, Deserialize)]
struct HookModel {
    id: Option<String>,
    display_name: String,
}
#[derive(Debug, Deserialize)]
struct HookEffort {
    level: String,
}
#[derive(Debug, Deserialize)]
struct HookCost {
    total_cost_usd: f64,
}
#[derive(Debug, Deserialize)]
struct HookContext {
    total_input_tokens: u64,
    context_window_size: u64,
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct Cache {
    date: String,
    #[serde(rename = "lastOutput")]
    last_output: String,
    #[serde(rename = "lastUpdateTime")]
    last_update_time: u64,
    #[serde(rename = "transcriptPath")]
    transcript_path: String,
    #[serde(rename = "transcriptMtime")]
    transcript_mtime: u64,
    #[serde(rename = "isUpdating", default)]
    is_updating: bool,
    pid: Option<u32>,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn cache_path(session_id: &str) -> PathBuf {
    std::env::temp_dir()
        .join("ccusage-semaphore")
        .join(format!("{session_id}.lock"))
}

fn read_cache(path: &Path) -> Option<Cache> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_cache(path: &Path, cache: &Cache) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec(cache) {
        let _ = std::fs::write(path, bytes);
    }
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn process_is_alive(pid: u32) -> bool {
    pid == std::process::id()
}

/// Replay the cached line while it's still fresh: unexpired and the
/// transcript unchanged. An in-flight writer (isUpdating + live pid) also
/// serves its previous line instead of nothing.
fn cached_output(cache: &Cache, current_mtime: u64, now: u64, refresh_interval: u64) -> Option<&str> {
    if cache.last_output.is_empty() {
        return None;
    }
    let expired = now.saturating_sub(cache.last_update_time) >= refresh_interval * 1000;
    let file_modified = cache.transcript_mtime != current_mtime;
    if expired || file_modified {
        if cache.is_updating && cache.pid.is_some_and(process_is_alive) {
            return Some(cache.last_output.as_str());
        }
        return None;
    }
    Some(cache.last_output.as_str())
}

fn transcript_mtime_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

/// Per-entry auto cost: stored when present (opencode's stored 0 doesn't
/// count), else calculated. Claude only ever sees the plain `cost_usd` arm.
fn entry_auto_cost(e: &UsageEntry, pricing: &Pricing) -> f64 {
    let stored = if e.agent == "opencode" {
        e.cost_usd.filter(|&c| c > 0.0)
    } else {
        e.cost_usd
    };
    stored.or_else(|| pricing.entry_cost(e)).unwrap_or(0.0)
}

/// Last `assistant` line in the transcript with usage, exactly like upstream:
/// `input_tokens + cache_creation_input_tokens + cache_read_input_tokens`,
/// window = the model's catalog context limit or 200K.
fn context_from_transcript(path: &Path, model_id: Option<&str>, pricing: &Pricing) -> Option<(u64, u64)> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let Some(usage) = value.get("message").and_then(|m| m.get("usage")) else {
            continue;
        };
        let Some(input) = usage.get("input_tokens").and_then(|v| v.as_u64()) else {
            continue;
        };
        let cc = usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or_default();
        let cr = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or_default();
        let size = model_id
            .filter(|id| !id.is_empty())
            .and_then(|id| pricing.context_limit(id))
            .unwrap_or(200_000);
        return Some((input + cc + cr, size));
    }
    None
}

fn remaining_time(minutes: i64) -> String {
    let (h, m) = (minutes / 60, minutes % 60);
    if h > 0 {
        format!("{h}h {m}m left")
    } else {
        format!("{m}m left")
    }
}

struct Args {
    visual_burn_rate: BurnStyle,
    cost_source: CostSource,
    low: u8,
    medium: u8,
}

#[derive(Clone, Copy, PartialEq)]
enum BurnStyle {
    Off,
    Emoji,
    Text,
    EmojiText,
}

#[derive(Clone, Copy, PartialEq)]
enum CostSource {
    Auto,
    Ccusage,
    Cc,
    Both,
}

fn parse_args(s: &StatusArgs) -> Result<Args, String> {
    if s.context_low_threshold >= s.context_medium_threshold {
        return Err(format!(
            "Context low threshold ({}) must be less than medium threshold ({})",
            s.context_low_threshold, s.context_medium_threshold
        ));
    }
    let burn = match s.visual_burn_rate.as_str() {
        "off" => BurnStyle::Off,
        "emoji" => BurnStyle::Emoji,
        "text" => BurnStyle::Text,
        "emoji-text" => BurnStyle::EmojiText,
        v => return Err(format!("Invalid visual burn rate '{v}'")),
    };
    let source = match s.cost_source.as_str() {
        "auto" => CostSource::Auto,
        "ccusage" => CostSource::Ccusage,
        "cc" => CostSource::Cc,
        "both" => CostSource::Both,
        v => return Err(format!("Invalid cost source '{v}'")),
    };
    Ok(Args {
        visual_burn_rate: burn,
        cost_source: source,
        low: s.context_low_threshold,
        medium: s.context_medium_threshold,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    readers: &[Box<dyn AgentReader>],
    pricing: &Pricing,
    tz: &jiff::tz::TimeZone,
    s: &StatusArgs,
) -> anyhow::Result<()> {
    let args = parse_args(s).map_err(|e| anyhow::anyhow!(e))?;

    // upstream blocks on stdin to EOF; a held-open empty pipe would hang
    // forever, so give the read a short bound before reporting no input
    let stdin = read_stdin();
    if stdin.trim().is_empty() {
        anyhow::bail!("❌ No input provided");
    }
    let hook: Hook = serde_json::from_str(stdin.trim())
        .map_err(|e| anyhow::anyhow!("Invalid input format: {e}"))?;

    let cache_enabled = s.cache && !s.no_cache;
    let path = cache_path(&hook.session_id);
    let current_mtime = transcript_mtime_ms(Path::new(&hook.transcript_path));
    let initial = cache_enabled.then(|| read_cache(&path)).flatten();
    if let Some(cache) = initial.as_ref() {
        if let Some(output) = cached_output(cache, current_mtime, now_millis(), s.refresh_interval)
        {
            println!("{output}");
            return Ok(());
        }
    }
    if cache_enabled {
        let now = now_millis();
        write_cache(
            &path,
            &Cache {
                date: report::ms_to_iso(now as i64),
                last_output: initial
                    .as_ref()
                    .map(|c| c.last_output.clone())
                    .unwrap_or_default(),
                last_update_time: initial.as_ref().map(|c| c.last_update_time).unwrap_or(0),
                transcript_path: hook.transcript_path.clone(),
                transcript_mtime: current_mtime,
                is_updating: true,
                pid: Some(std::process::id()),
            },
        );
    }

    match render(&hook, &args, s, readers, pricing, tz) {
        Ok(line) => {
            println!("{line}");
            if cache_enabled {
                let now = now_millis();
                write_cache(
                    &path,
                    &Cache {
                        date: report::ms_to_iso(now as i64),
                        last_output: line,
                        last_update_time: now,
                        transcript_path: hook.transcript_path.clone(),
                        transcript_mtime: current_mtime,
                        is_updating: false,
                        pid: None,
                    },
                );
            }
            Ok(())
        }
        Err(e) => {
            if let Some(cache) = initial.as_ref().filter(|c| !c.last_output.is_empty()) {
                println!("{}", cache.last_output);
            } else {
                println!("❌ Error generating status");
            }
            if let Some(mut cache) = cache_enabled.then(|| read_cache(&path)).flatten() {
                cache.is_updating = false;
                cache.pid = None;
                write_cache(&path, &cache);
            }
            Err(e)
        }
    }
}

fn read_stdin() -> String {
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return String::new();
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = std::io::stdin().read_to_string(&mut buf);
        let _ = tx.send(buf);
    });
    rx.recv_timeout(std::time::Duration::from_millis(50))
        .unwrap_or_default()
}

fn render(
    hook: &Hook,
    args: &Args,
    s: &StatusArgs,
    readers: &[Box<dyn AgentReader>],
    pricing: &Pricing,
    tz: &jiff::tz::TimeZone,
) -> anyhow::Result<String> {
    let entries = crate::app::load_entries(readers);
    let now = now_millis() as i64;

    // upstream's calculated session cost is a plain entry sum — it yields
    // Some(0.0) even when no entry matches the session, never N/A
    let session_cost_of = || -> Option<f64> {
        Some(
            entries
                .iter()
                .filter(|e| e.session_id.as_ref() == hook.session_id)
                .map(|e| entry_auto_cost(e, pricing))
                .sum(),
        )
    };

    let cc_cost = hook.cost.as_ref().map(|c| c.total_cost_usd);
    let session_display = match args.cost_source {
        CostSource::Both => format!(
            "({} cc / {} ccusage)",
            cc_cost.map(format_currency).unwrap_or_else(|| "N/A".into()),
            session_cost_of()
                .map(format_currency)
                .unwrap_or_else(|| "N/A".into())
        ),
        CostSource::Cc => cc_cost
            .map(format_currency)
            .unwrap_or_else(|| "N/A".into()),
        CostSource::Ccusage => session_cost_of()
            .map(format_currency)
            .unwrap_or_else(|| "N/A".into()),
        CostSource::Auto => cc_cost
            .or_else(session_cost_of)
            .map(format_currency)
            .unwrap_or_else(|| "N/A".into()),
    };

    let today_key = report::date_key(now, tz);
    let today_cost: f64 = entries
        .iter()
        .filter(|e| report::date_key(e.timestamp_ms, tz) == today_key)
        .map(|e| entry_auto_cost(e, pricing))
        .sum();

    let blocks = report::blocks(&entries, DEFAULT_SESSION_HOURS as f64, now, pricing);
    let active = blocks.iter().find(|b| b.is_active);
    let (block_info, burn_info) = match active {
        Some(b) => {
            let ctx = report::Ctx::new(
                pricing,
                report::CostMode::Auto,
                tz.clone(),
                true,
                false,
                1,
                false,
                crate::cli::Order::Asc,
                false,
                "all",
            );
            let cost = ctx.acc_cost(&b.acc);
            let remaining = (b.end_ms - now) / 60_000;
            let mut burn = String::new();
            // upstream burn rate uses the first→last-entry span, and a block
            // with a single instant has no rate at all
            let span_min = (b.acc.last_activity_ms - b.acc.first_activity_ms) as f64 / 60_000.0;
            if span_min > 0.0 {
                let per_hour = cost / span_min * 60.0;
                let non_cache = (b.acc.tokens.input + b.acc.tokens.output) as f64;
                let indicator = non_cache / span_min;
                let (emoji, text) = if indicator < 2000.0 {
                    ("🟢", "Normal")
                } else if indicator < 5000.0 {
                    ("⚠️", "Moderate")
                } else {
                    ("🚨", "High")
                };
                let mut segs = vec![format!("{}/hr", format_currency(per_hour))];
                if matches!(args.visual_burn_rate, BurnStyle::Emoji | BurnStyle::EmojiText) {
                    segs.push(emoji.to_string());
                }
                if matches!(args.visual_burn_rate, BurnStyle::Text | BurnStyle::EmojiText) {
                    segs.push(format!("({text})"));
                }
                if args.visual_burn_rate != BurnStyle::Off {
                    burn = format!(" | 🔥 {}", segs.join(" "));
                }
            }
            (
                format!("{} block ({})", format_currency(cost), remaining_time(remaining)),
                burn,
            )
        }
        None => ("No active block".to_string(), String::new()),
    };

    let style = TerminalStyle::default();
    let context = hook
        .context_window
        .as_ref()
        .map(|c| (c.total_input_tokens, c.context_window_size))
        .or_else(|| {
            context_from_transcript(
                Path::new(&hook.transcript_path),
                hook.model.id.as_deref(),
                pricing,
            )
        })
        .map(|(input, limit)| {
            let pct = if limit == 0 {
                0
            } else {
                ((input as f64 / limit as f64) * 100.0).round() as u64
            };
            let c = if pct < u64::from(args.low) {
                Color::Green
            } else if pct < u64::from(args.medium) {
                Color::Yellow
            } else {
                Color::Red
            };
            format!("{} ({})", format_number(input), color(style, format!("{pct}%"), c))
        })
        .unwrap_or_else(|| "N/A".into());

    let model_label = &hook.model.display_name;
    let model_segment = match hook.effort.as_ref().filter(|e| !e.level.is_empty()) {
        Some(e) => format!("{model_label} ({})", e.level),
        None => model_label.to_string(),
    };

    Ok(format!(
        "🤖 {} | 💰 {} session / {} today / {}{} | 🧠 {}",
        model_segment,
        session_display,
        format_currency(today_cost),
        block_info,
        burn_info,
        context
    ))
}
