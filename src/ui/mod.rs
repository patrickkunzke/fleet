//! The fleet view: the crew as a graph or a log, and the board beside it.
//!
//! herdr draws every agent's terminal, in a tab of its own, and owns
//! everything about it — typing, the mouse, the clipboard. This is the view
//! of the crew: who is doing what, the board, the graph. `↵`, or a click on
//! a card, goes to an agent's own tab rather than drawing it here, and no
//! key belongs to an agent, so none needs a prefix.
//!
//! Redraws are event-driven, not polled. Four sources feed one channel:
//! keystrokes, the session registry moving, herdr's agents changing, and a
//! slow tick that catches what produces no event of its own — a process
//! that died, and a clock that has to keep showing elapsed time.

pub mod board;
pub mod crew;
pub mod flow;
pub mod graph;
pub mod hosting;
pub mod picker;
pub mod preview;
pub mod resume;
pub mod theme;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph};

use crate::agent;
use crate::brief;
use crate::db::{self, Db};
use crate::host::{Host, What};
use crate::registry::{self, Registry, Watcher};
use crate::ui::crew::Row;
use crate::ui::flow::View;
use crate::ui::picker::Picker;

/// How often to redraw when nothing has happened. Slow on purpose: the only
/// things that change without an event are elapsed times and a process that
/// exited without touching its registry file.
const TICK: Duration = Duration::from_secs(2);

/// How often the graph is redrawn while a message travels across it.
const FRAME: Duration = Duration::from_millis(40);

enum Msg {
    Key(KeyEvent),
    /// A paste, whole. Only arrives because bracketed paste is on: without
    /// it the terminal types the text out, and every letter would be a key.
    Paste,
    Mouse(MouseEvent),
    Registry,
    /// herdr's agents, as they are now: after one of them changed.
    Agents(Vec<crate::herdr::Agent>),
    Tick,
    /// Time for the next frame of a message in flight. Far more frequent
    /// than the others, and draws only while one is.
    Frame,
}

pub struct App {
    registry: Registry,
    db: Db,
    root: Option<PathBuf>,
    /// herdr, which starts the agents and draws them. None only in a test
    /// or a preview, which must not reach a real one.
    host: Option<Host>,
    /// What herdr last said about its agents. Empty until the first answer.
    herdr_agents: Vec<crate::herdr::Agent>,
    /// Where sidebar labels and notifications are sent, off this thread.
    /// None until the loop starts.
    jobs: Option<Sender<hosting::Job>>,
    /// The labels last handed over, so an unchanged board sends nothing.
    labels: Vec<(String, String)>,
    /// Events already considered for a notification. None until the first
    /// read, which marks everything already on the board as seen: starting
    /// fleet must not replay yesterday's blockers as news.
    noticed: Option<std::collections::HashSet<String>>,
    /// The workers already announced as waiting for their go, with the task.
    /// None until the first read, for the same reason as `noticed`.
    go_noticed: Option<std::collections::HashSet<(String, String)>>,
    /// The agents already announced as nearly out of context. One that
    /// compacts and drops back under is forgotten, so the next climb is news.
    full_noticed: Option<std::collections::HashSet<String>>,
    rows: Vec<Row>,
    selected: usize,
    /// Where the graph was last drawn, so a click can be read against it.
    /// Empty while the log is up.
    graph_at: Rect,
    /// When fleet first saw each event, in seconds since the epoch. The graph
    /// shows a message travelling from the moment it appears here, which can
    /// be a beat after it was written: a pulse timed from the write alone
    /// would already be over by the time anyone could see it.
    first_seen: std::collections::HashMap<String, f64>,
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
    tasks: Vec<db::Task>,
    background: Vec<db::BgTask>,
    events: Vec<db::Event>,
    /// What the main column is showing.
    view: View,
    event_selected: usize,
    status: Option<String>,
    /// Whether the message on screen is one refresh put there, and so one
    /// refresh may take away.
    status_is_db_error: bool,
    quit: bool,
}

impl App {
    pub fn new(db: Db, db_path: PathBuf, root: Option<PathBuf>, host: Option<Host>) -> App {
        // The agents this view starts are named in herdr for its fleet.
        let host = host.map(|h| h.in_fleet(Some(&db_path)));
        let (tx, rx) = channel();
        App {
            db_path,
            tx,
            rx: Some(rx),
            registry: Registry::new(registry::default_dir()),
            db,
            root,
            host,
            herdr_agents: Vec::new(),
            jobs: None,
            labels: Vec::new(),
            noticed: None,
            go_noticed: None,
            full_noticed: None,
            rows: Vec::new(),
            selected: 0,
            graph_at: Rect::ZERO,
            first_seen: std::collections::HashMap::new(),
            picker: None,
            run: None,
            resume_picker: None,
            tasks: Vec::new(),
            background: Vec::new(),
            events: Vec::new(),
            view: View::Graph,
            event_selected: 0,
            status: None,
            status_is_db_error: false,
            quit: false,
        }
    }

    /// Re-read both sources and rebuild the crew.
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
                self.rows = crew::merge(&agents, &sessions);
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
        // With no run yet there is nothing to narrow it to.
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
        self.publish();
    }

    /// Record the conversation of every herdr agent the board has none for.
    ///
    /// A new agent registers its session only once its first dialog — the
    /// folder trust prompt, in a repository it has not seen — is answered,
    /// and that can be minutes after it was started. herdr knows the session
    /// as soon as there is one, so it is asked on every refresh until then.
    /// Without it the agent is on the graph but a later resume cannot find it.
    fn link_hosted(&mut self) {
        if self.herdr_agents.is_empty() {
            return;
        }
        let Ok(agents) = self.db.agents() else { return };
        for a in agents.iter().filter(|a| a.session_id.is_none()) {
            let Some(sid) = self.herdr_agent(a.target.as_deref()).and_then(|h| h.session_id()) else {
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
            .map(|r| self.herdr_agent(r.target.as_deref()).map(|h| h.agent_status.clone()))
            .collect();
        for (row, state) in self.rows.iter_mut().zip(states) {
            match state.as_deref() {
                Some("working") => {
                    row.presence = crew::Presence::Working;
                    row.asking = false;
                }
                Some("blocked") => {
                    row.presence = crew::Presence::Waiting;
                    row.asking = true;
                }
                Some("idle" | "done") => {
                    row.presence = crew::Presence::Waiting;
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
                let pane = self.herdr_agent(r.target.as_deref())?.pane_id.clone();
                let text = match r.role {
                    crew::Role::Chief => hosting::chief_label(&self.tasks),
                    crew::Role::Worker => hosting::worker_label(&r.name, &self.tasks, r.awaiting_go.as_deref()),
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

        // A worker that has stopped to wait for its go, once per task: when
        // it goes idle, since mid-turn it is still writing the plan, and not
        // again for answering a question and going idle once more.
        let awaiting: std::collections::HashSet<(String, String)> = self
            .rows
            .iter()
            .filter_map(|r| Some((r.name.clone(), r.awaiting_go.clone()?)))
            .collect();
        let idle: Vec<(String, String)> = self
            .rows
            .iter()
            .filter(|r| r.presence == crew::Presence::Waiting)
            .filter_map(|r| Some((r.name.clone(), r.awaiting_go.clone()?)))
            .collect();
        match self.go_noticed.as_mut() {
            None => self.go_noticed = Some(idle.into_iter().collect()),
            Some(seen) => {
                // Forget one that got its go or moved on, so a later wait
                // is news again.
                seen.retain(|k| awaiting.contains(k));
                for (agent, task) in idle {
                    if seen.insert((agent.clone(), task.clone())) {
                        let (title, body) = hosting::go_notice(&agent, &task);
                        let _ = jobs.send(hosting::Job::Notify { title, body });
                    }
                }
            }
        }

        // An agent nearly out of context compacts soon, and comes back with
        // a summary of its brief rather than the brief: worth handing the
        // rest of its task to a fresh one first.
        let full: Vec<(String, i64)> = self
            .rows
            .iter()
            .filter_map(|r| Some((r.name.clone(), r.context.filter(|c| *c >= graph::CONTEXT_HIGH)?)))
            .collect();
        match self.full_noticed.as_mut() {
            None => self.full_noticed = Some(full.into_iter().map(|(n, _)| n).collect()),
            Some(seen) => {
                seen.retain(|n| full.iter().any(|(f, _)| f == n));
                for (agent, percent) in full {
                    if seen.insert(agent.clone()) {
                        let (title, body) = hosting::full_notice(&agent, percent);
                        let _ = jobs.send(hosting::Job::Notify { title, body });
                    }
                }
            }
        }
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
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Char('q'), _) => self.quit = true,
            // Refresh used to live here; the board and the registry are
            // watched now, and bringing a run back is worth the key more.
            (KeyCode::Char('r'), _) => self.open_resume(),
            (KeyCode::Char('g'), _) => self.view = View::Graph,
            (KeyCode::Char('l'), _) => {
                self.view = if self.view == View::Log { View::Graph } else { View::Log }
            }
            (KeyCode::Char('n'), _) => self.open_picker(),
            (KeyCode::Char('x'), _) => self.retire_selected(),
            _ if self.view == View::Log => self.log_key(key),
            // Over the graph the cards are what the keys move between, and
            // ↵ goes to the one selected.
            (KeyCode::Right | KeyCode::Tab | KeyCode::Down | KeyCode::Char('j'), _) => self.move_by(1),
            (KeyCode::Left | KeyCode::BackTab | KeyCode::Up | KeyCode::Char('k'), _) => self.move_by(-1),
            (KeyCode::Home, _) => self.selected = 0,
            (KeyCode::End | KeyCode::Char('G'), _) => self.selected = self.rows.len().saturating_sub(1),
            (KeyCode::Enter, _) => self.go_to_selected(),
            _ => {}
        }
    }

    /// A click on a card goes to that agent, as selecting it and pressing
    /// `↵` would. Anything else is not a click on anything.
    fn on_mouse(&mut self, ev: MouseEvent) -> bool {
        if ev.kind != MouseEventKind::Down(MouseButton::Left) || self.picker.is_some() || self.resume_picker.is_some() {
            return false;
        }
        let ages = self.ages();
        let current = self.selected().map(|r| r.name.clone());
        let hit = graph::card_at(
            self.graph_at,
            &self.rows,
            &self.events,
            &ages,
            current.as_deref(),
            (ev.column, ev.row),
        )
        .map(String::from);
        let Some(at) = hit.and_then(|name| self.rows.iter().position(|r| r.name == name)) else {
            return false;
        };
        self.selected = at;
        self.go_to_selected();
        true
    }

    /// Nothing in the view takes text, and a paste typed out a key at a
    /// time would be a burst of commands.
    fn on_paste(&mut self) {
        self.status = Some("a paste goes to an agent — ↵ opens its tab".into());
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

    /// Go to the agent an event came from.
    fn jump_to_event(&mut self) {
        let Some(event) = self.events.get(self.event_selected) else {
            return;
        };
        let Some(who) = event.from_agent.clone().filter(|w| !w.is_empty()) else {
            return;
        };
        if let Some(at) = self.rows.iter().position(|r| r.name == who) {
            self.selected = at;
            self.go_to_selected();
        } else {
            self.status = Some(format!("{who} is not on the board any more"));
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

    /// Take the selected agent off the board.
    ///
    /// Its tab is left alone: the row is fleet's bookkeeping, and killing
    /// somebody's running session because they tidied a list would be a
    /// surprising thing for a list to do.
    fn retire_selected(&mut self) {
        let Some(row) = self.selected().cloned() else { return };
        if let Some(run) = self.run {
            let _ = self.db.retire_in_run(run, &row.name);
        }
        match self.db.retire_agent(&row.name) {
            Ok(()) => {
                self.status = Some(format!("{} is off the board; its tab is untouched", row.name));
                self.refresh();
            }
            Err(e) => self.status = Some(e.to_string()),
        }
    }

    /// Clear out agents that are finished with.
    ///
    /// Quitting fleet leaves every agent's row behind with its session gone,
    /// so without this the graph fills with the dead from previous runs. Only
    /// the ones holding no task: an agent that died mid-task is exactly what
    /// somebody needs to see.
    fn prune_dead(&mut self) {
        let dead: Vec<String> = self
            .rows
            .iter()
            .filter(|r| r.presence == crew::Presence::Gone && r.detail == "session ended")
            .map(|r| r.name.clone())
            .collect();
        for name in dead {
            // Off the graph, but not retired: its run keeps it, so it can be
            // resumed. Retiring it here, as this used to, is how a reboot
            // lost the whole crew.
            let _ = self.db.end_agent(&name);
        }
        self.refresh();
    }

    /// Make sure there is someone to brief.
    ///
    /// The chief is the first thing the workflow needs and the last thing
    /// anyone wants to set up by hand, so fleet starts one in the workspace
    /// if there is not a live one already. It is an ordinary Claude Code
    /// session; the board skill is what makes it a chief of staff.
    fn ensure_chief(&mut self) {
        let live = self.rows.iter().any(|r| {
            r.role == crew::Role::Chief
                && matches!(r.presence, crew::Presence::Working | crew::Presence::Waiting)
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

    /// Open the tab now, and let the session catch up on its own.
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
        let Some(host) = self.host.clone() else {
            self.status = Some("not in herdr — agents are started in herdr tabs".into());
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
        }

        self.adopt_later(host, spawned);
    }

    /// Hand the selected agent to herdr: its tab comes to the front, and the
    /// keyboard is its. This view stays where it is, a tab away.
    fn go_to_selected(&mut self) {
        let Some(row) = self.selected().cloned() else { return };
        let (Some(host), Some(target)) = (self.host.as_ref(), row.target.as_deref()) else {
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
            host.settle(&spawned.placed, Duration::from_secs(30));
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
        let Some(host) = self.host.clone() else {
            self.status = Some("not in herdr — agents are resumed into herdr tabs".into());
            return;
        };
        self.run = Some(run.id);
        let live: std::collections::HashSet<String> = self
            .rows
            .iter()
            .filter(|r| matches!(r.presence, crew::Presence::Working | crew::Presence::Waiting))
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
        if let Some(at) = self.rows.iter().position(|r| r.role == crew::Role::Chief) {
            self.selected = at;
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
        self.view == View::Graph
            && self
                .ages()
                .iter()
                .zip(&self.events)
                .any(|(&age, e)| e.kind == "message" && age < graph::PULSE_SECS + 0.2)
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

        // A side column of tasks needs a wide window. In a split pane it
        // leaves the graph a sliver and clips every title, so there the
        // board goes along the foot and has the whole width instead.
        let stacked = body.width < STACK_BELOW && body.height >= 20;
        let (centre, side, side_rule) = if stacked {
            let height = board::stacked_height(&self.tasks, &self.background)
                .min(body.height / 2)
                .max(6);
            let [centre, rule, side] =
                Layout::vertical([Constraint::Min(0), Constraint::Length(1), Constraint::Length(height)])
                    .areas(body);
            (centre, side, rule)
        } else {
            let [centre, side] = Layout::horizontal([Constraint::Min(24), Constraint::Length(36)]).areas(body);
            (centre, side, Rect::ZERO)
        };

        let ages = self.ages();
        let agent = self.rows.get(self.selected).map(|r| r.name.clone());
        self.graph_at = flow::render(
            frame,
            centre,
            self.view,
            &self.events,
            &ages,
            &self.rows,
            self.event_selected,
            agent.as_deref(),
            // Nothing to the centre's right to rule it off from, once the
            // board is below it.
            !stacked,
        );
        if stacked {
            theme::rule(frame, side_rule);
            board::render_stacked(
                frame,
                side,
                &self.tasks,
                &self.background,
                self.selected().map(|r| r.name.as_str()),
            );
        } else {
            board::render(
                frame,
                side,
                &self.tasks,
                &self.background,
                self.selected().map(|r| r.name.as_str()),
            );
        }
        self.draw_keys(frame, keys);

        // After everything, so that every rule and divider is on the buffer
        // to be joined up.
        let columns: Vec<u16> = [if stacked { Rect::ZERO } else { centre }]
            .iter()
            .filter(|r| !r.is_empty())
            .map(|r| r.x + r.width - 1)
            .collect();
        theme::join(frame.buffer_mut(), &columns, top_rule.y, key_rule.y + 1);

        if let Some(picker) = &self.picker {
            picker.render(frame, area);
        }
        if let Some(picker) = &self.resume_picker {
            picker.render(frame, area, unix_now());
        }
    }

    fn draw_top(&self, frame: &mut Frame, area: Rect) {
        let count = |p: crew::Presence| self.rows.iter().filter(|r| r.presence == p).count();
        let working = count(crew::Presence::Working);
        // Idle and waiting for a go is its own count: answering it is the
        // chief's or yours, and until then nothing in that repo changes.
        let needs_go = self
            .rows
            .iter()
            .filter(|r| r.presence == crew::Presence::Waiting && r.awaiting_go.is_some())
            .count();
        let waiting = count(crew::Presence::Waiting) - needs_go;
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
        if needs_go > 0 {
            push(&mut right, format!("{needs_go} need{} a go", if needs_go == 1 { "s" } else { "" }), theme::accent());
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
            None => "no workspace".to_string(),
        };
        let left = vec![
            Span::styled("fleet", theme::accent().add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(root, theme::faint()),
        ];

        frame.render_widget(Paragraph::new(theme::spread(left, right, inner.width)), inner);
    }

    fn draw_keys(&self, frame: &mut Frame, area: Rect) {
        if let Some(status) = &self.status {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(status.clone(), theme::accent()))),
                theme::pad(area),
            );
            return;
        }
        let keys: &[(&str, &str)] = match self.view {
            View::Log => &[
                ("↑↓", "event"),
                ("↵", "go to its agent"),
                ("l", "graph"),
                ("r", "resume"),
                ("q", "quit"),
            ],
            View::Graph => &[
                ("↑↓ ←→", "agent"),
                ("↵", "go to it"),
                ("n", "new agent"),
                ("l", "log"),
                ("r", "resume"),
                ("x", "retire"),
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

pub fn run(db: Db, db_path: PathBuf, root: Option<PathBuf>, host: Host) -> Result<()> {
    let mut app = App::new(db, db_path, root, Some(host));
    app.refresh();
    if let Some(root) = app.workspace() {
        // A board from before runs existed still holds the last crew that
        // ran; give it a run, once, so it can be brought back.
        let _ = app.db.adopt_legacy_run(&root);
    }
    let live = app
        .rows
        .iter()
        .any(|r| matches!(r.presence, crew::Presence::Working | crew::Presence::Waiting));
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
    spawn_input(tx.clone());
    spawn_registry(tx.clone());
    spawn_board_watch(tx.clone(), &app.db_path);
    spawn_frames(tx.clone());
    if let Some(host) = app.host.clone() {
        let back = tx.clone();
        hosting::follow(host.herdr.clone(), move |agents| back.send(Msg::Agents(agents)).is_ok());
        app.jobs = Some(hosting::worker(host.herdr));
    }
    spawn_ticker(tx);

    let mut term = ratatui::init();
    // Bracketed paste, so a paste arrives whole instead of as a burst of
    // keys, each of them a command here. And clicks, so a card can be
    // clicked.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste, Clicks);
    let result = (|| -> Result<()> {
        term.draw(|f| app.draw(f))?;
        while let Ok(first) = rx.recv() {
            // Everything already waiting, then one frame. One frame per
            // message meant a burst of events redrew the screen dozens of
            // times to show the last of them.
            let mut dirty = false;
            let mut refresh = false;
            let mut frame = false;
            for msg in std::iter::once(first).chain(rx.try_iter()) {
                match msg {
                    Msg::Key(key) => {
                        app.on_key(key);
                        dirty = true;
                    }
                    Msg::Paste => {
                        app.on_paste();
                        dirty = true;
                    }
                    Msg::Mouse(ev) => dirty |= app.on_mouse(ev),
                    // Several of these in one batch are one re-read.
                    Msg::Registry | Msg::Tick => refresh = true,
                    Msg::Agents(agents) => {
                        app.herdr_agents = agents;
                        refresh = true;
                    }
                    Msg::Frame => frame = true,
                }
                if app.quit {
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
            if frame && app.animating() {
                dirty = true;
            }
            if dirty {
                term.draw(|f| app.draw(f))?;
            }
        }
        Ok(())
    })();
    // A shell left in bracketed paste misreads the next thing typed into it.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableMouseCapture
    );
    ratatui::restore();
    crate::plugin::close_own_tab();
    result
}

/// A path as the design writes it: home as `~`, and the whole of the rest.
///
/// It says which workspace you are in, and a leaf name does not: two
/// checkouts of one project share it. Too long for the bar, it
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

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Below this many columns, the board goes under the graph rather than
/// beside it: what a graph needs, and the board's 36.
const STACK_BELOW: u16 = 80 + 36;

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

/// Presses and releases, in SGR coordinates — and not every movement of
/// the pointer, or a drag. crossterm's EnableMouseCapture also asks for
/// any-motion tracking, which reports the pointer each time it crosses a
/// cell: a message for nothing, dozens a second.
struct Clicks;

impl crossterm::Command for Clicks {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str(concat!(
            "\x1b[?1000h", // presses and releases
            "\x1b[?1006h", // SGR coordinates, with no 223-column limit
        ))
    }
}

fn spawn_input(tx: Sender<Msg>) {
    std::thread::spawn(move || {
        loop {
            let msg = match event::read() {
                Ok(Event::Key(key)) => Msg::Key(key),
                Ok(Event::Paste(_)) => Msg::Paste,
                // Only a press is worth a message: a release or the wheel
                // is not a click on anything.
                Ok(Event::Mouse(ev)) if ev.kind == MouseEventKind::Down(MouseButton::Left) => Msg::Mouse(ev),
                Ok(Event::Mouse(_)) => continue,
                // A resize still wants a redraw.
                Ok(_) => Msg::Tick,
                Err(_) => return,
            };
            if tx.send(msg).is_err() {
                return;
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


fn spawn_frames(tx: Sender<Msg>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(FRAME);
            if tx.send(Msg::Frame).is_err() {
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
        let mut app = App::new(db, PathBuf::from(":memory:"), Some(PathBuf::from("/nowhere")), None);
        app.refresh();
        app
    }

    fn drawn(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        format!("{}", term.backend())
    }

    #[test]
    fn the_frame_has_the_graph_the_board_and_a_key_bar() {
        let out = drawn(&mut app(), 130, 24);
        assert!(out.contains("fleet"), "{out}");
        assert!(out.contains("topology"), "the graph: {out}");
        assert!(out.contains("TASKS"), "the board: {out}");
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
        let host = Host {
            herdr: crate::herdr::Herdr::with(bin.to_string_lossy(), dir.join("sock")),
            workspace: "w1".into(),
            fleet: None,
        };
        let mut app = App::new(db, PathBuf::from(":memory:"), Some(PathBuf::from("/nowhere")), Some(host));
        app.refresh();
        app
    }

    #[test]
    fn l_flips_between_the_graph_and_the_log() {
        let mut app = app();
        assert_eq!(app.view, View::Graph, "herdr draws the agents; fleet draws the crew");
        app.on_key(KeyEvent::from(KeyCode::Char('l')));
        assert_eq!(app.view, View::Log);
        assert!(drawn(&mut app, 110, 24).contains("chronological"));
        app.on_key(KeyEvent::from(KeyCode::Char('l')));
        assert_eq!(app.view, View::Graph, "and back");
        app.on_key(KeyEvent::from(KeyCode::Char('l')));
        app.on_key(KeyEvent::from(KeyCode::Char('g')));
        assert_eq!(app.view, View::Graph, "g is always the graph");
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
        assert_eq!(app.view, View::Graph, "fleet stays as it was, a tab away");
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
        assert_eq!(row(&app, "billing-svc").presence, crew::Presence::Waiting);
        assert_eq!(row(&app, "chief").presence, crew::Presence::Working, "herdr's word over the registry's");
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
    fn a_worker_waiting_for_its_go_is_shown_and_announced_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = hosted_app(dir.path());
        let (tx, rx) = channel();
        app.jobs = Some(tx);
        app.db
            .add_task(&db::NewTask { key: "ENG-1-1", title: "consume the parameter", repo: "/repo/content", ..Default::default() })
            .unwrap();
        app.db.claim("ENG-1-1", "billing-svc").unwrap();
        app.db.upsert_agent("billing-svc", None, None, Some("sid-2"), None, None).unwrap();
        // Its mod checking in is what says the go is enforced here.
        app.db.check_in("sid-2", false, &db::Vitals::default()).unwrap();
        let notes = |rx: &Receiver<hosting::Job>| -> Vec<String> {
            rx.try_iter()
                .filter_map(|j| match j {
                    hosting::Job::Notify { title, .. } => Some(title),
                    _ => None,
                })
                .collect()
        };

        app.herdr_agents = vec![herdr_agent("chief", "w1:p1", "idle"), herdr_agent("billing-svc", "w1:p2", "working")];
        app.refresh();
        assert!(drawn(&mut app, 120, 30).contains("● planning"), "mid-turn: reading, not changing");
        assert!(notes(&rx).is_empty(), "still writing its plan");

        app.herdr_agents[1].agent_status = "idle".into();
        app.refresh();
        let shown = drawn(&mut app, 120, 30);
        assert!(shown.contains("◇ needs a go"), "{shown}");
        assert!(shown.contains("1 needs a go"), "and the header counts it: {shown}");
        assert_eq!(notes(&rx), ["fleet · billing-svc needs a go"]);

        // It answers a question and goes idle again: not news twice.
        app.herdr_agents[1].agent_status = "working".into();
        app.refresh();
        app.herdr_agents[1].agent_status = "idle".into();
        app.refresh();
        assert!(notes(&rx).is_empty());

        app.db.approve("ENG-1-1", "chief").unwrap();
        app.refresh();
        let shown = drawn(&mut app, 120, 30);
        assert!(!shown.contains("needs a go") && !shown.contains("planning"), "{shown}");
    }

    #[test]
    fn an_agent_nearly_out_of_context_is_announced_once_a_climb() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = hosted_app(dir.path());
        let (tx, rx) = channel();
        app.jobs = Some(tx);
        app.db.upsert_agent("billing-svc", None, None, Some("sid-2"), None, None).unwrap();
        let report = |app: &mut App, context: i64| {
            app.db.check_in("sid-2", false, &db::Vitals { tool: None, context: Some(context) }).unwrap();
            app.refresh();
        };
        let notes = |rx: &Receiver<hosting::Job>| -> Vec<String> {
            rx.try_iter()
                .filter_map(|j| match j {
                    hosting::Job::Notify { title, .. } => Some(title),
                    _ => None,
                })
                .collect()
        };

        report(&mut app, 60);
        assert!(notes(&rx).is_empty());
        report(&mut app, 82);
        assert_eq!(notes(&rx), ["fleet · billing-svc is at 82% context"]);
        report(&mut app, 88);
        assert!(notes(&rx).is_empty(), "once a climb");
        report(&mut app, 20);
        report(&mut app, 81);
        assert_eq!(notes(&rx), ["fleet · billing-svc is at 81% context"], "compacted, then full again");
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
        let mut app = App::new(db, PathBuf::from(":memory:"), None, None);
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
        let dir = tempfile::tempdir().unwrap();
        let mut app = hosted_app(dir.path());
        app.db
            .log_event("message", Some("billing-svc"), Some("chief"), None, "blocked", None, None)
            .unwrap();
        app.refresh();
        app.on_key(KeyEvent::from(KeyCode::Char('l')));
        app.on_key(KeyEvent::from(KeyCode::Enter));

        assert_eq!(app.selected().unwrap().name, "billing-svc");
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap_or_default();
        assert!(calls.contains("agent focus billing-svc"), "its tab comes forward: {calls}");
    }

    #[test]
    fn retiring_takes_an_agent_off_the_board_and_leaves_its_tab_alone() {
        let mut app = app();
        assert_eq!(app.rows.len(), 2);

        app.retire_selected();
        assert_eq!(app.rows.len(), 1, "the row goes");
        assert!(app.status.unwrap().contains("untouched"), "and says the tab did not");
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

        let mut app = App::new(db, PathBuf::from(":memory:"), None, None);
        app.refresh();
        app.prune_dead();

        let names: Vec<_> = app.rows.iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, vec!["died-working"]);
    }

    #[test]
    fn the_header_carries_the_whole_path_not_just_the_leaf() {
        // Two checkouts of one project share a leaf name; the leaf does not
        // say which workspace you are in.
        let db = Db::open_in_memory().unwrap();
        let mut app = App::new(
            db,
            PathBuf::from(":memory:"),
            Some(PathBuf::from("/w/Code/service/billing-service")),
            None,
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
    fn n_opens_the_picker_and_escape_closes_it_without_starting_anything() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("storefront/.git")).unwrap();
        std::fs::create_dir_all(root.path().join("billing-service/.git")).unwrap();

        let db = Db::open_in_memory().unwrap();
        let mut app = App::new(
            db,
            PathBuf::from(":memory:"),
            Some(root.path().to_path_buf()),
            None,
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
            None,
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
        let mut app = App::new(db, PathBuf::from(":memory:"), None, None);
        app.refresh();

        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(app.picker.is_none());
        assert!(app.status.unwrap().contains("--root"));
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

    #[test]
    fn clicking_a_workers_card_goes_to_its_tab() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = hosted_app(dir.path());
        let out = drawn(&mut app, 130, 30);
        assert_eq!(app.selected().unwrap().name, "chief", "the chief to begin with");

        let (y, line) = out.lines().enumerate().find(|(_, l)| l.contains(" billing-svc ")).expect("its card");
        let x = line[..line.find("billing-svc").unwrap()].chars().count() as u16;
        let click = |x, y| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(app.on_mouse(click(x, y as u16)));
        assert_eq!(app.selected().unwrap().name, "billing-svc");
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap_or_default();
        assert!(calls.contains("agent focus billing-svc"), "{calls}");

        assert!(!app.on_mouse(click(0, 0)), "a click on nothing does nothing");
        assert_eq!(app.selected().unwrap().name, "billing-svc");
    }

    #[test]
    fn the_mouse_is_asked_for_clicks_and_not_every_movement() {
        let mut ansi = String::new();
        crossterm::Command::write_ansi(&Clicks, &mut ansi).unwrap();
        assert!(ansi.contains("?1000h") && ansi.contains("?1006h"), "{ansi:?}");
        assert!(!ansi.contains("?1003h") && !ansi.contains("?1002h"), "{ansi:?}");
    }
}
