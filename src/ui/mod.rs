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
pub mod graph;
pub mod hosting;
pub mod keys;
pub mod picker;
pub mod mirror;
pub mod preview;
pub mod resume;
pub mod selection;
pub mod sender;
pub mod session;
pub mod theme;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph};

use crate::agent;
use crate::brief;
use crate::db::{self, Db};
use crate::host::{Host, Hosted, What};
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
    /// A paste, whole. Only arrives because bracketed paste is on: without
    /// it the terminal types the text out, a keystroke at a time.
    Paste(String),
    /// Word back from the thread that sends into panes.
    Sent(sender::Reply),
    Mouse(MouseEvent),
    Registry,
    /// herdr's agents, as they are now: after one of them changed.
    Agents(Vec<crate::herdr::Agent>),
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
pub(crate) const PREFIX: char = 'a';

pub struct App {
    registry: Registry,
    db: Db,
    root: Option<PathBuf>,
    projects: PathBuf,
    tmux: Option<Tmux>,
    /// herdr, when fleet runs in one of its panes. herdr then owns every
    /// agent's terminal — drawing, typing, the mouse, the clipboard — and
    /// fleet is the view of the crew: who is doing what, the board, the
    /// graph. ↵ goes to the agent's own tab instead of drawing it here.
    hosted: Option<Hosted>,
    /// What herdr last said about its agents. Empty outside herdr, and until
    /// the first answer.
    herdr_agents: Vec<crate::herdr::Agent>,
    /// Where sidebar labels and notifications are sent, off this thread.
    /// None until the loop starts, and outside herdr.
    jobs: Option<Sender<hosting::Job>>,
    /// The labels last handed over, so an unchanged board sends nothing.
    labels: Vec<(String, String)>,
    /// Events already considered for a notification. None until the first
    /// read, which marks everything already on the board as seen: starting
    /// fleet must not replay yesterday's blockers as news.
    noticed: Option<std::collections::HashSet<String>>,
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
    /// Whether the terminal speaks the kitty keyboard protocol, and so
    /// whether it was asked to. Remembered so it can be given back.
    enhanced: bool,
    /// When fleet first saw each event, in seconds since the epoch. The graph
    /// shows a message travelling from the moment it appears here, which can
    /// be a beat after it was written: a pulse timed from the write alone
    /// would already be over by the time anyone could see it.
    first_seen: std::collections::HashMap<String, f64>,
    /// Where keystrokes, pastes and the wheel go, off this thread. None
    /// until the loop starts: a frame drawn for a test or a preview sends
    /// nothing anywhere.
    sender: Option<sender::Sender>,
    /// True between the prefix and the key it modifies.
    armed: bool,
    /// The run new agents join: the crew in this workspace that a later
    /// resume brings back together.
    run: Option<i64>,
    /// The list of earlier runs to bring back, while it is open.
    resume_picker: Option<resume::ResumePicker>,
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
        // A test must not become a herdr view because the terminal it was
        // run from happens to be a herdr pane.
        let hosted = if cfg!(test) { None } else { Hosted::from_env() };
        App::within(db, db_path, root, hosted)
    }

    pub(crate) fn within(db: Db, db_path: PathBuf, root: Option<PathBuf>, hosted: Option<Hosted>) -> App {
        // The agents this view starts are named in herdr for its fleet.
        let hosted = hosted.map(|h| Hosted { fleet: crate::scope::fleet_of(&db_path), ..h });
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
            tmux: if hosted.is_some() { None } else { Tmux::detect(None).ok() },
            // With herdr drawing the agents, the middle column is the crew.
            centre_view: hosted.is_some().then_some(View::Graph),
            hosted,
            herdr_agents: Vec::new(),
            jobs: None,
            labels: Vec::new(),
            noticed: None,
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
            enhanced: false,
            sender: None,
            first_seen: std::collections::HashMap::new(),
            armed: false,
            picker: None,
            run: None,
            resume_picker: None,
            tasks: Vec::new(),
            background: Vec::new(),
            events: Vec::new(),
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
        self.link_hosted();
        self.registry.refresh();
        let sessions: Vec<_> = self.registry.sessions().cloned().collect();

        match self.db.agents() {
            Ok(agents) => {
                self.rows = fleet::merge(&agents, &sessions);
                self.apply_herdr();
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
        // Only this run's: the board has every run the workspace has had.
        // With no run yet, as in a view of all repos, there is nothing to
        // narrow it to.
        let events = match self.run {
            Some(run) => self.db.run_events(run, 200),
            None => self.db.events(200),
        };
        if let Ok(events) = events {
            self.events = events;
            let now = unix_now();
            let keys: std::collections::HashSet<String> = self.events.iter().map(event_key).collect();
            for key in &keys {
                self.first_seen.entry(key.clone()).or_insert(now);
            }
            // Forget what has fallen off the end of the log.
            self.first_seen.retain(|k, _| keys.contains(k));
        }
        self.event_selected = self
            .event_selected
            .min(self.events.len().saturating_sub(1));

        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.retarget();
        self.publish();
    }

    /// Record the conversation of every herdr agent the board has none for.
    ///
    /// A new agent registers its session only once its first dialog — the
    /// folder trust prompt, in a repository it has not seen — is answered,
    /// and that can be minutes after it was started. herdr knows the session
    /// as soon as there is one, so it is asked on every refresh until then.
    /// Without it the agent is on the rail but a later resume cannot find it.
    fn link_hosted(&mut self) {
        if self.herdr_agents.is_empty() {
            return;
        }
        let Ok(agents) = self.db.agents() else { return };
        for a in agents.iter().filter(|a| a.session_id.is_none()) {
            let Some(sid) = self.herdr_agent(a.tmux_target.as_deref()).and_then(|h| h.session_id()) else {
                continue;
            };
            let _ = agent::link(&self.db, &a.name, sid, self.run);
        }
    }

    /// The herdr agent a board target names, if herdr has it.
    fn herdr_agent(&self, target: Option<&str>) -> Option<&crate::herdr::Agent> {
        let name = target?.strip_prefix("herdr:")?;
        self.herdr_agents.iter().find(|h| h.name == name)
    }

    /// herdr's word on each agent's state, over the registry's.
    ///
    /// The registry says busy or not; herdr also says when an agent is
    /// stopped at a dialog, and says it the moment it happens. Where herdr
    /// has no opinion — an agent it has not recognised, or `unknown` — the
    /// registry's stands.
    fn apply_herdr(&mut self) {
        let states: Vec<Option<String>> = self
            .rows
            .iter()
            .map(|r| self.herdr_agent(r.tmux_target.as_deref()).map(|h| h.agent_status.clone()))
            .collect();
        for (row, state) in self.rows.iter_mut().zip(states) {
            match state.as_deref() {
                Some("working") => {
                    row.presence = fleet::Presence::Working;
                    row.asking = false;
                }
                Some("blocked") => {
                    row.presence = fleet::Presence::Waiting;
                    row.asking = true;
                }
                Some("idle" | "done") => {
                    row.presence = fleet::Presence::Waiting;
                    row.asking = false;
                }
                _ => {}
            }
        }
    }

    /// Hand herdr what changed: each agent's sidebar label, and a
    /// notification for each new board event that needs someone.
    fn publish(&mut self) {
        let Some(jobs) = self.jobs.clone() else { return };

        let labels: Vec<(String, String)> = self
            .rows
            .iter()
            .filter_map(|r| {
                let pane = self.herdr_agent(r.tmux_target.as_deref())?.pane_id.clone();
                let text = match r.role {
                    fleet::Role::Chief => hosting::chief_label(&self.tasks),
                    fleet::Role::Worker => hosting::worker_label(&r.name, &self.tasks),
                };
                Some((pane, text))
            })
            .collect();
        if labels != self.labels {
            self.labels = labels.clone();
            let _ = jobs.send(hosting::Job::Labels(labels));
        }

        let keys = self.events.iter().map(|e| (event_key(e), e));
        match self.noticed.as_mut() {
            None => self.noticed = Some(keys.map(|(k, _)| k).collect()),
            Some(seen) => {
                for (key, e) in keys {
                    if seen.insert(key)
                        && let Some((title, body)) = hosting::notice(e)
                    {
                        let _ = jobs.send(hosting::Job::Notify { title, body });
                    }
                }
            }
        }
    }

    /// Point the centre pane at the selection. Cheap when it has not changed.
    fn retarget(&mut self) {
        // Nothing to follow: herdr has the agent's terminal.
        if self.hosted.is_some() {
            return;
        }
        let row = self.selected().cloned();
        self.centre
            .follow(row.as_ref(), &self.projects, self.tmux.as_ref(), self.centre_size);
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if self.resume_picker.is_some() {
            self.resume_key(key);
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
            // Over the graph the cards are what the keys move between, and
            // ↵ opens the one selected — the design's "zoom into session".
            (KeyCode::Right | KeyCode::Tab, _) if self.centre_view == Some(View::Graph) => {
                self.move_by(1)
            }
            (KeyCode::Left | KeyCode::BackTab, _) if self.centre_view == Some(View::Graph) => {
                self.move_by(-1)
            }
            (KeyCode::Enter, _) if self.hosted.is_some() && self.centre_view == Some(View::Graph) => {
                self.go_to_selected()
            }
            (KeyCode::Enter, _) if self.centre_view == Some(View::Graph) => {
                self.centre_view = None;
                self.focus = Focus::Session;
            }
            (KeyCode::Tab, _) => {
                self.focus = match self.focus {
                    Focus::Rail => Focus::Session,
                    Focus::Session => Focus::Rail,
                }
            }
            // Refresh used to live here; the board and the registry are
            // watched now, and bringing a run back is worth the key more.
            (KeyCode::Char('r'), _) => self.open_resume(),
            (KeyCode::Char('z'), _) => {
                self.wide = !self.wide;
                self.focus = Focus::Session;
            }
            // In herdr there is no session to go back to: the two keys move
            // between the graph and the log.
            (KeyCode::Char('g'), _) if self.hosted.is_some() => self.centre_view = Some(View::Graph),
            (KeyCode::Char('l'), _) if self.hosted.is_some() => {
                self.centre_view = Some(if self.centre_view == Some(View::Log) { View::Graph } else { View::Log })
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
    /// A paste goes to the agent the keyboard is pointed at, in one piece.
    fn on_paste(&mut self, text: &str) {
        // The prefix was pressed and then a paste arrived instead of a
        // command: the paste wins, because nothing in fleet takes text.
        self.armed = false;
        let typing = self.focus == Focus::Session
            && self.centre_view.is_none()
            && self.centre.is_live();
        if !typing {
            self.status = Some(if self.hosted.is_some() {
                "a paste goes to an agent — ↵ opens its tab".into()
            } else {
                "a paste goes to an agent — click its pane first".into()
            });
            return;
        }
        let Some(tmux) = self.tmux.clone() else { return };
        self.centre.mirror_to_live();
        self.centre.clear_selection();
        if let (Some(sender), Some(pane)) = (&self.sender, self.centre.tmux_pane()) {
            sender.send(pane, sender::Job::Paste(text.to_string()));
            return;
        }
        if let Err(e) = self.centre.paste(&tmux, text) {
            self.status = Some(e.to_string());
        }
    }

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
        // Queued, not sent here: a send is a tmux process, and waiting on
        // one per keystroke is what made typing lag. The pane's echo arrives
        // through the mirror's own poll, not by sleeping for it.
        if let (Some(sender), Some(pane)) = (&self.sender, self.centre.tmux_pane()) {
            let job = match translated {
                keys::Key::Literal(text) => sender::Job::Text(text),
                keys::Key::Named(name) => sender::Job::Key(name),
            };
            sender.send(pane, job);
            return;
        }
        if let Err(e) = self.centre.send(&tmux, &translated) {
            self.status = Some(e.to_string());
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
    /// Returns whether anything changed, so a stray event costs no frame.
    fn on_mouse(&mut self, ev: MouseEvent) -> bool {
        let at = (ev.column, ev.row);
        if matches!(ev.kind, MouseEventKind::Moved) {
            return false;
        }
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
            MouseEventKind::ScrollUp if inside(self.centre_at, at) => self.wheel(1),
            MouseEventKind::ScrollDown if inside(self.centre_at, at) => self.wheel(-1),
            MouseEventKind::ScrollUp => self.rail_key(KeyEvent::from(KeyCode::Up)),
            MouseEventKind::ScrollDown => self.rail_key(KeyEvent::from(KeyCode::Down)),
            _ => {}
        }
        true
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
            if self.hosted.is_some() {
                self.go_to_selected();
                return;
            }
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
            "worker",
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
        if let Some(run) = self.run {
            let _ = self.db.retire_in_run(run, &row.name);
        }
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
            // Off the rail, but not retired: its run keeps it, so it can be
            // resumed. Retiring it here, as this used to, is how a reboot
            // lost the whole crew.
            let _ = self.db.end_agent(&name);
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
            crossterm::execute!(stdout(), CaptureMouse)
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
            "chief",
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
    fn launch(
        &mut self,
        name: &str,
        repo: &Path,
        naming: agent::Naming,
        role: &str,
        brief: &brief::Brief,
    ) {
        let Some(host) = self.host() else {
            self.status = Some("no tmux — agents are started in tmux panes".into());
            return;
        };
        let program = brief::claude_program();
        let what = What::Brief { brief, program: &program };
        let run = self.ensure_run();
        let spawned = match agent::start(&host, &self.db, name, repo, &what, naming, role, run) {
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

        self.adopt_later(host, spawned);
    }

    /// Where agents are started: herdr's tabs when fleet runs in herdr,
    /// tmux windows otherwise.
    fn host(&self) -> Option<Host> {
        match &self.hosted {
            Some(h) => Some(Host::Herdr(h.clone())),
            None => self.tmux.clone().map(Host::Tmux),
        }
    }

    /// Hand the selected agent to herdr: its tab comes to the front, and the
    /// keyboard is its. This view stays where it is, a tab away.
    fn go_to_selected(&mut self) {
        let Some(row) = self.selected().cloned() else { return };
        let (Some(host), Some(target)) = (self.host(), row.tmux_target.as_deref()) else {
            self.status = Some(format!("{} has no terminal fleet knows of", row.name));
            return;
        };
        if let Err(e) = host.focus(target) {
            self.status = Some(format!("cannot open {}: {e}", row.name));
        }
    }

    /// Wait, off this thread, for the session that appears in a new pane,
    /// and record it — on the board and in the run, which is what a resume
    /// reads later.
    fn adopt_later(&self, host: Host, spawned: agent::Spawned) {
        let db_path = self.db_path.clone();
        let tx = self.tx.clone();
        let run = self.run;
        std::thread::spawn(move || {
            host.settle(&spawned.placed, &spawned.name, Duration::from_secs(30));
            let Some(found) = agent::adopt(spawned.placed.pid, Duration::from_secs(30)) else {
                return;
            };
            // A separate connection: this thread cannot borrow the one the UI
            // is using, and SQLite in WAL mode is happy with both.
            if let Ok(db) = Db::open(&db_path) {
                let _ = agent::link(&db, &spawned.name, &found.session_id, run);
            }
            let _ = tx.send(Msg::Registry);
        });
    }

    /// The workspace, as runs record it: canonical, so that /tmp and
    /// /private/tmp are one workspace and not two.
    fn workspace(&self) -> Option<String> {
        let root = self.root.as_ref()?;
        let root = root.canonicalize().unwrap_or_else(|_| root.clone());
        Some(root.to_string_lossy().to_string())
    }

    /// The run new agents join, beginning one if there is none yet.
    fn ensure_run(&mut self) -> Option<i64> {
        if self.run.is_none() {
            let root = self.workspace()?;
            match self.db.start_run(&root) {
                Ok(id) => self.run = Some(id),
                Err(e) => self.status = Some(format!("cannot record the run: {e}")),
            }
        }
        self.run
    }

    /// Earlier runs in this workspace that have someone to bring back.
    fn offers(&self) -> Vec<resume::Offer> {
        let Some(root) = self.workspace() else {
            return Vec::new();
        };
        self.db
            .runs(&root)
            .unwrap_or_default()
            .into_iter()
            .filter_map(resume::Offer::of)
            .collect()
    }

    fn open_resume(&mut self) {
        let offers = self.offers();
        if offers.is_empty() {
            self.status = Some("no earlier run in this workspace to bring back".into());
            return;
        }
        self.resume_picker = Some(resume::ResumePicker::new(offers, self.run));
    }

    fn resume_key(&mut self, key: KeyEvent) {
        let Some(picker) = self.resume_picker.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.resume_picker = None,
            KeyCode::Up => picker.move_by(-1),
            KeyCode::Down => picker.move_by(1),
            KeyCode::Enter => {
                let Some(picker) = self.resume_picker.take() else { return };
                match picker.choice() {
                    resume::Choice::Run(i) => {
                        let offer = picker.offers.into_iter().nth(i).expect("chosen from the list");
                        self.resume_run(offer.run);
                    }
                    resume::Choice::Fresh => {
                        // A new run, not a continuation of the one before:
                        // what is started now belongs together, apart.
                        self.run = None;
                        self.ensure_chief();
                        self.select_chief();
                    }
                }
            }
            _ => {}
        }
    }

    /// Bring a run's crew back, each into its own conversation.
    fn resume_run(&mut self, run: db::Run) {
        let Some(host) = self.host() else {
            self.status = Some("no tmux — agents are resumed into tmux panes".into());
            return;
        };
        self.run = Some(run.id);
        let live: std::collections::HashSet<String> = self
            .rows
            .iter()
            .filter(|r| matches!(r.presence, fleet::Presence::Working | fleet::Presence::Waiting))
            .map(|r| r.name.clone())
            .collect();

        let mut back = Vec::new();
        let mut failed = Vec::new();
        for member in run.resumable() {
            // Still running from before: resuming it again would open a
            // second copy of the same conversation.
            if live.contains(&member.name) {
                continue;
            }
            match agent::resume(&host, &self.db, run.id, member) {
                Ok(spawned) => {
                    back.push(spawned.name.clone());
                    self.adopt_later(host.clone(), spawned);
                }
                Err(e) => failed.push(e.to_string()),
            }
        }
        let chief_back = run.resumable().any(|m| m.role == "chief");

        self.status = Some(match (back.is_empty(), failed.is_empty()) {
            (true, true) => "everyone in that run is already running".into(),
            (false, true) => format!("resumed {}", back.join(", ")),
            (_, false) => format!("resumed {} — {}", back.join(", "), failed.join("; ")),
        });
        self.refresh();
        // A run with no chief to resume still wants one to brief.
        if !chief_back {
            self.ensure_chief();
        }
        self.select_chief();
    }

    fn select_chief(&mut self) {
        if let Some(at) = self.rows.iter().position(|r| r.role == fleet::Role::Chief) {
            self.selected = at;
            self.retarget();
        }
    }

    /// Select the agent an event came from, and go back to watching it.
    /// Wait briefly for the pane to echo what was just sent.
    ///
    /// Without this a keystroke is invisible until the next poll, up to
    /// 400ms later, which reads as a dropped key. Bounded tightly: an agent
    /// that is busy will not echo at all, and the loop must not stall for it.
    /// A wheel notch over the centre pane.
    fn wheel(&mut self, notches: isize) {
        self.centre.clear_selection();
        match (&self.sender, self.centre.tmux_pane()) {
            // Where the notch goes depends on what the program in the pane
            // asked for, which only a tmux call can say — so it is decided
            // on the sender's thread, not this one.
            (Some(sender), Some(pane)) => sender.send(pane, sender::Job::Wheel(notches)),
            _ => {
                if !self.centre.scroll_mirror(notches * 3) {
                    self.centre.scroll_by(notches, 3);
                }
            }
        }
    }

    /// How old each event is, as the graph should see it.
    ///
    /// From its timestamp — unless fleet first saw it only moments after it
    /// was written, and then from that moment, so it is still seen in flight.
    /// An event already old when fleet started is old, not new: it must not
    /// set the whole graph pulsing on launch.
    fn ages(&self) -> Vec<f64> {
        let now = unix_now();
        self.events
            .iter()
            .map(|e| {
                let written = graph::epoch(&e.ts).unwrap_or(0.0);
                let seen = self.first_seen.get(&event_key(e)).copied().unwrap_or(written);
                let start = if seen - written < 10.0 { seen.max(written) } else { written };
                (now - start).max(0.0)
            })
            .collect()
    }

    /// Whether the graph has something moving on it, and so wants frames.
    fn animating(&self) -> bool {
        self.centre_view == Some(View::Graph)
            && self
                .ages()
                .iter()
                .zip(&self.events)
                .any(|(&age, e)| e.kind == "message" && age < graph::PULSE_SECS + 0.2)
    }

    /// What the sender reported back.
    fn on_sent(&mut self, reply: sender::Reply) {
        match reply {
            sender::Reply::Failed(e) => self.status = Some(e),
            sender::Reply::ScrollLocal(lines) => self.centre.scroll_local(lines),
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
        } else if self.hosted.is_some() {
            // In herdr the rail says nothing the graph does not: its cards
            // are the agents, selected with ←→ and opened with ↵, and herdr's
            // own sidebar lists the live ones with what they are on. Its
            // width goes to the graph.
            let [centre, side] = Layout::horizontal([Constraint::Min(24), Constraint::Length(36)]).areas(body);
            (Rect::ZERO, centre, side)
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
        if !rail.is_empty() {
            fleet::render(frame, rail, &self.rows, self.selected);
        }
        match self.centre_view {
            Some(view) => {
                let ages = self.ages();
                let agent = self.rows.get(self.selected).map(|r| r.name.clone());
                flow::render(
                    frame,
                    centre,
                    view,
                    &self.events,
                    &ages,
                    &self.rows,
                    self.event_selected,
                    agent.as_deref(),
                    !self.wide,
                )
            }
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
            let columns: Vec<u16> = [rail, centre]
                .iter()
                .filter(|r| !r.is_empty())
                .map(|r| r.x + r.width - 1)
                .collect();
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
        if let Some(picker) = &self.resume_picker {
            picker.render(frame, area, unix_now());
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
            if self.hosted.is_some() { "l" } else { "^a l" },
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
                        "  n new  x retire  r resume  m mouse  z wide  g graph  l log  tab focus  q quit",
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
            Some(View::Log) if self.hosted.is_some() => &[
                ("↑↓", "event"),
                ("↵", "go to its agent"),
                ("l", "graph"),
                ("r", "resume"),
                ("q", "quit"),
            ],
            _ if self.hosted.is_some() => &[
                ("↑↓ ←→", "agent"),
                ("↵", "go to it"),
                ("n", "new agent"),
                ("l", "log"),
                ("r", "resume"),
                ("x", "retire"),
                ("q", "quit"),
            ],
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
            Some(View::Graph) => &[
                ("←→", "agent"),
                ("↵", "open"),
                ("g", "back"),
                ("l", "log"),
                ("^a q", "quit"),
            ],
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
    if let Some(root) = app.workspace() {
        // A board from before runs existed still holds the last crew that
        // ran; give it a run, once, so it can be brought back.
        let _ = app.db.adopt_legacy_run(&root);
    }
    let live = app
        .rows
        .iter()
        .any(|r| matches!(r.presence, fleet::Presence::Working | fleet::Presence::Waiting));
    app.prune_dead();
    if live {
        // A fleet still running from before fleet was last closed: carry on
        // with it, and new agents join its run.
        app.run = app.workspace().and_then(|w| app.db.latest_run(&w).ok().flatten());
        app.ensure_chief();
        app.select_chief();
    } else {
        // Nothing running. If there is a crew to come back to, ask before
        // starting a new chief — that was the moment the old one was lost.
        let offers = app.offers();
        if offers.is_empty() {
            app.ensure_chief();
            app.select_chief();
        } else {
            app.resume_picker = Some(resume::ResumePicker::new(offers, None));
        }
    }

    let rx = app.rx.take().expect("the app owns its channel until run takes it");
    let tx = app.tx.clone();
    let paused = Arc::new(AtomicBool::new(false));
    spawn_input(tx.clone(), paused.clone());
    spawn_registry(tx.clone());
    spawn_board_watch(tx.clone(), &app.db_path);
    spawn_transcript_poll(tx.clone());
    if let Some(tmux) = app.tmux.clone() {
        let back = tx.clone();
        app.sender = Some(sender::Sender::start(tmux, move |reply| {
            let _ = back.send(Msg::Sent(reply));
        }));
    }
    if let Some(hosted) = app.hosted.clone() {
        let back = tx.clone();
        hosting::follow(hosted.herdr.clone(), move |agents| back.send(Msg::Agents(agents)).is_ok());
        app.jobs = Some(hosting::worker(hosted.herdr));
    }
    spawn_ticker(tx);

    let mut term = ratatui::init();
    // Asked once. The answer is the terminal's, and it does not change
    // between an attach and the return from one.
    app.enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    enter_modes(app.mouse, app.enhanced);
    let result = (|| -> Result<()> {
        term.draw(|f| app.draw(f))?;
        while let Ok(first) = rx.recv() {
            // Everything already waiting, then one frame. One frame per
            // message meant a wheel flick or a burst of output redrew the
            // screen dozens of times to show the last of them.
            let mut dirty = false;
            let mut refresh = false;
            let mut poll = false;
            for msg in std::iter::once(first).chain(rx.try_iter()) {
                match msg {
                    Msg::Key(key) => {
                        app.on_key(key);
                        dirty = true;
                    }
                    Msg::Paste(text) => {
                        app.on_paste(&text);
                        dirty = true;
                    }
                    Msg::Mouse(ev) => dirty |= app.on_mouse(ev),
                    Msg::Sent(reply) => {
                        app.on_sent(reply);
                        dirty = true;
                    }
                    // Several of these in one batch are one re-read.
                    Msg::Registry | Msg::Tick => refresh = true,
                    Msg::Agents(agents) => {
                        app.herdr_agents = agents;
                        refresh = true;
                    }
                    Msg::Transcript => poll = true,
                }
                if app.quit || app.wants_attach {
                    break;
                }
            }
            if app.quit {
                break;
            }
            if refresh {
                app.refresh();
                dirty = true;
            }
            // Nothing new is the common case, and then there is no frame —
            // unless a message is travelling across the graph, which needs
            // one every tick for as long as it is.
            if poll && (app.centre.poll() | app.animating()) {
                dirty = true;
            }
            if app.wants_attach {
                app.wants_attach = false;
                attach(&mut term, &mut app, &paused)?;
                dirty = true;
            }
            if dirty {
                term.draw(|f| app.draw(f))?;
            }
        }
        Ok(())
    })();
    leave_modes(app.enhanced);
    ratatui::restore();
    if app.hosted.is_some() {
        crate::plugin::close_own_tab();
    }
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

/// Show what the terminal sends for each key, and what fleet would pass on.
///
/// "The keys don't work" is unanswerable without this: a terminal decides
/// what Option+Left or Cmd+Left produces, the answer differs between Warp,
/// iTerm and Ghostty, and it differs again with each one's settings. Run it
/// in the terminal fleet is used in and the question answers itself.
pub fn keys() -> Result<()> {
    use std::io::Write;
    crossterm::terminal::enable_raw_mode()?;
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    enter_modes(false, enhanced);

    let mut out = std::io::stdout();
    let say = |out: &mut std::io::Stdout, line: &str| {
        let _ = write!(out, "{line}\r\n");
        let _ = out.flush();
    };
    say(&mut out, "fleet keys — press keys to see what arrives and what fleet sends on.");
    say(&mut out, "Ctrl-C twice to stop.\r\n");
    say(
        &mut out,
        &format!(
            "kitty keyboard protocol: {}    bracketed paste: on\r\n",
            if enhanced { "on — Shift+Enter can be told from Enter" } else { "not supported — Shift+Enter is the same byte as Enter" }
        ),
    );

    let mut last_ctrl_c = false;
    loop {
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let sent = match keys::translate(key) {
                    Some(keys::Key::Literal(t)) => format!("types {t:?}"),
                    Some(keys::Key::Named(n)) => format!("sends {n}"),
                    None => "sends nothing".into(),
                };
                // Spelled the way a keyboard labels them, not the way the
                // bitflags debug-print.
                let mut mods = String::new();
                for (flag, label) in [
                    (KeyModifiers::CONTROL, "Ctrl"),
                    (KeyModifiers::ALT, "Alt"),
                    (KeyModifiers::SHIFT, "Shift"),
                    (KeyModifiers::SUPER, "Cmd"),
                ] {
                    if key.modifiers.contains(flag) {
                        mods.push_str(label);
                        mods.push('+');
                    }
                }
                let note = if key.code == KeyCode::Char(PREFIX)
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    "   ← fleet's prefix: an agent never sees this key"
                } else {
                    ""
                };
                say(&mut out, &format!("{:<24} {sent}{note}", format!("{mods}{:?}", key.code)));

                let ctrl_c = key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL);
                if ctrl_c && last_ctrl_c {
                    break;
                }
                last_ctrl_c = ctrl_c;
            }
            Event::Paste(text) => {
                let lines = text.lines().count().max(1);
                say(
                    &mut out,
                    &format!(
                        "{:<24} pasted whole, bracketed — its newlines will not send",
                        format!("paste ({} chars, {lines} lines)", text.chars().count())
                    ),
                );
            }
            _ => {}
        }
    }

    leave_modes(enhanced);
    crossterm::terminal::disable_raw_mode()?;
    Ok(())
}

/// Clicks, drags and the wheel — and not every movement of the pointer.
///
/// crossterm's EnableMouseCapture also turns on any-motion tracking, mode
/// 1003, which reports the pointer every time it crosses a cell. Fleet uses
/// none of those reports, and each one was a message and a redrawn frame.
/// 1002 still reports movement while a button is held, which is what a drag
/// to select needs.
struct CaptureMouse;

impl crossterm::Command for CaptureMouse {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str(concat!(
            "\x1b[?1000h", // presses and releases, and the wheel
            "\x1b[?1002h", // movement, but only with a button down
            "\x1b[?1006h", // SGR coordinates, with no 223-column limit
        ))
    }
}

/// Ask the terminal fleet runs in for what it needs, all in one place.
///
/// Three modes, each for a reason the pane would otherwise feel wrong for.
/// The mouse, so a pane can be clicked and scrolled. Bracketed paste, so a
/// paste arrives whole instead of typed out a key at a time with every
/// newline an Enter. And, where the terminal has it, the kitty keyboard
/// protocol's disambiguation, which is the only way to tell Shift+Enter from
/// Enter at all — without it the two are the same byte.
fn enter_modes(mouse: bool, enhanced: bool) {
    use crossterm::event::{EnableBracketedPaste, KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
    let mut out = std::io::stdout();
    if mouse {
        let _ = crossterm::execute!(out, CaptureMouse);
    }
    let _ = crossterm::execute!(out, EnableBracketedPaste);
    if enhanced {
        let _ = crossterm::execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
}

/// Everything `enter_modes` asked for, given back. Before tmux takes the
/// terminal on attach, and before fleet exits: a shell left in bracketed
/// paste or the kitty protocol misreads the next thing typed into it.
fn leave_modes(enhanced: bool) {
    use crossterm::event::{DisableBracketedPaste, PopKeyboardEnhancementFlags};
    let mut out = std::io::stdout();
    if enhanced {
        let _ = crossterm::execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = crossterm::execute!(out, DisableBracketedPaste);
    let _ = crossterm::execute!(out, DisableMouseCapture);
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// An event's identity, for remembering when it was first seen. The log has
/// no id column to read, and these four together do not repeat.
fn event_key(e: &db::Event) -> String {
    format!(
        "{}|{}|{}|{}",
        e.ts,
        e.from_agent.as_deref().unwrap_or(""),
        e.to_agent.as_deref().unwrap_or(""),
        e.summary
    )
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

    leave_modes(app.enhanced);
    ratatui::restore();
    let failed = app.centre.attach(&tmux);
    *term = ratatui::init();
    // The mouse as the user left it. Handing it back and then taking it
    // again behind their back is the sort of thing that makes a program feel
    // haunted.
    enter_modes(app.mouse, app.enhanced);

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
                Ok(Event::Paste(text)) => {
                    if tx.send(Msg::Paste(text)).is_err() {
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

/// Refresh the moment another process writes to the board.
///
/// The two-second tick was the only way a new task or message reached the
/// screen, which is slow for a graph meant to show messages as they travel.
/// Only the database and its write-ahead log wake it: SQLite readers write
/// to `-shm`, and a watcher woken by that would refresh, read, and wake
/// itself again for ever.
fn spawn_board_watch(tx: Sender<Msg>, db: &Path) {
    let Some(dir) = db.parent().map(Path::to_path_buf) else {
        return;
    };
    let name = db.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    std::thread::spawn(move || {
        let wal = format!("{name}-wal");
        let watched = name.clone();
        let Ok(watcher) = Watcher::only(&dir, move |p| {
            p.file_name()
                .map(|f| f.to_string_lossy())
                .is_some_and(|f| f == watched || f == wal)
        }) else {
            return;
        };
        loop {
            if watcher.wait(Duration::from_secs(30)) && tx.send(Msg::Registry).is_err() {
                return;
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
/// How often the mirrored pane is checked for new output.
///
/// This is the frame rate of the centre pane, and it has to be a terminal's
/// rather than a dashboard's: at 400ms an agent's output arrived in visible
/// chunks and the pane felt slower than the terminal it is a picture of.
/// The check itself is one `stat`, and the loop skips the redraw when the
/// file has not grown, so the cost of the shorter interval is a syscall
/// twenty-five times a second.
const MIRROR_POLL: Duration = Duration::from_millis(40);

fn spawn_transcript_poll(tx: Sender<Msg>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(MIRROR_POLL);
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

    /// The view as it runs in a herdr pane, with a herdr that records what
    /// it was asked to do in `calls` beside it.
    fn hosted_app(dir: &Path) -> App {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("herdr");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\necho '{{\"result\":{{}}}}'\n",
                dir.join("calls").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let db = Db::open_in_memory().unwrap();
        db.upsert_agent("chief", Some("chief"), None, None, Some("herdr:chief"), None).unwrap();
        db.upsert_agent("billing-svc", None, Some("/repo/content"), None, Some("herdr:billing-svc"), None)
            .unwrap();
        let hosted = Hosted {
            herdr: crate::herdr::Herdr::with(bin.to_string_lossy(), dir.join("sock")),
            workspace: "w1".into(),
            fleet: None,
        };
        let mut app = App::within(db, PathBuf::from(":memory:"), Some(PathBuf::from("/nowhere")), Some(hosted));
        app.refresh();
        app
    }

    #[test]
    fn in_herdr_the_centre_is_the_crew_and_never_a_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = hosted_app(dir.path());
        assert_eq!(app.centre_view, Some(View::Graph), "herdr draws the agents; fleet draws the crew");
        app.on_key(KeyEvent::from(KeyCode::Char('l')));
        assert_eq!(app.centre_view, Some(View::Log));
        app.on_key(KeyEvent::from(KeyCode::Char('l')));
        assert_eq!(app.centre_view, Some(View::Graph), "and back, not to an empty session view");
        app.on_key(KeyEvent::from(KeyCode::Char('g')));
        assert_eq!(app.centre_view, Some(View::Graph));
        let out = drawn(&mut app, 120, 30);
        assert!(!out.contains("^a"), "no prefix where no key is an agent's:\n{out}");
    }

    #[test]
    fn in_herdr_enter_takes_you_to_the_agents_own_tab() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = hosted_app(dir.path());
        let at = app.rows.iter().position(|r| r.name == "billing-svc").unwrap();
        app.selected = at;
        app.on_key(KeyEvent::from(KeyCode::Enter));
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap_or_default();
        assert!(calls.contains("agent focus billing-svc"), "{calls}");
        assert_eq!(app.centre_view, Some(View::Graph), "fleet stays as it was, a tab away");
    }

    fn herdr_agent(name: &str, pane: &str, status: &str) -> crate::herdr::Agent {
        crate::herdr::Agent {
            pane_id: pane.into(),
            name: name.into(),
            agent: "claude".into(),
            agent_status: status.into(),
            ..Default::default()
        }
    }

    #[test]
    fn herdr_saying_an_agent_is_at_a_dialog_puts_it_first_in_line_for_you() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = hosted_app(dir.path());
        app.herdr_agents = vec![
            herdr_agent("chief", "w1:p1", "working"),
            herdr_agent("billing-svc", "w1:p2", "blocked"),
        ];
        app.refresh();
        let row = |app: &App, n: &str| app.rows.iter().find(|r| r.name == n).cloned().unwrap();
        assert!(row(&app, "billing-svc").asking);
        assert_eq!(row(&app, "billing-svc").presence, fleet::Presence::Waiting);
        assert_eq!(row(&app, "chief").presence, fleet::Presence::Working, "herdr's word over the registry's");
        assert!(drawn(&mut app, 120, 30).contains("! needs you"), "and the graph says so");

        app.herdr_agents[1].agent_status = "working".into();
        app.refresh();
        assert!(!row(&app, "billing-svc").asking, "answered, and moving again");
    }

    #[test]
    fn a_session_herdr_knows_of_is_put_on_the_board() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = hosted_app(dir.path());
        let mut a = herdr_agent("billing-svc", "w1:p2", "idle");
        a.agent_session = Some(crate::herdr::AgentSession { value: "sid-9".into() });
        app.herdr_agents = vec![a];
        app.refresh();
        let on_board = app.db.agents().unwrap().into_iter().find(|a| a.name == "billing-svc").unwrap();
        assert_eq!(on_board.session_id.as_deref(), Some("sid-9"));
    }

    #[test]
    fn the_sidebar_gets_each_agents_task_and_only_when_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = hosted_app(dir.path());
        let (tx, rx) = channel();
        app.jobs = Some(tx);
        app.db.upsert_epic("ENG-2553", "shared flag").unwrap();
        app.db
            .add_task(&db::NewTask {
                key: "ENG-2553-2",
                title: "consume the parameter",
                repo: "/repo/content",
                epic: Some("ENG-2553"),
                body: None,
                deps: &[],
                position: 0,
            })
            .unwrap();
        app.db.claim("ENG-2553-2", "billing-svc").unwrap();
        app.herdr_agents = vec![herdr_agent("chief", "w1:p1", "idle"), herdr_agent("billing-svc", "w1:p2", "working")];
        app.refresh();

        let labels: Vec<(String, String)> = rx
            .try_iter()
            .filter_map(|j| match j {
                hosting::Job::Labels(l) => Some(l),
                _ => None,
            })
            .last()
            .expect("labels sent");
        assert!(labels.contains(&("w1:p1".into(), "1 open".into())), "{labels:?}");
        assert!(labels.iter().any(|(p, t)| p == "w1:p2" && t.starts_with("ENG-2553-2")), "{labels:?}");

        app.refresh();
        assert!(rx.try_iter().all(|j| !matches!(j, hosting::Job::Labels(_))), "nothing changed, nothing sent");
    }

    #[test]
    fn a_new_blocker_is_announced_and_an_old_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = hosted_app(dir.path());
        let (tx, rx) = channel();
        app.db
            .log_event("task", Some("billing-svc"), None, Some("ENG-1-1"), "blocked: from before", None, None)
            .unwrap();
        app.jobs = Some(tx);
        app.refresh();
        let notes = |rx: &Receiver<hosting::Job>| -> Vec<String> {
            rx.try_iter()
                .filter_map(|j| match j {
                    hosting::Job::Notify { title, .. } => Some(title),
                    _ => None,
                })
                .collect()
        };
        assert!(notes(&rx).is_empty(), "what was already on the board is not news");

        app.db
            .log_event("task", Some("billing-svc"), None, Some("ENG-1-2"), "blocked: needs the flag", None, None)
            .unwrap();
        app.refresh();
        assert_eq!(notes(&rx), ["fleet · ENG-1-2 is blocked"]);
        app.refresh();
        assert!(notes(&rx).is_empty(), "and once");
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
    fn the_mouse_is_asked_for_clicks_and_drags_but_not_every_movement() {
        // Any-motion tracking reported the pointer every time it crossed a
        // cell, and each report was a message and a frame. Fleet uses none.
        let mut ansi = String::new();
        crossterm::Command::write_ansi(&CaptureMouse, &mut ansi).unwrap();
        assert!(ansi.contains("?1000h"), "presses and the wheel");
        assert!(ansi.contains("?1002h"), "drags, for selecting");
        assert!(ansi.contains("?1006h"), "coordinates past column 223");
        assert!(!ansi.contains("?1003h"), "every pointer movement: {ansi:?}");
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
