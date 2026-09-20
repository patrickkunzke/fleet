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
pub mod mirror;
pub mod session;
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
use crate::tmux::Tmux;
use crate::ui::fleet::Row;

/// How often to redraw when nothing has happened. Slow on purpose: the only
/// things that change without an event are elapsed times and a process that
/// exited without touching its registry file.
const TICK: Duration = Duration::from_secs(2);

enum Msg {
    Key(KeyEvent),
    Registry,
    Tick,
    /// The selected session may have written something. Far more frequent
    /// than the others and usually finds nothing, so it redraws only when
    /// the transcript actually grew.
    Transcript,
}

/// Which half of the frame the arrow keys belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Rail,
    Session,
}

/// In Insert every key belongs to the agent, including the ones that would
/// otherwise quit. Leaving is Ctrl-], the old telnet escape, chosen because
/// Claude Code wants Escape for interrupting a turn and Ctrl-C for its own
/// purposes — binding either of those to "leave" would make the two most
/// important keys in a runaway turn do the wrong thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Insert,
}

pub struct App {
    registry: Registry,
    db: Db,
    root: Option<PathBuf>,
    projects: PathBuf,
    tmux: Option<Tmux>,
    /// The centre column as last drawn, so a mirror can be attached at the
    /// right size before its first frame.
    centre_size: (u16, u16),
    rows: Vec<Row>,
    selected: usize,
    centre: session::Pane,
    focus: Focus,
    mode: Mode,
    input: session::Input,
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
            projects: registry::default_projects_dir(),
            // No tmux is survivable: every agent then falls back to its
            // transcript, which is exactly the non-local case.
            tmux: Tmux::detect(None).ok(),
            centre_size: (80, 24),
            rows: Vec::new(),
            selected: 0,
            centre: session::Pane::default(),
            focus: Focus::Rail,
            mode: Mode::Normal,
            input: session::Input::default(),
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
        self.retarget();
    }

    /// Point the centre pane at the selection. Cheap when it has not changed.
    fn retarget(&mut self) {
        let row = self.selected().cloned();
        self.centre
            .follow(row.as_ref(), &self.projects, self.tmux.as_ref(), self.centre_size);
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        // Insert mode comes first, before the keys that would quit: while
        // typing, q is a letter and Ctrl-C is an interrupt for the agent.
        if self.mode == Mode::Insert {
            self.insert_key(key);
            return;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Char('q'), _) => {
                self.quit = true;
                return;
            }
            (KeyCode::Tab, _) => {
                self.focus = match self.focus {
                    Focus::Rail => Focus::Session,
                    Focus::Session => Focus::Rail,
                };
                return;
            }
            (KeyCode::Char('r'), _) => {
                self.refresh();
                return;
            }
            _ => {}
        }

        match self.focus {
            Focus::Rail => self.rail_key(key),
            Focus::Session => self.session_key(key),
        }
    }

    fn rail_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Char('g') => {
                self.selected = 0;
                self.retarget();
            }
            KeyCode::Char('G') => {
                self.selected = self.rows.len().saturating_sub(1);
                self.retarget();
            }
            _ => {}
        }
    }

    fn session_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('i') && self.centre.is_live() {
            self.mode = Mode::Insert;
            self.status = None;
            return;
        }
        if key.code == KeyCode::Enter {
            // Hand over the real terminal. Inside tmux this moves you there;
            // outside it, the pane is selected and we say how to attach.
            if let Some(tmux) = self.tmux.as_ref() {
                self.status = self.centre.zoom(tmux);
            }
            return;
        }
        // A mirrored pane is the live screen: there is no history to scroll
        // through, and tmux owns the scrollback.
        if self.centre.is_live() {
            return;
        }
        // Up scrolls back through history, which means increasing the offset
        // from the bottom.
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.centre.scroll_by(1, 1),
            KeyCode::Down | KeyCode::Char('j') => self.centre.scroll_by(-1, 1),
            KeyCode::PageUp => self.centre.scroll_by(1, 10),
            KeyCode::PageDown => self.centre.scroll_by(-1, 10),
            KeyCode::Char('G') | KeyCode::End => self.centre.to_tail(),
            _ => {}
        }
    }

    /// Everything typed while the agent has the keyboard.
    fn insert_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            // Ctrl-] hands the keyboard back.
            KeyCode::Char(']') if ctrl => {
                self.mode = Mode::Normal;
                return;
            }
            KeyCode::Char('c') if ctrl => {
                self.forward("C-c");
                return;
            }
            _ => {}
        }

        match key.code {
            KeyCode::Char(c) => self.input.insert(c),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Enter => {
                let line = self.input.take();
                self.send(&line);
            }
            // With something typed, Escape is "forget it"; with an empty line
            // it is the agent's interrupt, which is the only way to stop a
            // turn that has gone wrong.
            KeyCode::Esc if !self.input.is_empty() => self.input.clear(),
            KeyCode::Esc => self.forward("Escape"),
            // A permission prompt is answered by moving and pressing Enter,
            // and no line of text can do that.
            KeyCode::Up if self.input.is_empty() => self.forward("Up"),
            KeyCode::Down if self.input.is_empty() => self.forward("Down"),
            KeyCode::Tab if self.input.is_empty() => self.forward("Tab"),
            _ => {}
        }
    }

    fn send(&mut self, line: &str) {
        let Some(tmux) = self.tmux.clone() else { return };
        match self.centre.send(&tmux, line) {
            Ok(()) => self.echo(),
            Err(e) => self.status = Some(e.to_string()),
        }
    }

    fn forward(&mut self, key: &str) {
        let Some(tmux) = self.tmux.clone() else { return };
        match self.centre.send_key(&tmux, key) {
            Ok(()) => self.echo(),
            Err(e) => self.status = Some(e.to_string()),
        }
    }

    /// Wait briefly for the pane to echo what was just sent.
    ///
    /// Without this a keystroke is invisible until the next poll, up to
    /// 400ms later, which reads as a dropped key. Bounded tightly: an agent
    /// that is busy will not echo at all, and the loop must not stall for it.
    fn echo(&mut self) {
        for _ in 0..4 {
            std::thread::sleep(Duration::from_millis(15));
            if self.centre.poll() {
                return;
            }
        }
    }

    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        self.retarget();
    }

    pub fn selected(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    pub fn draw(&mut self, frame: &mut Frame) {
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

        self.centre_size = (centre.width.saturating_sub(3), centre.height.saturating_sub(2));
        fleet::render(frame, rail, &self.rows, self.selected);
        let row = self.rows.get(self.selected).cloned();
        let input = (self.mode == Mode::Insert).then_some(&self.input);
        self.centre.render(frame, centre, row.as_ref(), input);
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
        let keys: &[(&str, &str)] = match self.focus {
            _ if self.mode == Mode::Insert => &[
                ("typing", "→ pane"),
                ("esc", "interrupt"),
                ("^]", "stop typing"),
            ],
            Focus::Rail => &[
                ("↑↓", "agent"),
                ("tab", "session"),
                ("r", "refresh"),
                ("q", "quit"),
            ],
            Focus::Session if self.centre.is_live() => &[
                ("i", "type"),
                ("↵", "zoom to pane"),
                ("tab", "agents"),
                ("q", "quit"),
            ],
            Focus::Session => &[
                ("↑↓", "scroll"),
                ("G", "live"),
                ("↵", "zoom to pane"),
                ("tab", "agents"),
                ("q", "quit"),
            ],
        };
        let mut spans = vec![Span::raw(" ")];
        for (key, what) in keys.iter().copied() {
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
    spawn_transcript_poll(tx.clone());
    spawn_ticker(tx);

    let mut term = ratatui::init();
    let result = (|| -> Result<()> {
        term.draw(|f| app.draw(f))?;
        while let Ok(msg) = rx.recv() {
            match msg {
                Msg::Key(key) => app.on_key(key),
                Msg::Registry | Msg::Tick => app.refresh(),
                Msg::Transcript => {
                    // Nothing new is the common case; redrawing anyway would
                    // burn a frame twice a second for no visible change.
                    if !app.centre.poll() {
                        continue;
                    }
                }
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

/// A live agent writes to its transcript continuously, and those writes
/// produce no event this loop would otherwise see.
fn spawn_transcript_poll(tx: Sender<Msg>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(400));
            if tx.send(Msg::Transcript).is_err() {
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

    fn drawn(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        format!("{}", term.backend())
    }

    #[test]
    fn the_frame_has_all_three_columns_and_a_key_bar() {
        let out = drawn(&mut app(), 110, 24);
        assert!(out.contains("fleet"), "{out}");
        assert!(out.contains("FLEET"), "the rail: {out}");
        assert!(out.contains("no session linked"), "the centre: {out}");
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

        let out = drawn(&mut app,  110, 24);
        assert!(out.contains("billing-svc"), "{out}");
        assert!(
            out.contains("no session linked"),
            "an agent with no live session says so: {out}"
        );
    }

    #[test]
    fn typing_is_not_offered_where_there_is_no_pane_to_type_into() {
        let mut app = app();
        app.on_key(KeyEvent::from(KeyCode::Tab));
        app.on_key(KeyEvent::from(KeyCode::Char('i')));
        assert_eq!(
            app.mode,
            Mode::Normal,
            "a transcript-only agent cannot be typed at"
        );
    }

    #[test]
    fn insert_mode_keeps_the_keys_that_would_otherwise_quit() {
        let mut app = app();
        app.mode = Mode::Insert;

        app.on_key(KeyEvent::from(KeyCode::Char('q')));
        assert!(!app.quit, "q is a letter while typing");

        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.quit, "and Ctrl-C interrupts the agent, not the program");

        app.on_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL));
        assert_eq!(app.mode, Mode::Normal, "Ctrl-] hands the keyboard back");

        app.on_key(KeyEvent::from(KeyCode::Char('q')));
        assert!(app.quit, "and then q quits again");
    }

    #[test]
    fn escape_clears_a_typed_line_but_interrupts_an_empty_one() {
        let mut app = app();
        app.mode = Mode::Insert;
        for c in "half a thought".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        assert!(!app.input.is_empty());

        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.input.is_empty(), "the first Escape discards the line");
        assert_eq!(app.mode, Mode::Insert, "and stays in insert mode");
    }

    #[test]
    fn enter_on_a_transcript_only_agent_does_not_pretend_to_zoom() {
        let mut app = app();
        app.on_key(KeyEvent::from(KeyCode::Tab));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        // Nothing to zoom to, so nothing is claimed.
        assert!(app.status.is_none());
    }

    #[test]
    fn tab_moves_the_arrow_keys_between_the_rail_and_the_session() {
        let mut app = app();
        assert_eq!(app.focus, Focus::Rail);
        assert!(drawn(&mut app,  110, 24).contains("agent"));

        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Session);

        // Down no longer changes which agent is selected.
        let before = app.selected().unwrap().name.clone();
        app.on_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.selected().unwrap().name, before);
        assert!(drawn(&mut app,  110, 24).contains("scroll"), "the key bar follows focus");

        app.on_key(KeyEvent::from(KeyCode::Tab));
        app.on_key(KeyEvent::from(KeyCode::Down));
        assert_ne!(app.selected().unwrap().name, before);
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
            let _ = drawn(&mut app(), w, h);
        }
    }
}
