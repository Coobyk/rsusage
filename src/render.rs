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

pub struct ReportTable {
    pub title: String,
    pub period_header: String,
    pub rows: Vec<Row>,
    pub merged: bool,
    pub no_cost: bool,
    pub detected: Vec<&'static str>,
}

fn detected_subtitle(merged: bool, detected: &[&str]) -> String {
    if merged && !detected.is_empty() {
        format!("Detected: {}", detected.join(", "))
    } else {
        String::new()
    }
}

impl ReportTable {
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&banner(
            &self.title,
            &detected_subtitle(self.merged, &self.detected),
        ));
        out.push('\n');

        let cost_header = if self.no_cost {
            Cell::r("-")
        } else {
            Cell::multi(vec!["Cost".into(), "(USD)".into()], true)
        };
        let mut header = vec![Cell::multi(wrap_date(&self.period_header, DATE_W), false)];
        if self.merged {
            header.push(Cell::l("Agent"));
        }
        header.push(Cell::l("Models"));
        header.extend([
            Cell::r("Input"),
            Cell::r("Output"),
            Cell::multi(vec!["Cache".into(), "Create".into()], true),
            Cell::r("Cache Read"),
            Cell::r("Total Tokens"),
            cost_header,
        ]);

        let mut trows: Vec<Vec<Cell>> = vec![];
        for r in &self.rows {
            let mut cells = vec![Cell::multi(wrap_date(&r.period, DATE_W), false)];
            if self.merged {
                cells.push(Cell::l("All"));
            }
            cells.push(Cell::l(""));
            cells.extend(num_cells(&r.totals()));
            trows.push(cells);
            // per-agent sub-rows in merged tables
            if self.merged {
                if let Some(abs) = &r.agent_breakdowns {
                    for a in abs {
                        let mut cells = vec![Cell::l("")];
                        cells.push(Cell::l(format!(
                            "- {}",
                            crate::agents::display_for(&a.agent).unwrap_or(&a.agent)
                        )));
                        cells.push(Cell::multi(
                            a.models_used
                                .iter()
                                .map(|m| format!("- {}", trunc(m, MODELS_W - 2)))
                                .collect(),
                            false,
                        ));
                        cells.extend(num_cells(&a.totals()));
                        trows.push(cells);
                    }
                }
            }
        }
        let tot = Totals::sum(&self.rows);
        let mut cells = vec![Cell::l("Total")];
        if self.merged {
            cells.push(Cell::l(""));
        }
        cells.push(Cell::l(""));
        cells.extend(num_cells(&tot));
        trows.push(cells);

        let mut widths = vec![Some(DATE_W)];
        if self.merged {
            widths.push(Some(11));
        }
        widths.push(Some(MODELS_W));
        widths.extend([None, None, None, None, None, None]);
        out.push_str(&table(header, trows, &widths));
        out
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
