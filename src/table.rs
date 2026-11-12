//! Faithful port of upstream's `ccusage-terminal` table engine plus the
//! shared output helpers — box titles, `SimpleTable` layout, number/model
//! formatting, ANSI colors — so `--table` output matches ccusage byte-for-byte
//! at the same terminal width.

use std::io::IsTerminal;

use unicode_width::UnicodeWidthChar;

// ---------------------------------------------------------------------------
// style (ccusage-terminal/style.rs)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Color {
    Blue,
    Green,
    Grey,
    Red,
    Yellow,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalStyle {
    pub color: bool,
    pub log_level: Option<u8>,
    pub no_color: bool,
}

pub fn color(style: TerminalStyle, value: impl AsRef<str>, color: Color) -> String {
    let value = value.as_ref();
    if !use_color(&style) {
        return value.to_string();
    }
    let code = match color {
        Color::Blue => 34,
        Color::Green => 32,
        Color::Grey => 90,
        Color::Red => 31,
        Color::Yellow => 33,
    };
    format!("\x1b[{code}m{value}\x1b[0m")
}

fn use_color(style: &TerminalStyle) -> bool {
    if style.no_color || std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    style.color || std::env::var_os("FORCE_COLOR").is_some() || std::io::stdout().is_terminal()
}

// ---------------------------------------------------------------------------
// width (ccusage-terminal/width.rs)
// ---------------------------------------------------------------------------

fn skip_ansi_escape(bytes: &[u8], index: usize) -> usize {
    let mut index = index + 1;
    if index < bytes.len() && bytes[index] == b'[' {
        index += 1;
        while index < bytes.len() && !(bytes[index] as char).is_ascii_alphabetic() {
            index += 1;
        }
        index += usize::from(index < bytes.len());
    }
    index
}

pub fn visible_width(value: &str) -> usize {
    let bytes = value.as_bytes();
    let mut width = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b {
            index = skip_ansi_escape(bytes, index);
            continue;
        }
        let Some(ch) = value[index..].chars().next() else {
            break;
        };
        width += UnicodeWidthChar::width(ch).unwrap_or(0);
        index += ch.len_utf8();
    }
    width
}

fn contains_ansi(value: &str) -> bool {
    value.as_bytes().contains(&0x1b)
}

pub fn visible_width_max_line(value: &str) -> usize {
    value.lines().map(visible_width).max().unwrap_or_default()
}

fn ansi_continuation(value: &str) -> String {
    let mut continuation = String::new();
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != 0x1b {
            let Some(ch) = value[index..].chars().next() else {
                break;
            };
            index += ch.len_utf8();
            continue;
        }
        let end = skip_ansi_escape(bytes, index);
        let escape = &value[index..end];
        if escape.ends_with('m') {
            if escape == "\x1b[0m" {
                continuation.clear();
            } else {
                continuation.push_str(escape);
            }
        }
        index = end;
    }
    continuation
}

fn ensure_ansi_reset(value: &str) -> String {
    if !contains_ansi(value) || value.ends_with("\x1b[0m") {
        return value.to_string();
    }
    format!("{value}\x1b[0m")
}

/// Shorten `value` to at most `width` display columns, marking the cut with `…`.
pub fn truncate_to_width(value: &str, width: usize) -> String {
    if visible_width(value) <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut output = String::new();
    let mut current_width = 0;
    let mut index = 0;
    let bytes = value.as_bytes();
    while index < bytes.len() {
        if bytes[index] == 0x1b {
            let start = index;
            index = skip_ansi_escape(bytes, index);
            output.push_str(&value[start..index]);
            continue;
        }
        let Some(ch) = value[index..].chars().next() else {
            break;
        };
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + char_width >= width {
            break;
        }
        output.push(ch);
        current_width += char_width;
        index += ch.len_utf8();
    }
    if contains_ansi(value) && !output.ends_with("\x1b[0m") {
        output.push_str("\x1b[0m");
    }
    output.push('…');
    output
}

// ---------------------------------------------------------------------------
// terminal (ccusage-terminal/terminal.rs)
// ---------------------------------------------------------------------------

pub const DEFAULT_TERMINAL_WIDTH: usize = 120;

pub fn terminal_width() -> usize {
    select_terminal_width(
        std::env::var("COLUMNS").ok().as_deref(),
        terminal_size::terminal_size().map(|(terminal_size::Width(w), _)| w as usize),
    )
}

fn select_terminal_width(columns: Option<&str>, detected_width: Option<usize>) -> usize {
    columns
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|width| *width > 0)
        .or_else(|| detected_width.filter(|width| *width > 0))
        .unwrap_or(DEFAULT_TERMINAL_WIDTH)
}

// ---------------------------------------------------------------------------
// title (ccusage-terminal/title.rs)
// ---------------------------------------------------------------------------

pub fn box_title_lines(title: &str, style: TerminalStyle) -> Vec<String> {
    let title_lines = title.lines().collect::<Vec<_>>();
    let content_width = title_lines
        .iter()
        .map(|line| visible_width(line))
        .max()
        .unwrap_or_default()
        .max(40)
        + 2;
    let mut lines = Vec::with_capacity(title_lines.len() + 5);
    lines.push(String::new());
    lines.push(format!("╭{}╮", "─".repeat(content_width + 2)));
    lines.push(format!("│{}│", " ".repeat(content_width + 2)));
    for line in title_lines {
        let padding = content_width.saturating_sub(visible_width(line));
        let left_padding = padding / 2;
        let right_padding = padding - left_padding;
        lines.push(format!(
            "│ {}{}{} │",
            " ".repeat(left_padding),
            color(style, line, Color::Blue),
            " ".repeat(right_padding)
        ));
    }
    lines.push(format!("│{}│", " ".repeat(content_width + 2)));
    lines.push(format!("╰{}╯", "─".repeat(content_width + 2)));
    lines.push(String::new());
    lines
}

// ---------------------------------------------------------------------------
// table (ccusage-terminal/table.rs)
// ---------------------------------------------------------------------------

const MAX_MODELS_CONTENT_WIDTH: usize = 25;
const MIN_WRAP_COLUMN_WIDTH: usize = 8;
const MAX_FALLBACK_TEXT_WIDTH: usize = 12;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Align {
    Left,
    Right,
}

pub struct SimpleTable {
    headers: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Option<Vec<String>>>,
    style: TerminalStyle,
    terminal_width: usize,
    compact_dates: bool,
}

struct ContentWidths<'a> {
    numeric: &'a [usize],
    text: &'a [usize],
}

impl SimpleTable {
    pub fn new(headers: Vec<&str>, aligns: Vec<Align>, style: TerminalStyle) -> Self {
        Self {
            headers: headers.into_iter().map(str::to_string).collect(),
            aligns,
            rows: Vec::new(),
            style,
            terminal_width: DEFAULT_TERMINAL_WIDTH,
            compact_dates: false,
        }
    }

    pub fn with_terminal_width(mut self, width: usize) -> Self {
        self.terminal_width = width;
        self
    }

    pub fn with_date_compaction(mut self, compact_dates: bool) -> Self {
        self.compact_dates = compact_dates;
        self
    }

    pub fn push(&mut self, row: Vec<String>) {
        self.rows.push(Some(row));
    }

    pub fn separator(&mut self) {
        self.rows.push(None);
    }

    pub fn column_count(&self) -> usize {
        self.headers.len()
    }

    pub fn render_lines(&self) -> Vec<String> {
        let widths = self.column_widths();
        let mut lines = Vec::new();
        lines.push(border('┌', '┬', '┐', &widths));
        for header_row in expand_multiline_row(&self.headers, self.headers.len(), &widths) {
            let header_row = header_row
                .iter()
                .map(|header| color(self.style, header, Color::Blue))
                .collect::<Vec<_>>();
            lines.push(table_line(&header_row, &self.aligns, &widths));
        }
        lines.push(border('├', '┼', '┤', &widths));
        for (row_index, row) in self.rows.iter().enumerate() {
            match row {
                Some(row) => {
                    let row = self.compact_date_row(row, &widths);
                    for physical_row in expand_multiline_row(&row, self.headers.len(), &widths) {
                        lines.push(table_line(&physical_row, &self.aligns, &widths));
                    }
                }
                None => lines.push(border('├', '┼', '┤', &widths)),
            }
            if row.is_some()
                && row_index + 1 < self.rows.len()
                && !matches!(self.rows.get(row_index + 1), Some(None))
            {
                lines.push(border('├', '┼', '┤', &widths));
            }
        }
        lines.push(border('└', '┴', '┘', &widths));
        lines
    }

    fn column_widths(&self) -> Vec<usize> {
        let model_column = self.headers.iter().position(|header| header == "Models");
        let numeric_content_widths = self
            .aligns
            .iter()
            .enumerate()
            .map(|(index, align)| {
                if *align != Align::Right {
                    return 0;
                }
                self.rows
                    .iter()
                    .flatten()
                    .filter_map(|row| row.get(index))
                    .map(|cell| visible_width_max_line(cell))
                    .max()
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        let text_content_widths = self
            .aligns
            .iter()
            .enumerate()
            .map(|(index, align)| {
                if *align == Align::Right {
                    return 0;
                }
                self.rows
                    .iter()
                    .flatten()
                    .filter_map(|row| row.get(index))
                    .map(|cell| visible_width_max_line(cell))
                    .max()
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        let content_widths = self
            .headers
            .iter()
            .enumerate()
            .map(|(index, header)| {
                let width = visible_width_max_line(header);
                if model_column == Some(index) {
                    width.min(MAX_MODELS_CONTENT_WIDTH)
                } else {
                    width
                }
            })
            .collect::<Vec<_>>();
        let mut content_widths = content_widths;
        for row in self.rows.iter().flatten() {
            for (index, cell) in row.iter().enumerate() {
                let cell_width = visible_width_max_line(cell);
                let cell_width = if model_column == Some(index) {
                    cell_width.min(MAX_MODELS_CONTENT_WIDTH)
                } else {
                    cell_width
                };
                if let Some(width) = content_widths.get_mut(index) {
                    *width = (*width).max(cell_width);
                }
            }
        }
        let widths = content_widths
            .iter()
            .enumerate()
            .map(|(index, width)| {
                if model_column == Some(index) {
                    (width + 2).clamp(15, MAX_MODELS_CONTENT_WIDTH + 2)
                } else if self.aligns.get(index) == Some(&Align::Right) {
                    (width + 3).max(11)
                } else if index == 1 {
                    (width + 2).max(15)
                } else {
                    (width + 2).max(10)
                }
            })
            .collect::<Vec<_>>();
        let total_required = cli_table_required_width(&widths);
        let first_column_min = if self.compact_dates && total_required <= self.terminal_width {
            12
        } else {
            10
        };
        fit_widths_to_terminal(
            widths,
            &self.aligns,
            self.terminal_width,
            first_column_min,
            model_column,
            ContentWidths {
                numeric: &numeric_content_widths,
                text: &text_content_widths,
            },
            self.compact_dates,
        )
    }

    fn compact_date_row(&self, row: &[String], widths: &[usize]) -> Vec<String> {
        if !self.compact_dates
            || widths
                .first()
                .copied()
                .unwrap_or_default()
                .saturating_sub(2)
                >= 10
        {
            return row.to_vec();
        }
        let mut row = row.to_vec();
        if let Some((first, compact)) = row
            .first_mut()
            .and_then(|f| compact_date_cell(f).map(|c| (f, c)))
        {
            *first = compact;
        }
        row
    }
}

fn expand_multiline_row(row: &[String], column_count: usize, widths: &[usize]) -> Vec<Vec<String>> {
    let cells = (0..column_count)
        .map(|index| {
            let content_width = widths
                .get(index)
                .copied()
                .unwrap_or_default()
                .saturating_sub(2);
            row.get(index)
                .map(|cell| wrap_cell_lines(cell, content_width))
                .filter(|lines| !lines.is_empty())
                .unwrap_or_else(|| vec![String::new()])
        })
        .collect::<Vec<_>>();
    let height = cells.iter().map(Vec::len).max().unwrap_or(1);
    (0..height)
        .map(|line_index| {
            cells
                .iter()
                .map(|lines| lines.get(line_index).cloned().unwrap_or_default())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn fit_widths_to_terminal(
    mut widths: Vec<usize>,
    aligns: &[Align],
    terminal_width: usize,
    first_column_min: usize,
    model_column: Option<usize>,
    content_widths: ContentWidths<'_>,
    compact_dates: bool,
) -> Vec<usize> {
    if cli_table_required_width(&widths) <= terminal_width {
        return widths;
    }

    let fallback_minimums = widths
        .iter()
        .enumerate()
        .map(|(index, _)| {
            if aligns.get(index) == Some(&Align::Right) {
                10
            } else if index == 0 {
                first_column_min
            } else if model_column == Some(index) || (model_column.is_none() && index == 1) {
                12
            } else {
                content_widths
                    .text
                    .get(index)
                    .copied()
                    .unwrap_or_default()
                    .saturating_add(2)
                    .clamp(MIN_WRAP_COLUMN_WIDTH, MAX_FALLBACK_TEXT_WIDTH)
            }
        })
        .collect::<Vec<_>>();

    let mut content_minimums = fallback_minimums
        .iter()
        .enumerate()
        .map(|(index, minimum)| {
            if aligns.get(index) == Some(&Align::Right) {
                (*minimum).max(
                    content_widths
                        .numeric
                        .get(index)
                        .copied()
                        .unwrap_or_default()
                        .saturating_add(2),
                )
            } else {
                *minimum
            }
        })
        .collect::<Vec<_>>();
    while cli_table_required_width(&content_minimums) > terminal_width {
        let Some(index) = content_minimums
            .iter()
            .enumerate()
            .filter(|(index, width)| {
                aligns.get(*index) != Some(&Align::Right) && **width > MIN_WRAP_COLUMN_WIDTH
            })
            .max_by_key(|(index, width)| {
                (
                    model_column == Some(*index) || (model_column.is_none() && *index == 1),
                    **width,
                )
            })
            .map(|(index, _)| index)
        else {
            break;
        };
        content_minimums[index] -= 1;
    }
    let natural_widths = widths.clone();
    let mut expansion_limits = natural_widths.clone();
    if compact_dates && expansion_limits.first().is_some() {
        let first = expansion_limits.first_mut().unwrap();
        *first = first_column_min;
    }
    if cli_table_required_width(&content_minimums) <= terminal_width {
        let mut widths = content_minimums;
        let spare = terminal_width.saturating_sub(cli_table_required_width(&widths));
        distribute_spare_width(&mut widths, &expansion_limits, spare);
        return widths;
    }

    let minimums = fallback_minimums;
    let available_width = terminal_width.saturating_sub(widths.len() + 1);
    let total_content_width = widths.iter().sum::<usize>();
    if total_content_width > 0 {
        let scale = available_width as f64 / total_content_width as f64;
        for (index, width) in widths.iter_mut().enumerate() {
            let scaled = (*width as f64 * scale).floor() as usize;
            *width = scaled.max(minimums[index]);
        }
    }

    while cli_table_required_width(&widths) > terminal_width {
        let Some(index) = widths
            .iter()
            .enumerate()
            .filter(|(index, width)| **width > minimums[*index])
            .max_by_key(|(index, width)| (aligns.get(*index) != Some(&Align::Right), **width))
            .map(|(index, _)| index)
        else {
            break;
        };
        widths[index] -= 1;
    }

    let spare = terminal_width.saturating_sub(cli_table_required_width(&widths));
    distribute_spare_width(&mut widths, &expansion_limits, spare);
    widths
}

fn distribute_spare_width(widths: &mut [usize], natural_widths: &[usize], mut spare: usize) {
    if widths.is_empty() {
        return;
    }
    let mut start = 0;
    while spare > 0 {
        let Some(index) = (0..widths.len())
            .map(|offset| (start + offset) % widths.len())
            .find(|index| widths[*index] < natural_widths[*index])
        else {
            break;
        };
        widths[index] += 1;
        spare -= 1;
        start = (index + 1) % widths.len();
    }
}

fn cli_table_required_width(widths: &[usize]) -> usize {
    widths.iter().sum::<usize>() + widths.len() + 1
}

fn wrap_cell_lines(cell: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    for line in cell.lines() {
        if visible_width(line) <= width {
            lines.push(line.to_string());
        } else {
            lines.extend(wrap_cell_line(line, width));
        }
    }
    lines
}

fn wrap_cell_line(line: &str, width: usize) -> Vec<String> {
    let mut remaining = line.split_whitespace();
    let mut words = Vec::new();
    while let Some(word) = remaining.next() {
        if is_list_marker(word) {
            words.push(
                remaining
                    .next()
                    .map_or_else(|| word.to_string(), |next| format!("{word} {next}")),
            );
        } else {
            words.push(word.to_string());
        }
    }
    if words.len() <= 1 {
        return vec![truncate_to_width(line, width)];
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_source = String::new();
    for word in words {
        let candidate_width = if current.is_empty() {
            visible_width(&word)
        } else {
            visible_width(&current) + 1 + visible_width(&word)
        };
        if candidate_width <= width {
            if !current.is_empty() {
                current.push(' ');
                current_source.push(' ');
            }
            current.push_str(&word);
            current_source.push_str(&word);
        } else {
            if !current.is_empty() {
                lines.push((current, current_source));
            }
            if visible_width(&word) > width {
                current = truncate_to_width(&word, width);
                current_source = word;
            } else {
                current_source = word.clone();
                current = word;
            }
        }
    }
    if !current.is_empty() {
        lines.push((current, current_source));
    }
    let mut continuation = String::new();
    lines
        .into_iter()
        .map(|(line, source)| {
            let line = if continuation.is_empty() {
                line
            } else {
                format!("{continuation}{line}")
            };
            continuation = ansi_continuation(&format!("{continuation}{source}"));
            line
        })
        .collect()
}

fn is_list_marker(word: &str) -> bool {
    visible_width(word) == 1 && word.contains('-')
}

pub fn compact_date_cell(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
    {
        Some(format!("{}\n{}", &value[..4], &value[5..]))
    } else {
        None
    }
}

fn table_line(cells: &[String], aligns: &[Align], widths: &[usize]) -> String {
    let mut line = String::from("│");
    for (index, width) in widths.iter().enumerate() {
        let cell = cells.get(index).map(String::as_str).unwrap_or("");
        let align = if index == 0 && cell.starts_with("(assuming ") {
            Align::Right
        } else {
            aligns.get(index).copied().unwrap_or(Align::Left)
        };
        line.push(' ');
        line.push_str(&pad_cell(cell, width.saturating_sub(2), align));
        line.push(' ');
        line.push('│');
    }
    line
}

fn pad_cell(cell: &str, width: usize, align: Align) -> String {
    let cell = ensure_ansi_reset(cell);
    let visible = visible_width(&cell);
    if visible >= width {
        return cell;
    }
    let padding = width - visible;
    match align {
        Align::Left => format!("{cell}{}", " ".repeat(padding)),
        Align::Right => format!("{}{cell}", " ".repeat(padding)),
    }
}

fn border(left: char, middle: char, right: char, widths: &[usize]) -> String {
    let mut line = String::new();
    line.push(left);
    for (index, width) in widths.iter().enumerate() {
        line.push_str(&"─".repeat(*width));
        line.push(if index + 1 == widths.len() {
            right
        } else {
            middle
        });
    }
    line
}

// ---------------------------------------------------------------------------
// shared output formats (ccusage-core/output.rs + project_names.rs)
// ---------------------------------------------------------------------------

pub fn format_number(value: u64) -> String {
    let s = value.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

pub fn format_currency(value: f64) -> String {
    format!("${value:.2}")
}

pub fn short_model_name(model: &str) -> String {
    let model = model
        .strip_prefix("anthropic/claude-")
        .or_else(|| model.strip_prefix("claude-"))
        .unwrap_or(model);
    let parts = model.split('-').collect::<Vec<_>>();
    if parts.len() >= 3 && parts.last().is_some_and(|part| part.len() == 8) {
        return parts[..parts.len() - 1].join("-");
    }
    model.to_string()
}

pub fn format_models_multiline(models: &[String]) -> String {
    let mut models = models
        .iter()
        .map(|model| short_model_name(model))
        .collect::<Vec<_>>();
    models.sort();
    models.dedup();
    models
        .into_iter()
        .map(|model| format!("- {model}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn format_breakdown_model_label(model: &str) -> String {
    format!("  └─ {}", short_model_name(model))
}

pub fn sanitize_terminal_text(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() {
            sanitized.extend(character.escape_default());
        } else {
            sanitized.push(character);
        }
    }
    sanitized
}
