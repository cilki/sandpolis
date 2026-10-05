use color_eyre::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, WidgetRef, Wrap};
use std::time::Duration;
use tokio_stream::StreamExt;

pub mod help;
pub mod loading;
pub mod login;
pub mod resident_vec;

/// Time between TUI redraws (30 fps).
const REDRAW_PERIOD: Duration = Duration::from_nanos(1_000_000_000 / 30);

/// Refuse to open a TUI when there's no terminal to open it on.
///
/// `ratatui::init` panics outright in that case ("failed to initialize
/// terminal: No such device or address"), which is what a scripted invocation —
/// a pipe, cron, CI — gets today from subcommands whose default form is a TUI.
/// Those subcommands take `--json` for exactly this situation, so point at it.
pub fn require_terminal() -> anyhow::Result<()> {
    use std::io::IsTerminal;

    if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
        Ok(())
    } else {
        anyhow::bail!(
            "this subcommand opens a terminal UI, but stdin/stdout is not a terminal; \
             pass --json to run noninteractively"
        )
    }
}

/// Run a full-screen TUI driven by a single root widget until the user quits
/// (`q` or Ctrl-C). Sets up and tears down the terminal. Each client subcommand
/// builds its own root and calls this.
pub async fn run_tui<W>(mut root: W) -> anyhow::Result<()>
where
    W: WidgetRef + EventHandler,
{
    require_terminal()?;
    let mut terminal = ratatui::init();
    let mut should_quit = false;
    let mut interval = tokio::time::interval(REDRAW_PERIOD);
    let mut events = EventStream::new();

    while !should_quit {
        tokio::select! {
            _ = interval.tick() => {
                terminal.draw(|frame| root.render_ref(frame.area(), frame.buffer_mut()))?;
            }
            Some(Ok(event)) = events.next() => {
                // The root handler gets first crack; anything it doesn't consume
                // can quit the app.
                if let Some(Event::Key(key)) = root.handle_event(event)
                    && key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Char('q') => should_quit = true,
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                should_quit = true;
                            }
                            _ => {}
                        }
                    }
            }
        }
    }
    ratatui::restore();
    Ok(())
}

/// Like [`run_tui`] but also ends as soon as `done` reports true, for widgets
/// that complete on their own (a login prompt) rather than when the user quits.
pub async fn run_tui_until<W, F>(mut root: W, done: F) -> anyhow::Result<W>
where
    W: WidgetRef + EventHandler,
    F: Fn(&W) -> bool,
{
    require_terminal()?;
    let mut terminal = ratatui::init();
    let mut should_quit = false;
    let mut interval = tokio::time::interval(REDRAW_PERIOD);
    let mut events = EventStream::new();

    while !should_quit && !done(&root) {
        tokio::select! {
            _ = interval.tick() => {
                terminal.draw(|frame| root.render_ref(frame.area(), frame.buffer_mut()))?;
            }
            Some(Ok(event)) = events.next() => {
                if let Some(Event::Key(key)) = root.handle_event(event)
                    && key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Char('q') => should_quit = true,
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                should_quit = true;
                            }
                            _ => {}
                        }
                    }
            }
        }
    }
    ratatui::restore();
    Ok(root)
}

/// A minimal full-screen panel for not-yet-implemented subcommands. Renders a
/// title and a hint, and passes all events through so `run_tui` can quit.
pub struct PlaceholderPanel {
    title: String,
}

impl PlaceholderPanel {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
        }
    }
}

impl WidgetRef for PlaceholderPanel {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .title(self.title.clone())
            .borders(Borders::ALL);
        let inner = block.inner(area);
        block.render(area, buf);
        Paragraph::new("Not yet implemented.\n\nPress q to quit.")
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .render(inner, buf);
    }
}

impl EventHandler for PlaceholderPanel {
    fn handle_event(&mut self, event: Event) -> Option<Event> {
        Some(event)
    }
}

/// Renders a widget for testing/rapid iteration.
pub async fn test_widget<W>(mut widget: W) -> Result<()>
where
    W: WidgetRef + EventHandler,
{
    color_eyre::install()?;
    if let Err(e) = require_terminal() {
        return Err(color_eyre::eyre::eyre!("{e}"));
    }
    let mut terminal = ratatui::init();

    let mut should_quit = false;
    let mut interval = tokio::time::interval(Duration::from_secs_f32(1.0 / 30.0));
    let mut events = EventStream::new();

    while !should_quit {
        tokio::select! {
            _ = interval.tick() => { terminal.draw(|frame| widget.render_ref(frame.area(), frame.buffer_mut()))?; },
            Some(Ok(event)) = events.next() => {
                if let Event::Key(key) = event
                    && key.kind == KeyEventKind::Press
                        && key.code == KeyCode::Esc {
                            should_quit = true;
                        }
                widget.handle_event(event); },
        }
    }
    ratatui::restore();
    Ok(())
}

pub trait EventHandler {
    fn handle_event(&mut self, event: Event) -> Option<Event>;
}

pub trait Panel: WidgetRef + EventHandler {
    fn set_focus(&mut self, focused: bool);
}
