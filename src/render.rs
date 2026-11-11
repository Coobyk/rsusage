//! Table rendering matching ccusage's boxed style.
use crate::types::*;
use unicode_width::UnicodeWidthStr;

fn w(s: &str) -> usize { UnicodeWidthStr::width(s) }

pub fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn money(c: f64) -> String { format!("${:.2}", c) }

/// Rounded banner box like ccusage prints above each report.
pub fn banner(title: &str, subtitle: &str) -> String {
    let lines: Vec<String> = {
        let mut v = vec![String::new(), title.to_string()];
        if !subtitle.is_empty() {
            v.push(subtitle.to_string());
        }
        v.push(String::new());
        v
    };
    let inner = lines.iter().map(|l| w(l)).max().unwrap_or(0).max(46) + 2;
    let mut out = String::new();
    out.push_str(&format!("╭{}╮\n", "─".repeat(inner)));
    for l in lines {
        let pad = inner - w(&l);
        out.push_str(&format!("│{}{}│\n", " ".repeat(pad / 2 + pad % 2) + &l, " ".repeat(pad / 2)));
    }
    out.push_str(&format!("╰{}╯\n", "─".repeat(inner)));
    out
}

#[derive(Clone)]
pub struct Cell {
    pub lines: Vec<String>,
    pub right: bool,
}

impl Cell {
    pub fn l(s: impl Into<String>) -> Self {
        Self { lines: vec![s.into()], right: false }
    }
    pub fn r(s: impl Into<String>) -> Self {
        Self { lines: vec![s.into()], right: true }
    }
    pub fn multi(lines: Vec<String>, right: bool) -> Self {
        Self { lines, right }
    }
}

/// truncate with ellipsis to `width`
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
pub fn table(header: Vec<Vec<Cell>>, rows: Vec<Vec<Cell>>, widths: &[Option<usize>]) -> String {
    let cols = header.iter().map(|h| h.len()).max().unwrap_or(0);
    // base widths from content
    let mut ws: Vec<usize> = vec![0; cols];
    let consider = |cells: &[Cell], ws: &mut Vec<usize>| {
        for (i, c) in cells.iter().enumerate() {
            for l in &c.lines {
                ws[i] = ws[i].max(w(l));
            }
        }
    };
    for h in &header {
        consider(h, &mut ws);
    }
    for r in &rows {
        consider(r, &mut ws);
    }
    for (i, fixed) in widths.iter().enumerate() {
        if let Some(f) = fixed {
            ws[i] = (*f).min(ws[i].max(3));
            ws[i] = ws[i].max(*f.min(&10));
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
    for h in &header {
        out.push_str(&render_row(h));
    }
    out.push_str(&sep("├", "┼", "┤"));
    for r in &rows {
        out.push_str(&render_row(r));
        out.push_str(&sep("├", "┼", "┤"));
    }
    // replace last separator with bottom border
    let tail = sep("├", "┼", "┤");
    if out.ends_with(&tail) {
        out.truncate(out.len() - tail.len());
        out.push_str(&sep("└", "┴", "┘"));
    }
    out
}

/// wrap a date string to look like ccusage's narrow date column: dates split
/// at the year boundary ("2026" / "08-14"), long ids chunk-wrap at `width`.
fn wrap_date(period: &str, width: usize) -> Vec<String> {
    if period.len() == 10 && period.chars().nth(4) == Some('-') {
        return vec![period[..4].to_string(), period[5..].to_string()];
    }
    if w(period) <= width {
        return vec![period.to_string()];
    }
    let mut lines = vec![];
    let mut cur = String::new();
    // hard-wrap unbroken long tokens first
    if !period.contains('-') {
        let mut lines = vec![];
        let mut cur = String::new();
        for ch in period.chars() {
            if w(&cur) + UnicodeWidthStr::width(ch.to_string().as_str()) > width {
                lines.push(cur.clone());
                cur.clear();
            }
            cur.push(ch);
        }
        if !cur.is_empty() {
            lines.push(cur);
        }
        return lines;
    }
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
        // unbreakable long token — hard truncate
        lines.push(trunc(period, width));
    }
    lines
}

fn num_cells(r: &Row) -> Vec<Cell> {
    vec![
        Cell::r(commas(r.input_tokens)),
        Cell::r(commas(r.output_tokens)),
        Cell::r(commas(r.cache_creation_tokens)),
        Cell::r(commas(r.cache_read_tokens)),
        Cell::r(commas(r.total_tokens)),
        Cell::r(money(r.total_cost)),
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
    pub detected: Vec<String>,
}

impl ReportTable {
    pub fn render(&self) -> String {
        let mut out = String::new();
        let subtitle = if self.merged && !self.detected.is_empty() {
            format!("Detected: {}", self.detected.join(", "))
        } else {
            String::new()
        };
        out.push_str(&banner(&self.title, &subtitle));
        out.push('\n');

        let mut header = vec![
            Cell::multi(wrap_date(&self.period_header, DATE_W), false),
        ];
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
            Cell::r(if self.no_cost { "-" } else { "(USD)" }),
        ]);
        // header line for cost
        if !self.no_cost {
            if let Some(c) = header.last_mut() {
                c.lines = vec!["Cost".into(), "(USD)".into()];
            }
        }

        let mut trows: Vec<Vec<Cell>> = vec![];
        let mut tot = Row {
            agent: None,
            period: "Total".into(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            total_tokens: 0,
            total_cost: 0.0,
            reasoning_tokens: 0,
            first_activity: None,
            last_activity: None,
            project: String::new(),
            models_used: vec![],
            model_breakdowns: vec![],
            metadata: None,
            agent_breakdowns: None,
        };
        for r in &self.rows {
            let mut cells = vec![Cell::multi(wrap_date(&r.period, DATE_W), false)];
            if self.merged {
                cells.push(Cell::l("All"));
            }
            cells.push(Cell::l(""));
            cells.extend(num_cells(r));
            trows.push(cells);
            tot.input_tokens += r.input_tokens;
            tot.output_tokens += r.output_tokens;
            tot.cache_creation_tokens += r.cache_creation_tokens;
            tot.cache_read_tokens += r.cache_read_tokens;
            tot.total_tokens += r.total_tokens;
            tot.total_cost += r.total_cost;
            // per-agent sub-rows in merged tables
            if self.merged {
                if let Some(abs) = &r.agent_breakdowns {
                    for a in abs {
                        let mut cells = vec![Cell::l("")];
                        cells.push(Cell::l(format!("- {}", crate::agents::display_for(&a.agent))));
                        cells.push(Cell::multi(
                            a.models_used
                                .iter()
                                .map(|m| format!("- {}", trunc(m, MODELS_W - 2)))
                                .collect(),
                            false,
                        ));
                        cells.extend([
                            Cell::r(commas(a.input_tokens)),
                            Cell::r(commas(a.output_tokens)),
                            Cell::r(commas(a.cache_creation_tokens)),
                            Cell::r(commas(a.cache_read_tokens)),
                            Cell::r(commas(a.total_tokens)),
                            Cell::r(money(a.total_cost)),
                        ]);
                        trows.push(cells);
                    }
                } else if let Some(md) = &r.metadata {
                    // merged row without --by-agent: still list agents
                    if let Some(agents) = md.get("agents").and_then(|a| a.as_array()) {
                        for ag in agents {
                            if let Some(name) = ag.as_str() {
                                let mut cells = vec![Cell::l("")];
                                cells.push(Cell::l(format!("- {}", crate::agents::display_for(name))));
                                let models: Vec<String> = r
                                    .model_breakdowns
                                    .iter()
                                    .filter(|_| true)
                                    .map(|m| format!("- {}", trunc(&m.model_name, MODELS_W - 2)))
                                    .collect();
                                cells.push(Cell::multi(models, false));
                                cells.extend(std::iter::repeat(Cell::r("")).take(6));
                                trows.push(cells);
                            }
                        }
                    }
                }
            }
        }
        // totals row
        let mut cells = vec![Cell::l("Total")];
        if self.merged {
            cells.push(Cell::l(""));
        }
        cells.push(Cell::l(""));
        cells.extend(num_cells(&tot));
        trows.push(cells);

        let widths: Vec<Option<usize>> = {
            let mut v = vec![Some(DATE_W)];
            if self.merged {
                v.push(Some(11));
            }
            v.push(Some(MODELS_W));
            v.extend([None, None, None, None, None, None]);
            v
        };
        out.push_str(&table(vec![header], trows, &widths));
        out
    }
}

/// Session report table (per-agent or merged).
pub fn session_table(title: &str, rows: &[Row], merged: bool, no_cost: bool, detected: &[String]) -> String {
    ReportTable {
        title: title.into(),
        period_header: "Session".into(),
        rows: rows.to_vec(),
        merged,
        no_cost,
        detected: detected.to_vec(),
    }
    .render()
}

/// Blocks table.
pub fn blocks_table(
    rows: &[BlockRow],
    title: &str,
    detected: &[String],
) -> String {
    let mut out = String::new();
    let subtitle = if detected.is_empty() {
        String::new()
    } else {
        format!("Detected: {}", detected.join(", "))
    };
    out.push_str(&banner(title, &subtitle));
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
    let mut trows = vec![];
    for b in rows {
        let tc = &b.token_counts;
        let g = |k: &str| tc.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        trows.push(vec![
            Cell::multi(wrap_date(&b.start_time.chars().take(16).collect::<String>(), 16), false),
            Cell::l(if b.is_active { "ACTIVE" } else { "" }),
            Cell::multi(
                b.models.iter().map(|m| trunc(m, MODELS_W)).collect(),
                false,
            ),
            Cell::r(commas(g("inputTokens"))),
            Cell::r(commas(g("outputTokens"))),
            Cell::r(commas(g("cacheCreationInputTokens"))),
            Cell::r(commas(g("cacheReadInputTokens"))),
            Cell::r(commas(b.total_tokens)),
            Cell::r(money(b.cost_usd)),
        ]);
    }
    out.push_str(&table(vec![header], trows, &[Some(16), None, Some(MODELS_W), None, None, None, None, None, None]));
    out
}
