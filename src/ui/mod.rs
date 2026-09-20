//! The terminal UI: the frame, and the panes inside it.
//!
//! The frame is three columns — the agents on the left, whatever is selected
//! in the middle, the chief's tasks and the background processes on the
//! right. Only the left rail is filled in so far; the other two say so rather
//! than showing invented content.
//!
//! Redraws are event-driven, not polled. Three sources feed one channel:
//! keystrokes, the session registry moving, and a slow tick that catches what
//! produces no event of its own — a process that died, and a clock that has
//! to keep showing elapsed time.

pub mod fleet;
pub mod theme;

use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::db::{self, Db};
use crate::registry::{self, Registry, Watcher};
use crate::ui::fleet::Row;

/// How often to redraw when nothing has happened. Slow on purpose: the only
/// things that change without an event are elapsed times and a process that
/// exited without touching its registry file.
const TICK: Duration = Duration::from_secs(2);

enum Msg {
    Key(KeyEvent),
    Registry,
    Tick,
}

pub struct App {
    registry: Registry,
    db: Db,
    root: Option<PathBuf>,
    rows: Vec<Row>,
    selected: usize,
    /// Counts for the right rail until it has panes of its own.
    tasks: usize,
    blocked: usize,
    background: usize,
    status: Option<String>,
    quit: bool,
}

impl App {
    pub fn new(db: Db, root: Option<PathBuf>) -> App {
        App {
            registry: Registry::new(registry::default_dir()),
            db,
            root,
            rows: Vec::new(),
            selected: 0,
            tasks: 0,
            blocked: 0,
            background: 0,
            status: None,
            quit: false,
        }
    }

    /// Re-read both sources and rebuild the rail.
    ///
    /// A failure here is shown rather than fatal: the database is a file
    /// other processes are writing, and a locked moment should not take the
    /// UI down with it.
    pub fn refresh(&mut self) {
        self.registry.refresh();
        let sessions: Vec<_> = self.registry.sessions().cloned().collect();

        match self.db.agents() {
            Ok(agents) => {
                self.rows = fleet::merge(&agents, &sessions, self.root.as_deref());
                self.status = None;
            }
            Err(e) => self.status = Some(format!("board unreadable: {e}")),
        }
        if let Ok(board) = self.db.board() {
            self.tasks = board.iter().filter(|t| t.state != db::State::Done).count();
            self.blocked = board
                .iter()
                .filter(|t| t.state == db::State::Blocked)
                .count();
        }
        if let Ok(bg) = self.db.background() {
            self.background = bg.iter().filter(|b| b.state == "running").count();
        }

        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Char('q'), _) => {
                self.quit = true
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => self.move_by(1),
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => self.move_by(-1),
            (KeyCode::Char('g'), _) => self.selected = 0,
            (KeyCode::Char('G'), _) => self.selected = self.rows.len().saturating_sub(1),
            (KeyCode::Char('r'), _) => self.refresh(),
            _ => {}
        }
    }

    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    pub fn selected(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(Block::default().style(theme::base()), area);

        let [top, body, keys] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);

        self.draw_top(frame, top);

        let [rail, centre, side] = Layout::horizontal([
            Constraint::Length(26),
            Constraint::Min(20),
            Constraint::Length(34),
        ])
        .areas(body);

        fleet::render(frame, rail, &self.rows, self.selected);
        self.draw_centre(frame, centre);
        self.draw_side(frame, side);
        self.draw_keys(frame, keys);
    }

    fn draw_top(&self, frame: &mut Frame, area: Rect) {
        let working = self
            .rows
            .iter()
            .filter(|r| r.presence == fleet::Presence::Working)
            .count();
        let root = self
            .root
            .as_deref()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("all repos");

        let mut spans = vec![
            Span::styled(" fleet", theme::accent().add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(root, theme::faint()),
        ];
        if working > 0 {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("{working} working"),
                Style::default().fg(theme::BUSY),
            ));
        }
        if self.blocked > 0 {
            spans.push(Span::styled("  ·  ", theme::faint()));
            spans.push(Span::styled(
                format!("{} blocked", self.blocked),
                theme::accent(),
            ));
        }
        spans.push(Span::styled(
            format!("  ·  {} agents", self.rows.len()),
            theme::dim(),
        ));

        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_centre(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(theme::BORDER));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines = Vec::new();
        match self.selected() {
            Some(row) => {
                lines.push(Line::from(vec![
                    Span::styled(row.name.clone(), Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                    Span::styled(row.repo.clone(), theme::faint()),
                ]));
                lines.push(Line::from(Span::styled(row.detail.clone(), theme::dim())));
                lines.push(Line::raw(""));
                match &row.session_id {
                    Some(id) => lines.push(Line::from(Span::styled(
                        format!("session {id}"),
                        theme::faint(),
                    ))),
                    None => lines.push(Line::from(Span::styled(
                        "no session linked — spawn or adopt one",
                        theme::faint(),
                    ))),
                }
            }
            None => lines.push(Line::from(Span::styled(
                "nothing selected",
                theme::faint(),
            ))),
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "the session pane goes here",
            theme::faint(),
        )));

        frame.render_widget(Paragraph::new(lines), pad(inner));
    }

    fn draw_side(&self, frame: &mut Frame, area: Rect) {
        let [tasks, bg] =
            Layout::vertical([Constraint::Percentage(55), Constraint::Min(0)]).areas(area);

        let mut top = vec![Line::from(vec![
            Span::styled("TASKS", theme::label()),
            Span::raw("  "),
            Span::styled(format!("{} open", self.tasks), theme::faint()),
        ])];
        top.push(Line::raw(""));
        top.push(Line::from(Span::styled(
            "the board goes here",
            theme::faint(),
        )));
        frame.render_widget(Paragraph::new(top), pad(tasks));

        let divider = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(theme::BORDER));
        let inner = divider.inner(bg);
        frame.render_widget(divider, bg);

        let mut lower = vec![Line::from(vec![
            Span::styled("BACKGROUND", theme::label()),
            Span::raw("  "),
            Span::styled(format!("{} running", self.background), theme::faint()),
        ])];
        lower.push(Line::raw(""));
        lower.push(Line::from(Span::styled(
            "dev servers and runs go here",
            theme::faint(),
        )));
        frame.render_widget(Paragraph::new(lower), pad(inner));
    }

    fn draw_keys(&self, frame: &mut Frame, area: Rect) {
        if let Some(status) = &self.status {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(" {status}"),
                    theme::accent(),
                ))),
                area,
            );
            return;
        }
        let keys = [
            ("↑↓", "agent"),
            ("r", "refresh"),
            ("q", "quit"),
        ];
        let mut spans = vec![Span::raw(" ")];
        for (key, what) in keys {
            spans.push(Span::styled(key, theme::dim()));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(what, theme::faint()));
            spans.push(Span::raw("   "));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

/// One column of breathing room, without spending a border on it.
fn pad(area: Rect) -> Rect {
    Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    }
}

/// Render one frame to stdout and exit.
///
/// The TUI cannot be looked at over a pipe, in CI, or from an agent, and a
/// layout bug that only appears at some width is exactly the kind that
/// survives to someone else's terminal.
pub fn snapshot(db: Db, root: Option<PathBuf>, width: u16, height: u16) -> Result<()> {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = App::new(db, root);
    app.refresh();
    let mut term = Terminal::new(TestBackend::new(width, height))?;
    term.draw(|f| app.draw(f))?;
    print!("{}", term.backend());
    Ok(())
}

pub fn run(db: Db, root: Option<PathBuf>) -> Result<()> {
    let mut app = App::new(db, root);
    app.refresh();

    let (tx, rx) = channel();
    spawn_input(tx.clone());
    spawn_registry(tx.clone());
    spawn_ticker(tx);

    let mut term = ratatui::init();
    let result = (|| -> Result<()> {
        term.draw(|f| app.draw(f))?;
        while let Ok(msg) = rx.recv() {
            match msg {
                Msg::Key(key) => app.on_key(key),
                Msg::Registry | Msg::Tick => app.refresh(),
            }
            if app.quit {
                break;
            }
            term.draw(|f| app.draw(f))?;
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

fn spawn_input(tx: Sender<Msg>) {
    std::thread::spawn(move || {
        loop {
            match event::read() {
                Ok(Event::Key(key)) => {
                    if tx.send(Msg::Key(key)).is_err() {
                        return;
                    }
                }
                // Resize and mouse events still want a redraw.
                Ok(_) => {
                    if tx.send(Msg::Tick).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
}

fn spawn_registry(tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let Ok(watcher) = Watcher::new(&registry::default_dir()) else {
            return; // No watch is survivable: the ticker still refreshes.
        };
        loop {
            watcher.wait(Duration::from_secs(5));
            if tx.send(Msg::Registry).is_err() {
                return;
            }
        }
    });
}

fn spawn_ticker(tx: Sender<Msg>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(TICK);
            if tx.send(Msg::Tick).is_err() {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        let db = Db::open_in_memory().unwrap();
        db.upsert_agent("chief", Some("chief"), None, None, None, None)
            .unwrap();
        db.upsert_agent("billing-svc", None, Some("/repo/content"), None, None, None)
            .unwrap();
        let mut app = App::new(db, Some(PathBuf::from("/nowhere")));
        app.refresh();
        app
    }

    fn drawn(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        format!("{}", term.backend())
    }

    #[test]
    fn the_frame_has_all_three_columns_and_a_key_bar() {
        let out = drawn(&app(), 110, 24);
        assert!(out.contains("fleet"), "{out}");
        assert!(out.contains("FLEET"), "the rail: {out}");
        assert!(out.contains("session pane"), "the centre: {out}");
        assert!(out.contains("TASKS"), "the right rail: {out}");
        assert!(out.contains("BACKGROUND"), "and its lower half: {out}");
        assert!(out.contains("quit"), "the key bar: {out}");
    }

    #[test]
    fn selection_moves_and_stops_at_the_ends() {
        let mut app = app();
        assert_eq!(app.selected().unwrap().name, "chief");

        app.on_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.selected().unwrap().name, "billing-svc");

        // Already at the bottom: staying put beats wrapping to the top, which
        // would move the user somewhere they did not ask to go.
        app.on_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.selected().unwrap().name, "billing-svc");

        app.on_key(KeyEvent::from(KeyCode::Up));
        app.on_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(app.selected().unwrap().name, "chief");
    }

    #[test]
    fn the_centre_follows_the_selection() {
        let mut app = app();
        app.on_key(KeyEvent::from(KeyCode::Down));

        let out = drawn(&app, 110, 24);
        assert!(out.contains("billing-svc"), "{out}");
        assert!(
            out.contains("no session linked"),
            "an agent with no live session says so: {out}"
        );
    }

    #[test]
    fn q_and_ctrl_c_both_quit() {
        let mut with_q = app();
        with_q.on_key(KeyEvent::from(KeyCode::Char('q')));
        assert!(with_q.quit);

        let mut with_ctrl_c = app();
        with_ctrl_c.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(with_ctrl_c.quit);
    }

    #[test]
    fn key_releases_are_ignored_so_one_press_moves_one_row() {
        let mut app = app();
        let mut release = KeyEvent::from(KeyCode::Down);
        release.kind = KeyEventKind::Release;

        app.on_key(release);
        assert_eq!(
            app.selected().unwrap().name,
            "chief",
            "a release must not move the selection a second time"
        );
    }

    #[test]
    fn it_draws_in_a_small_terminal_without_panicking() {
        // Layout maths that overflows a narrow terminal is the classic way a
        // TUI dies on someone else's machine.
        for (w, h) in [(40, 8), (60, 10), (200, 60)] {
            let _ = drawn(&app(), w, h);
        }
    }
}
