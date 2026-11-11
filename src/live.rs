//! `--live`: a ratatui view that refreshes blocks on a timer.
use crate::agents::AgentReader;
use crate::cli::BlocksArgs;
use crate::pricing::Pricing;
use crate::report::{self, Ctx};
use crate::types::UsageEntry;
use crossterm::event::{self, Event, KeyCode};
use ratatui::prelude::*;
use ratatui::widgets::{Block as RBlock, Borders, Paragraph};
use std::time::{Duration, Instant};

pub fn run(
    readers: &[Box<dyn AgentReader>],
    pricing: &Pricing,
    tz: &jiff::tz::TimeZone,
    args: &BlocksArgs,
    title: &str,
) -> anyhow::Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let ctx = Ctx {
        pricing,
        mode: report::CostMode::Auto,
        tz: tz.clone(),
        merged: readers.len() > 1,
        by_agent: true,
        no_cost: args.report.no_cost,
    };

    let mut entries: Vec<UsageEntry> = vec![];
    let mut last_load = Instant::now() - Duration::from_secs(10);
    let result = loop {
        // reload data every 3s
        if last_load.elapsed() > Duration::from_secs(3) {
            let mut es: Vec<UsageEntry> = readers.iter().flat_map(|r| r.entries()).collect();
            es = crate::agents::dedup(es);
            entries = es;
            last_load = Instant::now();
        }
        let now = jiff::Timestamp::now().as_millisecond();
        let mut bl = report::blocks(&entries, args.session_length, now, pricing, tz);
        if args.active {
            bl.retain(|b| b.is_active);
        }
        let rows: Vec<crate::types::BlockRow> =
            bl.iter().map(|b| crate::block_row(b, &ctx, args, now)).collect();
        let text = crate::render::blocks_table(&rows, title, &[]);
        terminal.draw(|f| {
            let area = f.area();
            let p = Paragraph::new(text)
                .block(RBlock::default().borders(Borders::NONE))
                .style(Style::default());
            f.render_widget(p, area);
        })?;
        if event::poll(Duration::from_millis(500))? {
            if let Event::Key(k) = event::read()? {
                if matches!(k.code, KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('c')) {
                    break Ok(());
                }
            }
        }
    };
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), crossterm::terminal::LeaveAlternateScreen)?;
    result
}
