//! `--live`: a ratatui view that refreshes blocks on a timer.
use crate::agents::AgentReader;
use crate::cli::BlocksArgs;
use crate::report::Ctx;
use crossterm::event::{self, Event, KeyCode};
use ratatui::prelude::*;
use ratatui::widgets::{Block as RBlock, Borders, Paragraph};
use std::time::{Duration, Instant};

pub fn run(
    readers: &[Box<dyn AgentReader>],
    ctx: &Ctx,
    args: &BlocksArgs,
    title: &str,
) -> anyhow::Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, readers, ctx, args, title);
    // always restore the terminal, even if the loop errored
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen
    );
    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    readers: &[Box<dyn AgentReader>],
    ctx: &Ctx,
    args: &BlocksArgs,
    title: &str,
) -> anyhow::Result<()> {
    let mut entries = vec![];
    let mut last_load = Instant::now() - Duration::from_secs(10);
    loop {
        if last_load.elapsed() > Duration::from_secs(3) {
            entries = crate::app::load_entries(readers);
            crate::app::apply_filters(&mut entries, &args.filter, &ctx.tz);
            last_load = Instant::now();
        }
        let now = jiff::Timestamp::now().as_millisecond();
        let rows = crate::app::block_rows(&entries, args, ctx, now);
        let detected = if ctx.merged {
            crate::app::detected_names(&entries)
        } else {
            vec![]
        };
        let text = crate::render::blocks_table(&rows, title, ctx.merged, &detected);
        terminal.draw(|f| {
            let area = f.area();
            let p = Paragraph::new(text)
                .block(RBlock::default().borders(Borders::NONE))
                .style(Style::default());
            f.render_widget(p, area);
        })?;
        if event::poll(Duration::from_millis(500))? {
            if let Event::Key(k) = event::read()? {
                if matches!(
                    k.code,
                    KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('c')
                ) {
                    return Ok(());
                }
            }
        }
    }
}
