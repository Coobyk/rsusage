//! Table rendering matching ccusage's boxed style.
use crate::types::*;
use unicode_width::UnicodeWidthStr;

fn w(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

pub fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn money(c: f64) -> String {
    format!("${:.2}", c)
}

/// Rounded banner box like ccusage prints above each report.
pub fn banner(title: &str, subtitle: &str) -> String {
    let mut lines = vec![String::new(), title.to_string()];
    if !subtitle.is_empty() {
        lines.push(subtitle.to_string());
    }
    lines.push(String::new());
    let inner = lines.iter().map(|l| w(l)).max().unwrap_or(0).max(46) + 2;
    let mut out = String::new();
    out.push_str(&format!("╭{}╮\n", "─".repeat(inner)));
    for l in lines {
        let pad = inner - w(&l);
        out.push_str(&format!(
            "│{}{}│\n",
            " ".repeat(pad / 2 + pad % 2) + &l,
            " ".repeat(pad / 2)
        ));
    }
    out.push_str(&format!("╰{}╯\n", "─".repeat(inner)));
    out
}

pub struct Cell {
    pub lines: Vec<String>,
    pub right: bool,
}

impl Cell {
    pub fn l(s: impl Into<String>) -> Self {
        Self {
            lines: vec![s.into()],
            right: false,
        }
    }
    pub fn r(s: impl Into<String>) -> Self {
        Self {
            lines: vec![s.into()],
            right: true,
        }
    }
    pub fn multi(lines: Vec<String>, right: bool) -> Self {
        Self { lines, right }
    }
}

fn trunc(s: &str, width: usize) -> String {
    if w(s) <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = UnicodeWidthStr::width(c.to_string().as_str());
        if used + cw > width - 1 {
            break;
        }
        used += cw;
        out.push(c);
    }
    out.push('…');
    out
}

/// Render a bordered table. `widths[i] == Some(n)` fixes that column width
/// (cells wrap/truncate to it); `None` = content-sized.
pub fn table(header: Vec<Cell>, rows: Vec<Vec<Cell>>, widths: &[Option<usize>]) -> String {
    let cols = header.len();
    let mut ws: Vec<usize> = vec![0; cols];
    let consider = |cells: &[Cell], ws: &mut Vec<usize>| {
        for (i, c) in cells.iter().enumerate() {
            for l in &c.lines {
                ws[i] = ws[i].max(w(l));
            }
        }
    };
    consider(&header, &mut ws);
    for r in &rows {
        consider(r, &mut ws);
    }
    // fixed columns: cap content at `f`, but keep at least min(f, 10) for
    // alignment and never below 3
    for (i, fixed) in widths.iter().enumerate() {
        if let Some(f) = fixed {
            ws[i] = ws[i].max(3).min(*f).max((*f).min(10));
        }
    }
    let sep = |l: &str, m: &str, r: &str| -> String {
        let mut s = String::from(l);
        for (i, cw) in ws.iter().enumerate() {
            s.push_str(&"─".repeat(cw + 2));
            s.push_str(if i + 1 == cols { r } else { m });
        }
        s.push('\n');
        s
    };
    let render_row = |cells: &[Cell]| -> String {
        let h = cells.iter().map(|c| c.lines.len()).max().unwrap_or(1);
        let mut out = String::new();
        for li in 0..h {
            out.push('│');
            for (i, c) in cells.iter().enumerate() {
                let raw = c.lines.get(li).cloned().unwrap_or_default();
                let text = trunc(&raw, ws[i]);
                let pad = ws[i] - w(&text);
                out.push(' ');
                if c.right {
                    out.push_str(&" ".repeat(pad));
                    out.push_str(&text);
                } else {
                    out.push_str(&text);
                    out.push_str(&" ".repeat(pad));
                }
                out.push(' ');
                out.push('│');
            }
            out.push('\n');
        }
        out
    };
    let mut out = sep("┌", "┬", "┐");
    out.push_str(&render_row(&header));
    out.push_str(&sep("├", "┼", "┤"));
    for r in &rows {
        out.push_str(&render_row(r));
        out.push_str(&sep("├", "┼", "┤"));
    }
    let tail = sep("├", "┼", "┤");
    if out.ends_with(&tail) {
        out.truncate(out.len() - tail.len());
        out.push_str(&sep("└", "┴", "┘"));
    }
    out
}

/// ccusage's narrow date column: dates split at the year boundary
/// ("2026" / "08-14"), long session ids chunk-wrap at `width`.
fn wrap_date(period: &str, width: usize) -> Vec<String> {
    // byte check, not char: a '-' byte at 4 means the slice boundary is safe
    // even when the period isn't ASCII
    let b = period.as_bytes();
    if b.len() == 10 && b[4] == b'-' {
        return vec![period[..4].to_string(), period[5..].to_string()];
    }
    if w(period) <= width {
        return vec![period.to_string()];
    }
    if !period.contains('-') {
        return hard_wrap(period, width);
    }
    let mut lines = vec![];
    let mut cur = String::new();
    for part in period.split_inclusive('-') {
        if w(&cur) + w(part) > width && !cur.is_empty() {
            lines.push(cur.trim_end_matches('-').to_string());
            cur = String::new();
        }
        cur.push_str(part);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(trunc(period, width));
    }
    lines
}

fn hard_wrap(s: &str, width: usize) -> Vec<String> {
    let mut lines = vec![];
    let mut cur = String::new();
    for ch in s.chars() {
        if w(&cur) + UnicodeWidthStr::width(ch.to_string().as_str()) > width {
            lines.push(cur.clone());
            cur.clear();
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

fn num_cells(t: &Totals) -> Vec<Cell> {
    vec![
        Cell::r(commas(t.input_tokens)),
        Cell::r(commas(t.output_tokens)),
        Cell::r(commas(t.cache_creation_tokens)),
        Cell::r(commas(t.cache_read_tokens)),
        Cell::r(commas(t.total_tokens)),
        Cell::r(money(t.total_cost)),
    ]
}

const DATE_W: usize = 10;
const MODELS_W: usize = 12;

// ---------------------------------------------------------------------------
// upstream-shaped report tables (ccusage-core output.rs, adapters/common,
// adapters/codex, adapter-all)
// ---------------------------------------------------------------------------

use crate::cli::FilterArgs;
use crate::report::{Ctx, Report};
use crate::table::*;
use std::collections::BTreeSet;
use std::io::IsTerminal;

const USAGE_COMPACT_WIDTH_THRESHOLD: usize = 100;

pub fn first_column(kind: Report) -> &'static str {
    match kind {
        Report::Daily => "Date",
        Report::Weekly => "Week",
        Report::Monthly => "Month",
        Report::Session => "Session",
    }
}

pub fn kind_label(kind: Report) -> &'static str {
    match kind {
        Report::Daily => "Daily",
        Report::Weekly => "Weekly",
        Report::Monthly => "Monthly",
        Report::Session => "Session",
    }
}

fn style_of(f: &FilterArgs) -> TerminalStyle {
    TerminalStyle {
        color: f.color,
        log_level: None,
        no_color: f.no_color,
    }
}

fn use_compact(f: &FilterArgs, width: usize) -> bool {
    f.compact || (std::io::stdout().is_terminal() && width < USAGE_COMPACT_WIDTH_THRESHOLD)
}

/// The installed binary renders the first 10 chars of the stored
/// RFC3339-millis value — its `parse_ts_timestamp` build predates the
/// tz-converting `format_last_activity_date`, so `--timezone` has no
/// effect on this column.
fn last_activity_date(value: &str) -> String {
    value.get(..10).unwrap_or(value).to_string()
}

/// stderr tail shared by the focused/common/codex paths: missing-pricing
/// warnings then the compact-mode note.
fn table_stderr(rows: &[Row], f: &FilterArgs, compact: bool) -> String {
    let mut err = String::new();
    let models: BTreeSet<&str> = rows
        .iter()
        .flat_map(|r| r.model_breakdowns.iter())
        .filter(|m| m.missing_pricing)
        .map(|m| m.model_name.as_str())
        .collect();
    for model in models {
        if f.offline {
            err.push_str(&format!(
                "WARN  Missing embedded pricing for {model}; cost excludes this model. Run without --offline or update ccusage after pricing is added.\n"
            ));
        } else {
            err.push_str(&format!(
                "WARN  Missing pricing for {model}; cost excludes this model. Update pricing or run again after LiteLLM has the model.\n"
            ));
        }
    }
    if compact {
        err.push_str("\nRunning in Compact Mode\nExpand terminal width to see cache metrics and total tokens\n");
    }
    err
}

fn focused_columns(
    first: &'static str,
    compact: bool,
    show_cc: bool,
) -> (Vec<&'static str>, Vec<Align>) {
    let mut headers = vec![first, "Models", "Input", "Output"];
    let mut aligns = vec![Align::Left, Align::Left, Align::Right, Align::Right];
    if !compact && show_cc {
        headers.push("Cache Create");
        aligns.push(Align::Right);
    }
    if compact {
        headers.push("Cost (USD)");
        aligns.push(Align::Right);
    } else {
        headers.extend(["Cache Read", "Total Tokens", "Cost (USD)"]);
        aligns.extend([Align::Right, Align::Right, Align::Right]);
    }
    (headers, aligns)
}

fn push_focused_breakdown(
    table: &mut SimpleTable,
    r: &Row,
    compact: bool,
    show_cc: bool,
    include_last_activity: bool,
    f: &FilterArgs,
) {
    if !f.breakdown {
        return;
    }
    let style = style_of(f);
    for b in &r.model_breakdowns {
        let total = b.total_tokens;
        let mut values = vec![
            color(style, format_breakdown_model_label(&b.model_name), Color::Grey),
            String::new(),
            color(style, format_number(b.input_tokens), Color::Grey),
            color(style, format_number(b.output_tokens), Color::Grey),
        ];
        if !compact && show_cc {
            values.push(color(style, format_number(b.cache_creation_tokens), Color::Grey));
        }
        if compact {
            values.push(color(style, format_currency(b.cost), Color::Grey));
        } else {
            values.extend([
                color(style, format_number(b.cache_read_tokens), Color::Grey),
                color(style, format_number(total), Color::Grey),
                color(style, format_currency(b.cost), Color::Grey),
            ]);
        }
        if f.no_cost {
            values.pop();
        }
        if include_last_activity {
            values.push(String::new());
        }
        table.push(values);
    }
}

/// `print_usage_table_with_options` — the focused per-agent table.
pub fn focused_table(
    title: &str,
    kind: Report,
    rows: &[Row],
    ctx: &Ctx,
    f: &FilterArgs,
    show_cache_creation: bool,
) -> (String, String) {
    let width = terminal_width();
    let compact = use_compact(f, width);
    let style = style_of(f);
    let include_last_activity = rows.iter().any(|r| r.last_activity.is_some());
    let mut out = box_title_lines(title, style);
    let (mut headers, mut aligns) = focused_columns(first_column(kind), compact, show_cache_creation);
    if f.no_cost {
        headers.pop();
        aligns.pop();
    }
    if include_last_activity {
        headers.push("Last Activity");
        aligns.push(Align::Left);
    }
    let mut table = SimpleTable::new(headers, aligns, style)
        .with_terminal_width(width)
        .with_date_compaction(true);
    for r in rows {
        let models = format_models_multiline(&r.models_used);
        let mut values = vec![
            r.period.clone(),
            models,
            format_number(r.input_tokens),
            format_number(r.output_tokens),
        ];
        if !compact && show_cache_creation {
            values.push(format_number(r.cache_creation_tokens));
        }
        if compact {
            values.push(format_currency(r.total_cost));
        } else {
            values.extend([
                format_number(r.cache_read_tokens),
                format_number(r.total_tokens),
                format_currency(r.total_cost),
            ]);
        }
        if f.no_cost {
            values.pop();
        }
        if include_last_activity {
            values.push(last_activity_date(r.last_activity.as_deref().unwrap_or_default()));
        }
        table.push(values);
        push_focused_breakdown(&mut table, r, compact, show_cache_creation, include_last_activity, f);
    }
    table.separator();
    let mut total_row = vec![
        color(style, "Total", Color::Yellow),
        String::new(),
        color(style, format_number(rows.iter().map(|r| r.input_tokens).sum()), Color::Yellow),
        color(style, format_number(rows.iter().map(|r| r.output_tokens).sum()), Color::Yellow),
    ];
    if !compact && show_cache_creation {
        total_row.push(color(
            style,
            format_number(rows.iter().map(|r| r.cache_creation_tokens).sum()),
            Color::Yellow,
        ));
    }
    if compact {
        total_row.push(color(
            style,
            format_currency(rows.iter().map(|r| r.total_cost).sum()),
            Color::Yellow,
        ));
    } else {
        total_row.push(color(
            style,
            format_number(rows.iter().map(|r| r.cache_read_tokens).sum()),
            Color::Yellow,
        ));
        total_row.push(color(
            style,
            format_number(rows.iter().map(|r| r.total_tokens).sum()),
            Color::Yellow,
        ));
        total_row.push(color(
            style,
            format_currency(rows.iter().map(|r| r.total_cost).sum()),
            Color::Yellow,
        ));
    }
    if f.no_cost {
        total_row.pop();
    }
    if include_last_activity {
        total_row.push(String::new());
    }
    table.push(total_row);
    out.extend(table.render_lines());
    (out.join("\n") + "\n", table_stderr(rows, f, compact))
}

/// `print_table_for_agent` in adapters/common — the credits-column table
/// used by amp, codebuff and goose.
pub fn credits_table(
    agent_name: &str,
    kind: Report,
    rows: &[Row],
    ctx: &Ctx,
    f: &FilterArgs,
) -> (String, String) {
    let width = terminal_width();
    let compact = use_compact(f, width);
    let style = style_of(f);
    let include_last_activity = rows.iter().any(|r| r.last_activity.is_some());
    let mut out = box_title_lines(
        &format!("{agent_name} Token Usage Report - {}", kind_label(kind)),
        style,
    );
    let (mut headers, mut aligns) = if compact {
        (
            vec![
                first_column(kind),
                "Models",
                "Input",
                "Output",
                "Credits",
                "Cost (USD)",
            ],
            vec![
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
            ],
        )
    } else {
        (
            vec![
                first_column(kind),
                "Models",
                "Input",
                "Output",
                "Cache Create",
                "Cache Read",
                "Total Tokens",
                "Credits",
                "Cost (USD)",
            ],
            vec![
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
            ],
        )
    };
    if f.no_cost {
        headers.pop();
        aligns.pop();
    }
    if include_last_activity {
        headers.push("Last Activity");
        aligns.push(Align::Left);
    }
    let mut table = SimpleTable::new(headers, aligns, style)
        .with_terminal_width(width)
        .with_date_compaction(true);
    for r in rows {
        let models = format_models_multiline(&r.models_used);
        let mut values = if compact {
            vec![
                r.period.clone(),
                models,
                format_number(r.input_tokens),
                format_number(r.output_tokens),
                "0.00".to_string(),
                format_currency(r.total_cost),
            ]
        } else {
            vec![
                r.period.clone(),
                models,
                format_number(r.input_tokens),
                format_number(r.output_tokens),
                format_number(r.cache_creation_tokens),
                format_number(r.cache_read_tokens),
                format_number(r.total_tokens),
                "0.00".to_string(),
                format_currency(r.total_cost),
            ]
        };
        if f.no_cost {
            values.pop();
        }
        if include_last_activity {
            values.push(last_activity_date(r.last_activity.as_deref().unwrap_or_default()));
        }
        table.push(values);
        if f.breakdown {
            for b in &r.model_breakdowns {
                let mut values = if compact {
                    vec![
                        color(style, format_breakdown_model_label(&b.model_name), Color::Grey),
                        String::new(),
                        color(style, format_number(b.input_tokens), Color::Grey),
                        color(style, format_number(b.output_tokens), Color::Grey),
                        color(style, "0.00", Color::Grey),
                        color(style, format_currency(b.cost), Color::Grey),
                    ]
                } else {
                    vec![
                        color(style, format_breakdown_model_label(&b.model_name), Color::Grey),
                        String::new(),
                        color(style, format_number(b.input_tokens), Color::Grey),
                        color(style, format_number(b.output_tokens), Color::Grey),
                        color(style, format_number(b.cache_creation_tokens), Color::Grey),
                        color(style, format_number(b.cache_read_tokens), Color::Grey),
                        color(style, format_number(b.total_tokens), Color::Grey),
                        color(style, "0.00", Color::Grey),
                        color(style, format_currency(b.cost), Color::Grey),
                    ]
                };
                if f.no_cost {
                    values.pop();
                }
                if include_last_activity {
                    values.push(String::new());
                }
                table.push(values);
            }
        }
    }
    table.separator();
    let total_cost: f64 = rows.iter().map(|r| r.total_cost).sum();
    let mut total_row = if compact {
        vec![
            color(style, "Total", Color::Yellow),
            String::new(),
            color(style, format_number(rows.iter().map(|r| r.input_tokens).sum()), Color::Yellow),
            color(style, format_number(rows.iter().map(|r| r.output_tokens).sum()), Color::Yellow),
            color(style, "0.00", Color::Yellow),
            color(style, format_currency(total_cost), Color::Yellow),
        ]
    } else {
        vec![
            color(style, "Total", Color::Yellow),
            String::new(),
            color(style, format_number(rows.iter().map(|r| r.input_tokens).sum()), Color::Yellow),
            color(style, format_number(rows.iter().map(|r| r.output_tokens).sum()), Color::Yellow),
            color(style, format_number(rows.iter().map(|r| r.cache_creation_tokens).sum()), Color::Yellow),
            color(style, format_number(rows.iter().map(|r| r.cache_read_tokens).sum()), Color::Yellow),
            color(style, format_number(rows.iter().map(|r| r.total_tokens).sum()), Color::Yellow),
            color(style, "0.00", Color::Yellow),
            color(style, format_currency(total_cost), Color::Yellow),
        ]
    };
    if f.no_cost {
        total_row.pop();
    }
    if include_last_activity {
        total_row.push(String::new());
    }
    table.push(total_row);
    out.extend(table.render_lines());
    (out.join("\n") + "\n", table_stderr(rows, f, compact))
}

/// Codex's own table: reasoning column, non-cached input, per-day date
/// compaction whenever the terminal is at most 120 columns.
pub fn codex_table(kind: Report, rows: &[Row], ctx: &Ctx, f: &FilterArgs) -> (String, String) {
    let width = terminal_width();
    let style = style_of(f);
    let mut out = box_title_lines(
        &format!("Codex Token Usage Report - {}", kind_label(kind)),
        style,
    );
    let mut headers = vec![
        first_column(kind),
        "Models",
        "Input",
        "Output",
        "Reasoning",
        "Cache Create",
        "Cache Read",
        "Total Tokens",
        "Cost (USD)",
    ];
    let mut aligns = vec![
        Align::Left,
        Align::Left,
        Align::Right,
        Align::Right,
        Align::Right,
        Align::Right,
        Align::Right,
        Align::Right,
        Align::Right,
    ];
    if f.no_cost {
        headers.pop();
        aligns.pop();
    }
    let mut table = SimpleTable::new(headers, aligns, style)
        .with_terminal_width(width)
        .with_date_compaction(true);
    for r in rows {
        // upstream `codex_table_label`: daily labels compact at <=120 cols
        // regardless of fit
        let label = sanitize_terminal_text(&r.period);
        let label = if kind == Report::Daily && width <= 120 {
            compact_date_cell(&label).unwrap_or(label)
        } else {
            label
        };
        let mut row = vec![
            label,
            format_models_multiline(&r.models_used),
            format_number(r.input_tokens),
            format_number(r.output_tokens),
            format_number(r.reasoning_tokens),
            format_number(r.cache_creation_tokens),
            format_number(r.cache_read_tokens),
            format_number(r.total_tokens),
            format_currency(r.total_cost),
        ];
        if f.no_cost {
            row.pop();
        }
        table.push(row);
        if f.breakdown {
            for b in &r.model_breakdowns {
                let mut row = vec![
                    color(style, format_breakdown_model_label(&b.model_name), Color::Grey),
                    String::new(),
                    color(style, format_number(b.input_tokens), Color::Grey),
                    color(style, format_number(b.output_tokens), Color::Grey),
                    color(style, format_number(b.reasoning_tokens), Color::Grey),
                    color(style, format_number(b.cache_creation_tokens), Color::Grey),
                    color(style, format_number(b.cache_read_tokens), Color::Grey),
                    color(style, format_number(b.total_tokens), Color::Grey),
                    color(style, format_currency(b.cost), Color::Grey),
                ];
                if f.no_cost {
                    row.pop();
                }
                table.push(row);
            }
        }
    }
    table.separator();
    let mut total_row = vec![
        color(style, "Total", Color::Yellow),
        String::new(),
        color(style, format_number(rows.iter().map(|r| r.input_tokens).sum()), Color::Yellow),
        color(style, format_number(rows.iter().map(|r| r.output_tokens).sum()), Color::Yellow),
        color(style, format_number(rows.iter().map(|r| r.reasoning_tokens).sum()), Color::Yellow),
        color(style, format_number(rows.iter().map(|r| r.cache_creation_tokens).sum()), Color::Yellow),
        color(style, format_number(rows.iter().map(|r| r.cache_read_tokens).sum()), Color::Yellow),
        color(style, format_number(rows.iter().map(|r| r.total_tokens).sum()), Color::Yellow),
        color(style, format_currency(rows.iter().map(|r| r.total_cost).sum()), Color::Yellow),
    ];
    if f.no_cost {
        total_row.pop();
    }
    table.push(total_row);
    out.extend(table.render_lines());
    (out.join("\n") + "\n", table_stderr(rows, f, false))
}

/// Upstream `agent_label` for merged table cells.
pub(crate) fn agent_label(agent: &str) -> &str {
    match agent {
        "all" => "All",
        "claude" => "Claude",
        "codex" => "Codex",
        "opencode" => "OpenCode",
        "amp" => "Amp",
        "droid" => "Droid",
        "codebuff" => "Codebuff",
        "hermes" => "Hermes",
        "pi" => "pi-agent",
        "goose" => "Goose",
        "openclaw" => "OpenClaw",
        "kilo" => "Kilo",
        "copilot" => "GitHub Copilot CLI",
        "gemini" => "Gemini CLI",
        "antigravity" => "Antigravity",
        "kimi" => "Kimi",
        "qwen" => "Qwen",
        "grok" => "Grok",
        "zcode" => "ZCode",
        "cline" => "Cline",
        other => other,
    }
}

/// Merged (all-agents) table — adapter-all `print_table`: Agent column,
/// per-agent sub-rows under aggregated parents, model breakdowns on
/// `--breakdown`.
pub fn all_title(kind: Report, detected_labels: &[String]) -> String {
    format!(
        "Coding (Agent) CLI Usage Report - {}\nDetected: {}",
        kind_label(kind),
        if detected_labels.is_empty() {
            "None".to_string()
        } else {
            detected_labels.join(", ")
        }
    )
}

pub fn all_table(
    kind: Report,
    rows: &[Row],
    _ctx: &Ctx,
    f: &FilterArgs,
    detected_labels: &[String],
) -> (String, String) {
    let width = terminal_width();
    let compact = use_compact(f, width);
    let style = style_of(f);
    let mut out = box_title_lines(&all_title(kind, detected_labels), style);
    let (mut headers, mut aligns) = if compact {
        (
            vec![
                first_column(kind),
                "Agent",
                "Models",
                "Input",
                "Output",
                "Cost (USD)",
            ],
            vec![
                Align::Left,
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
            ],
        )
    } else {
        (
            vec![
                first_column(kind),
                "Agent",
                "Models",
                "Input",
                "Output",
                "Cache Create",
                "Cache Read",
                "Total Tokens",
                "Cost (USD)",
            ],
            vec![
                Align::Left,
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
            ],
        )
    };
    if f.no_cost {
        headers.pop();
        aligns.pop();
    }
    let mut table = SimpleTable::new(headers, aligns, style)
        .with_terminal_width(width)
        .with_date_compaction(true);
    for r in rows {
        table.push(all_table_row(r, compact, false, f));
        if let Some(subs) = &r.agent_breakdowns {
            for a in subs {
                table.push(merged_sub_row(a, compact, f));
                if f.breakdown {
                    for b in &a.model_breakdowns {
                        push_merged_model_row(&mut table, b, compact, f);
                    }
                }
            }
        } else if f.breakdown {
            for b in &r.model_breakdowns {
                push_merged_model_row(&mut table, b, compact, f);
            }
        }
    }
    table.separator();
    let total_cost: f64 = rows.iter().map(|r| r.total_cost).sum();
    let mut total_row = if compact {
        vec![
            color(style, "Total", Color::Yellow),
            String::new(),
            String::new(),
            color(style, format_number(rows.iter().map(|r| r.input_tokens).sum()), Color::Yellow),
            color(style, format_number(rows.iter().map(|r| r.output_tokens).sum()), Color::Yellow),
            color(style, format_currency(total_cost), Color::Yellow),
        ]
    } else {
        vec![
            color(style, "Total", Color::Yellow),
            String::new(),
            String::new(),
            color(style, format_number(rows.iter().map(|r| r.input_tokens).sum()), Color::Yellow),
            color(style, format_number(rows.iter().map(|r| r.output_tokens).sum()), Color::Yellow),
            color(style, format_number(rows.iter().map(|r| r.cache_creation_tokens).sum()), Color::Yellow),
            color(style, format_number(rows.iter().map(|r| r.cache_read_tokens).sum()), Color::Yellow),
            color(style, format_number(rows.iter().map(|r| r.total_tokens).sum()), Color::Yellow),
            color(style, format_currency(total_cost), Color::Yellow),
        ]
    };
    if f.no_cost {
        total_row.pop();
    }
    table.push(total_row);
    out.extend(table.render_lines());
    (out.join("\n") + "\n", table_stderr(rows, f, compact))
}

fn all_table_row(r: &Row, compact: bool, breakdown: bool, f: &FilterArgs) -> Vec<String> {
    let period = if breakdown { String::new() } else { r.period.clone() };
    let agent = if breakdown {
        format!("- {}", agent_label(r.agent.as_deref().unwrap_or("all")))
    } else if r.agent_breakdowns.is_some() {
        "All".to_string()
    } else {
        agent_label(r.agent.as_deref().unwrap_or("all")).to_string()
    };
    let models = if r.agent_breakdowns.is_some() {
        String::new()
    } else {
        format_models_multiline(&r.models_used)
    };
    let mut values = if compact {
        vec![
            period,
            agent,
            models,
            format_number(r.input_tokens),
            format_number(r.output_tokens),
            format_currency(r.total_cost),
        ]
    } else {
        vec![
            period,
            agent,
            models,
            format_number(r.input_tokens),
            format_number(r.output_tokens),
            format_number(r.cache_creation_tokens),
            format_number(r.cache_read_tokens),
            format_number(r.total_tokens),
            format_currency(r.total_cost),
        ]
    };
    if f.no_cost {
        values.pop();
    }
    values
}

/// Per-agent sub-row under a merged parent: empty period, "- {label}"
/// agent cell, the agent's models and numbers.
fn merged_sub_row(a: &AgentRow, compact: bool, f: &FilterArgs) -> Vec<String> {
    let models = format_models_multiline(&a.models_used);
    let mut values = if compact {
        vec![
            String::new(),
            format!("- {}", agent_label(&a.agent)),
            models,
            format_number(a.input_tokens),
            format_number(a.output_tokens),
            format_currency(a.total_cost),
        ]
    } else {
        vec![
            String::new(),
            format!("- {}", agent_label(&a.agent)),
            models,
            format_number(a.input_tokens),
            format_number(a.output_tokens),
            format_number(a.cache_creation_tokens),
            format_number(a.cache_read_tokens),
            format_number(a.total_tokens),
            format_currency(a.total_cost),
        ]
    };
    if f.no_cost {
        values.pop();
    }
    values
}

fn push_merged_model_row(table: &mut SimpleTable, b: &ModelRow, compact: bool, f: &FilterArgs) {
    let style = style_of(f);
    let model = color(style, format_breakdown_model_label(&b.model_name), Color::Grey);
    let mut row = if compact {
        vec![
            String::new(),
            String::new(),
            model,
            color(style, format_number(b.input_tokens), Color::Grey),
            color(style, format_number(b.output_tokens), Color::Grey),
            color(style, format_currency(b.cost), Color::Grey),
        ]
    } else {
        vec![
            String::new(),
            String::new(),
            model,
            color(style, format_number(b.input_tokens), Color::Grey),
            color(style, format_number(b.output_tokens), Color::Grey),
            color(style, format_number(b.cache_creation_tokens), Color::Grey),
            color(style, format_number(b.cache_read_tokens), Color::Grey),
            color(style, format_number(b.total_tokens), Color::Grey),
            color(style, format_currency(b.cost), Color::Grey),
        ]
    };
    if f.no_cost {
        row.pop();
    }
    table.push(row);
}


fn detected_subtitle(merged: bool, detected: &[&str]) -> String {
    if merged && !detected.is_empty() {
        format!("Detected: {}", detected.join(", "))
    } else {
        String::new()
    }
}

pub fn blocks_table(rows: &[BlockRow], title: &str, merged: bool, detected: &[&str]) -> String {
    let mut out = String::new();
    out.push_str(&banner(title, &detected_subtitle(merged, detected)));
    out.push('\n');
    let header = vec![
        Cell::l("Block"),
        Cell::l("Status"),
        Cell::l("Models"),
        Cell::r("Input"),
        Cell::r("Output"),
        Cell::multi(vec!["Cache".into(), "Create".into()], true),
        Cell::r("Cache Read"),
        Cell::r("Total Tokens"),
        Cell::r("Cost"),
    ];
    let trows: Vec<Vec<Cell>> = rows
        .iter()
        .map(|b| {
            let tc = &b.token_counts;
            vec![
                Cell::multi(
                    hard_wrap(&b.start_time[..16.min(b.start_time.len())], 16),
                    false,
                ),
                Cell::l(if b.is_active { "ACTIVE" } else { "" }),
                Cell::multi(b.models.iter().map(|m| trunc(m, MODELS_W)).collect(), false),
                Cell::r(commas(tc.input_tokens)),
                Cell::r(commas(tc.output_tokens)),
                Cell::r(commas(tc.cache_creation_input_tokens)),
                Cell::r(commas(tc.cache_read_input_tokens)),
                Cell::r(commas(b.total_tokens)),
                Cell::r(money(b.cost_usd)),
            ]
        })
        .collect();
    out.push_str(&table(
        header,
        trows,
        &[
            Some(16),
            None,
            Some(MODELS_W),
            None,
            None,
            None,
            None,
            None,
            None,
        ],
    ));
    out
}
