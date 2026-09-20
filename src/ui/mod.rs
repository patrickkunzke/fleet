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

pub mod board;
pub mod fleet;
pub mod flow;
pub mod picker;
pub mod mirror;
pub mod session;
pub mod theme;

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph};

use crate::agent;
use crate::db::{self, Db};
use crate::registry::{self, Registry, Watcher};
use crate::tmux::Tmux;
use crate::ui::fleet::Row;
use crate::ui::flow::View;
use crate::ui::picker::Picker;

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
    /// The repository picker, while it is open. An overlay rather than a
    /// mode: everything underneath keeps updating behind it.
    picker: Option<Picker>,
    /// Where the database lives, so a worker thread can open its own
    /// connection rather than borrow the one the UI is using.
    db_path: PathBuf,
    tx: Sender<Msg>,
    rx: Option<Receiver<Msg>>,
    tasks: Vec<db::Task>,
    background: Vec<db::BgTask>,
    events: Vec<db::Event>,
    /// What the middle column is showing. The rail selection still drives
    /// the session view underneath, so switching away and back keeps it.
    centre_view: Option<View>,
    event_selected: usize,
    status: Option<String>,
    quit: bool,
}

impl App {
    pub fn new(db: Db, db_path: PathBuf, root: Option<PathBuf>) -> App {
        let (tx, rx) = channel();
        App {
            db_path,
            tx,
            rx: Some(rx),
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
            picker: None,
            tasks: Vec::new(),
            background: Vec::new(),
            events: Vec::new(),
            centre_view: None,
            event_selected: 0,
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
            self.tasks = board;
        }
        if let Ok(bg) = self.db.background() {
            self.background = bg;
        }
        if let Ok(events) = self.db.events(200) {
            self.events = events;
        }
        self.event_selected = self
            .event_selected
            .min(self.events.len().saturating_sub(1));

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
        // The overlay and insert mode both take every key, before the ones
        // that would quit: while typing, q is a letter.
        if self.picker.is_some() {
            self.picker_key(key);
            return;
        }
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
            // Pressing the same key again puts the session back, so neither
            // view is a place you get stuck in.
            (KeyCode::Char('g'), _) => {
                self.centre_view = (self.centre_view != Some(View::Graph)).then_some(View::Graph);
                return;
            }
            (KeyCode::Char('l'), _) => {
                self.centre_view = (self.centre_view != Some(View::Log)).then_some(View::Log);
                self.focus = Focus::Session;
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
            KeyCode::Home => {
                self.selected = 0;
                self.retarget();
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = self.rows.len().saturating_sub(1);
                self.retarget();
            }
            KeyCode::Char('n') => self.open_picker(),
            _ => {}
        }
    }

    /// Select the agent an event came from, and go back to watching it.
    fn jump_to_event(&mut self) {
        let Some(event) = self.events.get(self.event_selected) else {
            return;
        };
        let Some(who) = event.from_agent.clone().filter(|w| !w.is_empty()) else {
            return;
        };
        if let Some(at) = self.rows.iter().position(|r| r.name == who) {
            self.selected = at;
            self.centre_view = None;
            self.focus = Focus::Rail;
            self.retarget();
        } else {
            self.status = Some(format!("{who} is not on the rail any more"));
        }
    }

    fn open_picker(&mut self) {
        let Some(root) = self.root.clone() else {
            self.status = Some("start fleet with --root to spawn from here".into());
            return;
        };
        let taken: Vec<String> = self
            .db
            .agents()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|a| a.repo)
            .collect();
        self.picker = Some(Picker::new(agent::candidates(&root, &taken)));
    }

    fn picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Up => picker.move_by(-1),
            KeyCode::Down => picker.move_by(1),
            KeyCode::Backspace => picker.backspace(),
            KeyCode::Char(c) => picker.push(c),
            KeyCode::Enter => {
                // Nothing matched: Enter is not a command to start something
                // arbitrary.
                let Some(chosen) = picker.chosen().cloned() else {
                    return;
                };
                self.picker = None;
                self.start_agent(&chosen);
            }
            _ => {}
        }
    }

    /// Open the pane now, and let the session catch up on its own.
    ///
    /// Waiting here for Claude Code to register itself would freeze the UI
    /// for several seconds on every spawn, which is how a key stops being
    /// worth pressing.
    fn start_agent(&mut self, chosen: &agent::Candidate) {
        let Some(tmux) = self.tmux.clone() else {
            self.status = Some("no tmux — agents are started in tmux panes".into());
            return;
        };
        let spawned = match agent::start(&tmux, &self.db, &chosen.name, &chosen.path, "claude") {
            Ok(s) => s,
            Err(e) => {
                self.status = Some(format!("cannot start {}: {e}", chosen.name));
                return;
            }
        };

        self.status = Some(format!("started {} — waiting for its session", spawned.name));
        self.refresh();
        // Select what was just started: pressing the key was the intent.
        if let Some(at) = self.rows.iter().position(|r| r.name == spawned.name) {
            self.selected = at;
            self.retarget();
        }

        let db_path = self.db_path.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let Some(found) = agent::adopt(spawned.pane.pid, Duration::from_secs(30)) else {
                return;
            };
            // A separate connection: this thread cannot borrow the one the UI
            // is using, and SQLite in WAL mode is happy with both.
            if let Ok(db) = Db::open(&db_path) {
                let _ = agent::link(&db, &spawned.name, &found.session_id);
            }
            let _ = tx.send(Msg::Registry);
        });
    }

    fn session_key(&mut self, key: KeyEvent) {
        // The log owns the arrows while it is up.
        if self.centre_view == Some(View::Log) {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.event_selected = self.event_selected.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.event_selected =
                        (self.event_selected + 1).min(self.events.len().saturating_sub(1))
                }
                // Jump to whoever the event is about, and show their session.
                KeyCode::Enter => self.jump_to_event(),
                _ => {}
            }
            return;
        }
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
        match self.centre_view {
            Some(view) => flow::render(
                frame,
                centre,
                view,
                &self.events,
                &self.rows,
                self.event_selected,
            ),
            None => {
                let row = self.rows.get(self.selected).cloned();
                let input = (self.mode == Mode::Insert).then_some(&self.input);
                self.centre.render(frame, centre, row.as_ref(), input);
            }
        }
        self.draw_side(frame, side);
        self.draw_keys(frame, keys);

        if let Some(picker) = &self.picker {
            picker.render(frame, area);
        }
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
        let blocked = self
            .tasks
            .iter()
            .filter(|t| t.state == db::State::Blocked)
            .count();
        if blocked > 0 {
            spans.push(Span::styled("  ·  ", theme::faint()));
            spans.push(Span::styled(format!("{blocked} blocked"), theme::accent()));
        }
        spans.push(Span::styled(
            format!("  ·  {} agents", self.rows.len()),
            theme::dim(),
        ));

        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_side(&self, frame: &mut Frame, area: Rect) {
        board::render(
            frame,
            area,
            &self.tasks,
            &self.background,
            self.selected().map(|r| r.name.as_str()),
        );
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
            _ if self.centre_view == Some(View::Log) => &[
                ("↑↓", "event"),
                ("↵", "jump to agent"),
                ("l", "back"),
                ("g", "graph"),
                ("q", "quit"),
            ],
            _ if self.centre_view == Some(View::Graph) => &[
                ("g", "back"),
                ("l", "log"),
                ("n", "new agent"),
                ("q", "quit"),
            ],
            Focus::Rail => &[
                ("↑↓", "agent"),
                ("n", "new agent"),
                ("g", "graph"),
                ("l", "log"),
                ("tab", "session"),
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

pub fn snapshot(
    db: Db,
    db_path: PathBuf,
    root: Option<PathBuf>,
    width: u16,
    height: u16,
    view: Option<&str>,
) -> Result<()> {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = App::new(db, db_path, root);
    app.refresh();
    app.centre_view = match view {
        Some("graph") => Some(View::Graph),
        Some("log") => Some(View::Log),
        _ => None,
    };
    let mut term = Terminal::new(TestBackend::new(width, height))?;
    term.draw(|f| app.draw(f))?;
    print!("{}", term.backend());
    Ok(())
}

pub fn run(db: Db, db_path: PathBuf, root: Option<PathBuf>) -> Result<()> {
    let mut app = App::new(db, db_path, root);
    app.refresh();

    let rx = app.rx.take().expect("the app owns its channel until run takes it");
    let tx = app.tx.clone();
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
        let mut app = App::new(db, PathBuf::from(":memory:"), Some(PathBuf::from("/nowhere")));
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
        assert!(
            out.contains("nothing on the board"),
            "an empty board says so rather than showing a heading and a void: {out}"
        );
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
    fn g_and_l_swap_the_centre_and_put_it_back() {
        let mut app = app();
        assert!(app.centre_view.is_none());

        app.on_key(KeyEvent::from(KeyCode::Char('g')));
        assert_eq!(app.centre_view, Some(View::Graph));
        assert!(drawn(&mut app, 110, 24).contains("topology"));

        app.on_key(KeyEvent::from(KeyCode::Char('l')));
        assert_eq!(app.centre_view, Some(View::Log), "l switches straight across");
        assert!(drawn(&mut app, 110, 24).contains("chronological"));

        app.on_key(KeyEvent::from(KeyCode::Char('l')));
        assert!(app.centre_view.is_none(), "the same key again goes back");
    }

    #[test]
    fn the_log_takes_the_arrows_while_it_is_up() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_agent("chief", Some("chief"), None, None, None, None)
            .unwrap();
        for i in 0..3 {
            db.log_event("message", Some("chief"), Some("worker"), None, &format!("m{i}"), None, None)
                .unwrap();
        }
        let mut app = App::new(db, PathBuf::from(":memory:"), None);
        app.refresh();

        app.on_key(KeyEvent::from(KeyCode::Char('l')));
        assert_eq!(app.event_selected, 0);

        app.on_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.event_selected, 1, "the arrows move the event, not the agent");

        // And they stop at the end rather than wrapping.
        for _ in 0..10 {
            app.on_key(KeyEvent::from(KeyCode::Down));
        }
        assert_eq!(app.event_selected, app.events.len() - 1);
    }

    #[test]
    fn enter_in_the_log_goes_to_the_agent_the_event_came_from() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_agent("chief", Some("chief"), None, None, None, None)
            .unwrap();
        db.upsert_agent("billing-svc", None, Some("/repo"), None, None, None)
            .unwrap();
        db.log_event("message", Some("billing-svc"), Some("chief"), None, "blocked", None, None)
            .unwrap();

        let mut app = App::new(db, PathBuf::from(":memory:"), None);
        app.refresh();
        app.on_key(KeyEvent::from(KeyCode::Char('l')));
        app.on_key(KeyEvent::from(KeyCode::Enter));

        assert!(app.centre_view.is_none(), "it returns to watching that agent");
        assert_eq!(app.selected().unwrap().name, "billing-svc");
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
    fn n_opens_the_picker_and_escape_closes_it_without_starting_anything() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("storefront/.git")).unwrap();
        std::fs::create_dir_all(root.path().join("billing-service/.git")).unwrap();

        let db = Db::open_in_memory().unwrap();
        let mut app = App::new(
            db,
            PathBuf::from(":memory:"),
            Some(root.path().to_path_buf()),
        );
        app.refresh();

        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(app.picker.is_some());

        let out = drawn(&mut app, 110, 24);
        assert!(out.contains("start an agent in"), "{out}");
        assert!(out.contains("storefront"), "{out}");

        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.picker.is_none());
        assert!(
            app.rows.is_empty(),
            "cancelling must not have started anything"
        );
    }

    #[test]
    fn the_picker_swallows_the_keys_that_would_otherwise_act() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("storefront/.git")).unwrap();
        let db = Db::open_in_memory().unwrap();
        let mut app = App::new(
            db,
            PathBuf::from(":memory:"),
            Some(root.path().to_path_buf()),
        );
        app.refresh();
        app.on_key(KeyEvent::from(KeyCode::Char('n')));

        app.on_key(KeyEvent::from(KeyCode::Char('q')));
        assert!(!app.quit, "q types into the filter, it does not quit");
        assert!(app.picker.is_some());
    }

    #[test]
    fn spawning_without_a_root_says_why_rather_than_opening_an_empty_picker() {
        let db = Db::open_in_memory().unwrap();
        let mut app = App::new(db, PathBuf::from(":memory:"), None);
        app.refresh();

        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(app.picker.is_none());
        assert!(app.status.unwrap().contains("--root"));
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
