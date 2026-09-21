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
pub mod clipboard;
pub mod fleet;
pub mod flow;
pub mod keys;
pub mod picker;
pub mod mirror;
pub mod preview;
pub mod selection;
pub mod session;
pub mod theme;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph};

use crate::agent;
use crate::brief;
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
    Mouse(MouseEvent),
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

/// Fleet's own key, borrowed from every multiplexer there has ever been.
///
/// Typing belongs to the agent, so fleet cannot also own the alphabet. Ctrl-A
/// rather than tmux's Ctrl-B, because `↵` hands you to tmux and the two must
/// not be the same key. Ctrl-A twice sends a literal one through.
const PREFIX: char = 'a';

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
    /// Where each pane was last drawn, so a click can be routed to it.
    rail_at: Rect,
    centre_at: Rect,
    /// Whether fleet is taking the mouse. While it does, the terminal
    /// cannot select text, so this is something you can give back.
    mouse: bool,
    /// True between the prefix and the key it modifies.
    armed: bool,
    /// The repository picker, while it is open. An overlay rather than a
    /// mode: everything underneath keeps updating behind it.
    picker: Option<Picker>,
    /// Where the database lives, so a worker thread can open its own
    /// connection rather than borrow the one the UI is using.
    db_path: PathBuf,
    tx: Sender<Msg>,
    rx: Option<Receiver<Msg>>,
    /// Rails folded away, leaving the agent's terminal the whole window.
    wide: bool,
    /// Set when the user asked for the real terminal. Acted on by the run
    /// loop, which owns the screen — the key handler does not.
    wants_attach: bool,
    tasks: Vec<db::Task>,
    background: Vec<db::BgTask>,
    events: Vec<db::Event>,
    /// What the middle column is showing. The rail selection still drives
    /// the session view underneath, so switching away and back keeps it.
    centre_view: Option<View>,
    event_selected: usize,
    status: Option<String>,
    /// Whether the message on screen is one refresh put there, and so one
    /// refresh may take away.
    status_is_db_error: bool,
    quit: bool,
}

impl App {
    pub fn new(db: Db, db_path: PathBuf, root: Option<PathBuf>) -> App {
        let (tx, rx) = channel();
        App {
            db_path,
            tx,
            rx: Some(rx),
            wide: false,
            wants_attach: false,
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
            // The agent has the keyboard by default; that is the whole
            // point of a prefix.
            focus: Focus::Session,
            rail_at: Rect::ZERO,
            centre_at: Rect::ZERO,
            mouse: true,
            armed: false,
            picker: None,
            tasks: Vec::new(),
            background: Vec::new(),
            events: Vec::new(),
            centre_view: None,
            event_selected: 0,
            status: None,
            status_is_db_error: false,
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
                self.rows = fleet::merge(&agents, &sessions);
                // Only clear what refresh itself reported. Anything else on
                // screen was said by an action, and a refresh two ticks
                // later must not swallow it — which is how "started X" had
                // been going unseen.
                if self.status_is_db_error {
                    self.status = None;
                    self.status_is_db_error = false;
                }
            }
            Err(e) => {
                self.status = Some(format!("board unreadable: {e}"));
                self.status_is_db_error = true;
            }
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
        if self.picker.is_some() {
            self.picker_key(key);
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.armed {
            self.armed = false;
            // The prefix twice means the prefix itself, as everywhere else.
            if ctrl && key.code == KeyCode::Char(PREFIX) {
                self.forward(key);
            } else {
                self.command(key);
            }
            return;
        }
        if ctrl && key.code == KeyCode::Char(PREFIX) {
            self.armed = true;
            return;
        }

        // With a pane in front of you and the keyboard pointed at it, every
        // key is the agent's. Otherwise — a transcript, the flow, the rail —
        // there is nothing to type into, so keys act directly.
        if self.focus == Focus::Session && self.centre_view.is_none() && self.centre.is_live() {
            self.forward(key);
        } else {
            self.command(key);
        }
    }

    /// One of fleet's own keys.
    fn command(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Char('q'), _) => {
                self.quit = true
            }
            (KeyCode::Tab, _) => {
                self.focus = match self.focus {
                    Focus::Rail => Focus::Session,
                    Focus::Session => Focus::Rail,
                }
            }
            (KeyCode::Char('r'), _) => self.refresh(),
            (KeyCode::Char('z'), _) => {
                self.wide = !self.wide;
                self.focus = Focus::Session;
            }
            (KeyCode::Char('g'), _) => {
                self.centre_view = (self.centre_view != Some(View::Graph)).then_some(View::Graph)
            }
            (KeyCode::Char('l'), _) => {
                self.centre_view = (self.centre_view != Some(View::Log)).then_some(View::Log)
            }
            (KeyCode::Char('m'), _) => self.take_mouse(!self.mouse),
            (KeyCode::Char('n'), _) => self.open_picker(),
            (KeyCode::Char('x'), _) => self.retire_selected(),
            (KeyCode::Enter, _) if self.centre_view.is_none() => self.hand_over(),
            _ if self.centre_view == Some(View::Log) => self.log_key(key),
            // Paging and the wheel move a transcript; the arrows do not.
            // Keys only reach here when nothing is live to type into, and in
            // that state switching agent is what they are wanted for.
            (KeyCode::PageUp | KeyCode::PageDown | KeyCode::End, _) => {
                self.transcript_key(key)
            }
            _ => self.rail_key(key),
        }
    }

    /// Hand a keystroke to the agent.
    fn forward(&mut self, key: KeyEvent) {
        let Some(translated) = keys::translate(key) else {
            return;
        };
        let Some(tmux) = self.tmux.clone() else { return };
        // Typing returns you to the live edge: writing into a screen you are
        // scrolled away from shows nothing happening. It also moves the text
        // a selection was drawn over, so the selection goes with it.
        self.centre.mirror_to_live();
        self.centre.clear_selection();
        match self.centre.send(&tmux, &translated) {
            Ok(()) => self.echo(),
            Err(e) => self.status = Some(e.to_string()),
        }
    }

    /// Give the user the actual terminal.
    fn hand_over(&mut self) {
        if Tmux::inside() {
            if let Some(tmux) = self.tmux.as_ref() {
                self.status = self.centre.zoom(tmux);
            }
        } else if self.centre.is_live() {
            self.wants_attach = true;
        }
    }

    fn transcript_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::PageUp => self.centre.scroll_by(1, 10),
            KeyCode::PageDown => self.centre.scroll_by(-1, 10),
            KeyCode::End => self.centre.to_tail(),
            _ => {}
        }
    }

    fn log_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.event_selected = self.event_selected.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.event_selected =
                    (self.event_selected + 1).min(self.events.len().saturating_sub(1))
            }
            KeyCode::Enter => self.jump_to_event(),
            _ => {}
        }
    }

    /// Clicks and the wheel. The reason there is a prefix at all: a mouse
    /// needs no mode.
    fn on_mouse(&mut self, ev: MouseEvent) {
        let at = (ev.column, ev.row);
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if inside(self.rail_at, at) {
                    self.focus = Focus::Rail;
                    if let Some(i) =
                        fleet::row_at(self.rail_at, ev.row, self.rows.len(), self.selected)
                        && i < self.rows.len()
                    {
                        self.selected = i;
                        self.retarget();
                    }
                } else if inside(self.centre_at, at) {
                    // Clicking a terminal is how you say "talk to this one".
                    self.focus = Focus::Session;
                    // And it is where a drag over its output begins.
                    self.centre.press(ev.column, ev.row);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => self.centre.drag(ev.column, ev.row),
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(text) = self.centre.release() {
                    let lines = text.lines().count();
                    let s = if lines == 1 { "" } else { "s" };
                    self.status = Some(if clipboard::copy(&text) {
                        format!("copied {lines} line{s}")
                    } else {
                        "nothing here can reach the clipboard".into()
                    });
                }
            }
            // The live pane and the transcript each keep their own history,
            // so the wheel means the same thing over either.
            MouseEventKind::ScrollUp if inside(self.centre_at, at) => {
                if !self.centre.scroll_mirror(3) {
                    self.centre.scroll_by(1, 3);
                }
            }
            MouseEventKind::ScrollDown if inside(self.centre_at, at) => {
                if !self.centre.scroll_mirror(-3) {
                    self.centre.scroll_by(-1, 3);
                }
            }
            MouseEventKind::ScrollUp => self.rail_key(KeyEvent::from(KeyCode::Up)),
            MouseEventKind::ScrollDown => self.rail_key(KeyEvent::from(KeyCode::Down)),
            _ => {}
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

    fn start_agent(&mut self, chosen: &agent::Candidate) {
        // Started by hand rather than dispatched, so there is no task yet.
        // The brief says how to go and look for one, which is better than an
        // agent sitting at an empty prompt waiting to be told it exists.
        let brief = brief::worker(&chosen.name, &chosen.path, None, None);
        self.launch(
            &chosen.name,
            &chosen.path.clone(),
            agent::Naming::Unique,
            &brief,
        );
    }

    /// Take the selected agent off the rail.
    ///
    /// Its pane is left alone: the row is fleet's bookkeeping, and killing
    /// somebody's running session because they tidied a list would be a
    /// surprising thing for a list to do.
    fn retire_selected(&mut self) {
        let Some(row) = self.selected().cloned() else { return };
        match self.db.retire_agent(&row.name) {
            Ok(()) => {
                self.status = Some(format!("{} is off the rail; its pane is untouched", row.name));
                self.refresh();
            }
            Err(e) => self.status = Some(e.to_string()),
        }
    }

    /// Clear out agents that are finished with.
    ///
    /// Quitting fleet leaves every agent's row behind with its session gone,
    /// so without this the rail fills with the dead from previous runs. Only
    /// the ones holding no task: an agent that died mid-task is exactly what
    /// somebody needs to see.
    fn prune_dead(&mut self) {
        let dead: Vec<String> = self
            .rows
            .iter()
            .filter(|r| r.presence == fleet::Presence::Gone && r.detail == "session ended")
            .map(|r| r.name.clone())
            .collect();
        for name in dead {
            let _ = self.db.retire_agent(&name);
        }
        self.refresh();
    }

    /// Take the mouse, or hand it back to the terminal.
    ///
    /// A terminal selects text with the mouse, and an application that
    /// reports mouse events takes that away: there is no way to have both,
    /// and no way for us to put a selection on the clipboard on the
    /// terminal's behalf. So it is a switch. Clicking a pane and scrolling
    /// one stop working while it is off, which is a fair trade for being
    /// able to copy what an agent just said.
    fn take_mouse(&mut self, take: bool) {
        use std::io::stdout;
        let done = if take {
            crossterm::execute!(stdout(), EnableMouseCapture)
        } else {
            crossterm::execute!(stdout(), DisableMouseCapture)
        };
        if done.is_err() {
            self.status = Some("the terminal would not change mouse reporting".into());
            return;
        }
        self.mouse = take;
        self.status = take.then(|| "mouse back to fleet — click a pane, scroll it".into());
    }

    /// Make sure there is someone to brief.
    ///
    /// The chief is the first thing the workflow needs and the last thing
    /// anyone wants to set up by hand, so fleet starts one in the workspace
    /// if there is not a live one already. It is an ordinary Claude Code
    /// session; the board skill is what makes it a chief of staff.
    fn ensure_chief(&mut self) {
        let live = self.rows.iter().any(|r| {
            r.role == fleet::Role::Chief
                && matches!(r.presence, fleet::Presence::Working | fleet::Presence::Waiting)
        });
        if live {
            return;
        }
        let Some(root) = self.root.clone() else { return };
        self.launch(
            "chief",
            &root,
            agent::Naming::Exact,
            &brief::chief(&root),
        );
        if let Err(e) = self.db.upsert_agent("chief", Some("chief"), None, None, None, None) {
            self.status = Some(format!("cannot record the chief: {e}"));
        }
        self.refresh();
    }

    /// Open the pane now, and let the session catch up on its own.
    ///
    /// Waiting here for Claude Code to register itself would freeze the UI
    /// for several seconds on every spawn, which is how a key stops being
    /// worth pressing.
    fn launch(&mut self, name: &str, repo: &Path, naming: agent::Naming, brief: &str) {
        let Some(tmux) = self.tmux.clone() else {
            self.status = Some("no tmux — agents are started in tmux panes".into());
            return;
        };
        let command = brief::command(brief);
        let spawned = match agent::start(&tmux, &self.db, name, repo, &command, naming) {
            Ok(s) => s,
            Err(e) => {
                self.status = Some(format!("cannot start {name}: {e}"));
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

    /// Select the agent an event came from, and go back to watching it.
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
        // No outer margin: the panes bring their own gutters, and the rules
        // are meant to reach the edges of the terminal.
        let area = frame.area();
        self.draw_into(frame, area);
    }

    /// The same frame, into a given rectangle rather than the whole terminal.
    /// Only the preview wants this — it draws a chosen width inline, inside a
    /// window that is usually wider.
    pub fn draw_into(&mut self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Block::default().style(theme::base()), area);

        // A row above the title, so it is not jammed against the terminal's
        // first line.
        let [_, top, top_rule, body, key_rule, keys] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);

        self.draw_top(frame, top);
        theme::rule(frame, top_rule);
        theme::rule(frame, key_rule);

        // Folded away, the terminal is the window. That is the point of the
        // key: a REPL rendered into a third of the screen is a preview of a
        // terminal rather than one.
        let (rail, centre, side) = if self.wide {
            (Rect::ZERO, body, Rect::ZERO)
        } else {
            let [rail, centre, side] = Layout::horizontal([
                Constraint::Length(28),
                Constraint::Min(24),
                Constraint::Length(36),
            ])
            .areas(body);
            (rail, centre, side)
        };

        self.rail_at = rail;
        self.centre_at = centre;
        self.centre_size = (
            centre.width.saturating_sub(theme::GUTTER * 2 + 1),
            centre.height.saturating_sub(3),
        );
        if !self.wide {
            fleet::render(frame, rail, &self.rows, self.selected);
        }
        match self.centre_view {
            Some(view) => flow::render(
                frame,
                centre,
                view,
                &self.events,
                &self.rows,
                self.event_selected,
                !self.wide,
            ),
            None => {
                let row = self.rows.get(self.selected).cloned();
                let typing = self.focus == Focus::Session;
                self.centre.render(frame, centre, row.as_ref(), typing, !self.wide);
            }
        }
        if !self.wide {
            self.draw_side(frame, side);
        }
        self.draw_keys(frame, keys);

        // After everything, so that every rule and divider is on the buffer
        // to be joined up.
        if !self.wide {
            let columns = [rail.x + rail.width - 1, centre.x + centre.width - 1];
            // The flow views are ours to draw; a session's output is not.
            let theirs = match self.centre_view {
                Some(_) => Rect::ZERO,
                None => self.centre.content_at(),
            };
            theme::join(
                frame.buffer_mut(),
                &columns,
                top_rule.y,
                key_rule.y + 1,
                theirs,
            );
        }

        if let Some(picker) = &self.picker {
            picker.render(frame, area);
        }
    }

    fn draw_top(&self, frame: &mut Frame, area: Rect) {
        let count = |p: fleet::Presence| self.rows.iter().filter(|r| r.presence == p).count();
        let working = count(fleet::Presence::Working);
        let waiting = count(fleet::Presence::Waiting);
        let blocked = self
            .tasks
            .iter()
            .filter(|t| t.state == db::State::Blocked)
            .count();
        // Counts against the right edge, in the order the design has them.
        let mut right: Vec<Span> = Vec::new();
        let push = |right: &mut Vec<Span>, text: String, style: Style| {
            if !right.is_empty() {
                right.push(Span::styled("  ·  ", theme::faint()));
            }
            right.push(Span::styled(text, style));
        };
        if working > 0 {
            push(&mut right, format!("{working} working"), Style::default().fg(theme::BUSY));
        }
        if waiting > 0 {
            push(&mut right, format!("{waiting} waiting on you"), Style::default().fg(theme::OK));
        }
        if blocked > 0 {
            push(&mut right, format!("{blocked} blocked"), theme::accent());
        }
        let n = self.rows.len();
        push(
            &mut right,
            format!("{n} agent{}", if n == 1 { "" } else { "s" }),
            theme::dim(),
        );

        let inner = theme::pad(area);

        // The path last, because how much of it fits depends on the counts.
        // "fleet", two spaces, the counts, and the space spread keeps.
        let taken: usize = right.iter().map(|s| s.width()).sum::<usize>() + 5 + 2;
        let root = match self.root.as_deref() {
            Some(p) => shorten(p, (inner.width as usize).saturating_sub(taken)),
            None => "all repos".to_string(),
        };
        let left = vec![
            Span::styled("fleet", theme::accent().add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(root, theme::faint()),
        ];

        frame.render_widget(Paragraph::new(theme::spread(left, right, inner.width)), inner);
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
        // Before the status, and instead of the keys: with the mouse gone,
        // half of what the bar advertises does nothing, and a click that
        // quietly fails is worse than one the bar warned you about.
        if !self.mouse {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("the terminal has the mouse", theme::accent()),
                    Span::styled("   ^a m", theme::dim()),
                    Span::styled(" mouse back to fleet", theme::faint()),
                ])),
                theme::pad(area),
            );
            return;
        }
        if let Some(status) = &self.status {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(status.clone(), theme::accent()))),
                theme::pad(area),
            );
            return;
        }
        // What the prefix is for is worth saying, since nothing else on the
        // screen can: every other key is going to the agent.
        if self.armed {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" ^a ", theme::accent().add_modifier(Modifier::REVERSED)),
                    Span::styled(
                        "  n new  x retire  m mouse  z wide  g graph  l log  tab focus  q quit",
                        theme::dim(),
                    ),
                ])),
                theme::pad(area),
            );
            return;
        }

        let typing = self.focus == Focus::Session
            && self.centre_view.is_none()
            && self.centre.is_live();
        let keys: &[(&str, &str)] = match self.centre_view {
            _ if typing => &[
                ("keys", "→ agent"),
                ("^a", "fleet"),
                ("↵", "attach"),
                ("drag", "to copy"),
            ],
            Some(View::Log) => &[
                ("↑↓", "event"),
                ("↵", "jump to agent"),
                ("l", "back"),
                ("^a q", "quit"),
            ],
            Some(View::Graph) => &[("g", "back"), ("l", "log"), ("^a q", "quit")],
            None => &[
                ("↑↓", "agent"),
                ("n", "new agent"),
                ("z", "wide"),
                ("tab", "focus"),
                ("q", "quit"),
            ],
        };

        let mut spans: Vec<Span> = Vec::new();
        for (key, what) in keys.iter().copied() {
            spans.push(Span::styled(key, theme::dim()));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(what, theme::faint()));
            spans.push(Span::raw("   "));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), theme::pad(area));
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
    // Only here, never in snapshot: drawing a frame must not start a
    // Claude Code session as a side effect.
    app.prune_dead();
    app.ensure_chief();
    if let Some(at) = app.rows.iter().position(|r| r.role == fleet::Role::Chief) {
        app.selected = at;
        app.retarget();
    }

    let rx = app.rx.take().expect("the app owns its channel until run takes it");
    let tx = app.tx.clone();
    let paused = Arc::new(AtomicBool::new(false));
    spawn_input(tx.clone(), paused.clone());
    spawn_registry(tx.clone());
    spawn_transcript_poll(tx.clone());
    spawn_ticker(tx);

    let mut term = ratatui::init();
    // Clicking a pane and scrolling it is the point; without capture the
    // terminal keeps the mouse to itself.
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let result = (|| -> Result<()> {
        term.draw(|f| app.draw(f))?;
        while let Ok(msg) = rx.recv() {
            match msg {
                Msg::Key(key) => app.on_key(key),
                Msg::Mouse(ev) => app.on_mouse(ev),
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
            if app.wants_attach {
                app.wants_attach = false;
                attach(&mut term, &mut app, &paused)?;
            }
            term.draw(|f| app.draw(f))?;
        }
        Ok(())
    })();
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// A path as the design writes it: home as `~`, and the whole of the rest.
///
/// It says which workspace you are in, and a leaf name does not — three
/// checkouts on this machine are called `acme`. Too long for the bar, it
/// gives up leading directories rather than its tail: the end of a path is
/// the part that identifies it.
fn shorten(path: &Path, width: usize) -> String {
    let full = match std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf))
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    };
    if full.chars().count() <= width {
        return full;
    }

    // Whole directories, never half of one: "…/service/billing-service" is a
    // path, "…e/billing-service" is a string that happens to end like one.
    let parts: Vec<&str> = full.split('/').collect();
    for start in 1..parts.len() {
        let tail = parts[start..].join("/");
        if tail.chars().count() + 2 <= width {
            return format!("…/{tail}");
        }
    }
    // Not even the last component fits; the caller's line will cut it.
    parts.last().unwrap_or(&"").to_string()
}

fn inside(area: Rect, (x, y): (u16, u16)) -> bool {
    area.width > 0
        && x >= area.x
        && x < area.x + area.width
        && y >= area.y
        && y < area.y + area.height
}

/// Give the terminal to tmux, and take it back when the user detaches.
fn attach(
    term: &mut ratatui::DefaultTerminal,
    app: &mut App,
    paused: &Arc<AtomicBool>,
) -> Result<()> {
    let Some(tmux) = app.tmux.clone() else {
        return Ok(());
    };

    // Stop reading stdin before tmux starts: two readers on one terminal
    // means keystrokes land wherever they happen to be collected. The pause
    // outlasts the reader's poll window so nothing is in flight.
    paused.store(true, Ordering::SeqCst);
    std::thread::sleep(POLL * 2);

    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    let failed = app.centre.attach(&tmux);
    *term = ratatui::init();
    // Whatever the user left it as. Handing the mouse back and then taking
    // it again behind their back is the sort of thing that makes a program
    // feel haunted.
    if app.mouse {
        let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    }

    paused.store(false, Ordering::SeqCst);
    term.clear()?;
    app.status = failed;
    // The pane kept running while we were away, and its size may have
    // changed under tmux's attached client.
    app.refresh();
    Ok(())
}

/// How long the reader waits for a key before looking at the pause flag.
/// Short enough that handing the terminal over feels immediate.
const POLL: Duration = Duration::from_millis(60);

fn spawn_input(tx: Sender<Msg>, paused: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        loop {
            // Polled rather than blocking on read, so that stdin can be
            // given up while tmux has the terminal.
            if paused.load(Ordering::SeqCst) {
                std::thread::sleep(POLL);
                continue;
            }
            match event::poll(POLL) {
                Ok(false) => continue,
                Err(_) => return,
                Ok(true) => {}
            }
            match event::read() {
                Ok(Event::Key(key)) => {
                    if tx.send(Msg::Key(key)).is_err() {
                        return;
                    }
                }
                Ok(Event::Mouse(ev)) => {
                    if tx.send(Msg::Mouse(ev)).is_err() {
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
        assert!(out.contains("AGENTS"), "the rail: {out}");
        assert!(out.contains("new agent"), "the rail's footer: {out}");
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
        // No agent here has a live pane, so the arrows are fleet's.
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
    fn keys_act_directly_when_there_is_no_pane_to_type_into() {
        // Nothing is mirrored here, so q must still quit rather than being
        // swallowed as a keystroke for an agent that does not exist.
        let mut app = app();
        app.on_key(KeyEvent::from(KeyCode::Char('q')));
        assert!(app.quit);
    }

    #[test]
    fn the_prefix_takes_the_next_key_for_fleet_and_then_lets_go() {
        let mut app = app();
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(app.armed, "the key bar shows what the prefix can do");
        assert!(drawn(&mut app, 110, 24).contains("new"));

        app.on_key(KeyEvent::from(KeyCode::Char('g')));
        assert!(!app.armed, "and lets go after one key");
        assert_eq!(app.centre_view, Some(View::Graph));
    }

    #[test]
    fn retiring_takes_an_agent_off_the_rail_and_leaves_its_pane_alone() {
        let mut app = app();
        assert_eq!(app.rows.len(), 2);

        app.retire_selected();
        assert_eq!(app.rows.len(), 1, "the row goes");
        assert!(app.status.unwrap().contains("untouched"), "and says the pane did not");
    }

    #[test]
    fn pruning_clears_the_dead_but_keeps_one_that_still_holds_a_task() {
        let db = Db::open_in_memory().unwrap();
        // Both had a session and both are gone; only one was mid-task.
        db.upsert_agent("finished", None, Some("/repo"), Some("dead-1"), None, None)
            .unwrap();
        db.upsert_agent("died-working", None, Some("/repo"), Some("dead-2"), None, None)
            .unwrap();
        db.add_task(&crate::db::NewTask {
            key: "ENG-1-1",
            title: "half done",
            repo: "/repo",
            ..Default::default()
        })
        .unwrap();
        db.claim("ENG-1-1", "died-working").unwrap();
        db.transition("ENG-1-1", crate::db::State::Running, None).unwrap();

        let mut app = App::new(db, PathBuf::from(":memory:"), None);
        app.refresh();
        app.prune_dead();

        let names: Vec<_> = app.rows.iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, vec!["died-working"]);
    }

    #[test]
    fn the_header_carries_the_whole_path_not_just_the_leaf() {
        // Three checkouts on this machine are called acme; the leaf does
        // not say which workspace you are in.
        let db = Db::open_in_memory().unwrap();
        let mut app = App::new(
            db,
            PathBuf::from(":memory:"),
            Some(PathBuf::from("/w/Code/service/billing-service")),
        );
        app.refresh();
        let out = drawn(&mut app, 120, 12);
        assert!(out.contains("/w/Code/service/billing-service"), "{out}");
    }

    #[test]
    fn a_path_too_long_for_the_bar_gives_up_its_start() {
        // The end of a path is the part that identifies it, and a directory
        // is given up whole: "…e/billing-service" is not a path.
        let long = Path::new("/w/one/two/three/four/five/six/billing-service");
        assert_eq!(shorten(long, 80), "/w/one/two/three/four/five/six/billing-service");
        assert_eq!(shorten(long, 24), "…/six/billing-service");
        assert_eq!(shorten(long, 20), "…/billing-service");
    }

    #[test]
    fn home_is_written_as_a_tilde() {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let Some(home) = home else { return };
        assert_eq!(shorten(&home.join("Code/fleet"), 40), "~/Code/fleet");
        assert_eq!(shorten(&home, 40), "~");
    }

    #[test]
    fn the_mouse_can_be_handed_back_to_the_terminal() {
        // An application that reports mouse events stops the terminal
        // selecting text, and there is no way to have both.
        let mut app = app();
        assert!(app.mouse, "fleet takes it to begin with");

        app.on_key(KeyEvent::new(KeyCode::Char(PREFIX), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::from(KeyCode::Char('m')));
        assert!(!app.mouse);

        // The bar has to say so: with the mouse gone, half of what it
        // advertises does nothing, and a click that quietly fails is worse
        // than one it warned you about.
        let out = drawn(&mut app, 110, 24);
        assert!(out.contains("the terminal has the mouse"), "{out}");
        assert!(out.contains("^a m"), "and how to get it back: {out}");

        app.on_key(KeyEvent::new(KeyCode::Char(PREFIX), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::from(KeyCode::Char('m')));
        assert!(app.mouse);
        let out = drawn(&mut app, 110, 24);
        assert!(!out.contains("the terminal has the mouse"), "{out}");
    }

    #[test]
    fn a_click_selects_the_agent_it_landed_on() {
        let mut app = app();
        drawn(&mut app, 110, 24); // the rail has to have been drawn to be clicked
        assert_eq!(app.selected().unwrap().name, "chief");

        let rail = app.rail_at;
        let click = |app: &mut App, row: u16| {
            app.on_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rail.x + 2,
                row,
                modifiers: KeyModifiers::NONE,
            });
        };

        // The heading and its blank line, then four rows an agent: padding,
        // its name, its detail, padding. The second agent's name is on the
        // seventh line of the rail.
        click(&mut app, rail.y + 7);
        assert_eq!(app.selected().unwrap().name, "billing-svc");
        assert_eq!(app.focus, Focus::Rail, "clicking the rail points the keys at it");

        // The padding is part of the block the eye sees lit, so a click on
        // it selects that agent rather than the one below.
        click(&mut app, rail.y + 5);
        assert_eq!(app.selected().unwrap().name, "chief");
        click(&mut app, rail.y + 6);
        assert_eq!(app.selected().unwrap().name, "billing-svc");
    }

    #[test]
    fn clicking_the_centre_points_the_keyboard_at_the_agent() {
        let mut app = app();
        drawn(&mut app, 110, 24);
        app.focus = Focus::Rail;

        let centre = app.centre_at;
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: centre.x + 4,
            row: centre.y + 4,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.focus, Focus::Session);
    }

    #[test]
    fn enter_on_a_transcript_only_agent_does_not_pretend_to_zoom() {
        let mut app = app();
        app.on_key(KeyEvent::from(KeyCode::Enter));
        // Nothing to hand over, so nothing is claimed and nothing is queued.
        assert!(app.status.is_none());
        assert!(
            !app.wants_attach,
            "giving up the screen for an agent with no pane would strand the user"
        );
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
    fn tab_moves_the_keyboard_between_the_rail_and_the_session() {
        let mut app = app();
        app.focus = Focus::Rail;
        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Session);
        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Rail);
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
