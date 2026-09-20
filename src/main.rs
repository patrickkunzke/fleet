//! Until the TUI lands, this binary is how the pieces get exercised against
//! real data: live sessions, one session's transcript, and the board.
//!
//! Board *writes* still belong to cli/board.sh. The write path exists in
//! [`db`] and is tested, but moving half the subcommands over would leave a
//! CLI where some verbs write and others do not — worse than either end state.
//! It migrates in one step when the TUI needs it.

mod db;
mod registry;
mod tmux;
mod transcript;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::db::Db;
use crate::registry::{Change, Registry, Session, Watcher};
use crate::tmux::Tmux;
use crate::transcript::{BACKFILL_BYTES, Entry, Outcome, Transcript};

#[derive(Parser)]
#[command(name = "fleet", version, about = "Orchestrate Claude Code sessions across repos")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Live sessions and what they are doing.
    Sessions {
        /// Follow the registry and report each change.
        #[arg(short, long)]
        watch: bool,
    },
    /// Render one session's transcript the way the centre pane will.
    Session {
        /// A name as `fleet sessions` prints it.
        name: String,
        #[arg(short, long)]
        watch: bool,
        /// How many entries to show.
        #[arg(short, long, default_value_t = 40)]
        lines: usize,
    },
    /// Start an agent in a tmux pane and register it on the board.
    Spawn {
        /// What to call it. Also the tmux window name.
        name: String,
        /// The repository the agent works in.
        #[arg(long)]
        repo: PathBuf,
        /// What to run. Overridable so the plumbing can be exercised without
        /// starting a real agent.
        #[arg(long, default_value = "claude")]
        command: String,
        /// tmux session to spawn into. Defaults to the current one, else "fleet".
        #[arg(long)]
        session: Option<String>,
        /// Seconds to wait for the agent to register itself.
        #[arg(long, default_value_t = 20)]
        timeout: u64,
        #[arg(long, env = "FLEET_DB")]
        db: Option<PathBuf>,
    },
    /// Read the coordination database.
    Board {
        #[command(subcommand)]
        view: BoardView,
        /// Override the database path.
        #[arg(long, env = "FLEET_DB")]
        db: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum BoardView {
    /// Every task, with what it is waiting for.
    Ls,
    /// Queued tasks whose dependencies are all done.
    Ready,
    /// Registered agents and the task each one holds.
    Agents,
    /// Background processes, running or recently finished.
    Bg,
    /// The flow log, newest first.
    Log {
        #[arg(default_value_t = 20)]
        limit: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Sessions { watch: false }) {
        Command::Sessions { watch } => sessions(watch),
        Command::Session { name, watch, lines } => session(&name, watch, lines),
        Command::Board { view, db } => board(view, db),
        Command::Spawn {
            name,
            repo,
            command,
            session,
            timeout,
            db,
        } => spawn(&name, &repo, &command, session.as_deref(), timeout, db),
    }
}

/// Start an agent and join the three sources back together: a tmux pane, the
/// Claude Code session that appears inside it, and the row on the board.
fn spawn(
    name: &str,
    repo: &Path,
    command: &str,
    session: Option<&str>,
    timeout: u64,
    db_path: Option<PathBuf>,
) -> Result<()> {
    let repo = repo
        .canonicalize()
        .with_context(|| format!("no such repository: {}", repo.display()))?;
    let tmux = Tmux::detect(session)?;
    let pane = tmux.spawn(name, &repo, command)?;
    println!("{name}  pane {} in session {}", pane.id, pane.session);

    let db = Db::open(db_path.unwrap_or_else(db::default_path))?;
    let target = format!("{}:{}", pane.session, pane.window_name);

    // The agent registers itself a moment after the process starts, so the row
    // goes in without a session id first. An agent that never reports is still
    // an agent someone has to deal with, and a board that omits it is worse
    // than one that shows it unlinked.
    db.upsert_agent(name, None, Some(&repo.to_string_lossy()), None, Some(&target), None)?;

    match adopt(&pane.pid, timeout) {
        Some(found) => {
            db.upsert_agent(name, None, None, Some(&found.session_id), None, None)?;
            db.log_event(
                "note",
                Some(name),
                None,
                None,
                &format!("spawned in {target}"),
                None,
                None,
            )?;
            println!("      adopted session {} (pid {})", found.session_id, found.pid);
        }
        None => println!(
            "      no session registered within {timeout}s — the pane is up, \
             the board row is unlinked"
        ),
    }
    Ok(())
}

/// Wait for a Claude Code session to appear inside a pane we just created.
///
/// Matched by process tree rather than by working directory: two agents in
/// the same repo are ordinary here, and a cwd match would adopt the wrong one.
fn adopt(pane_pid: &i32, timeout_secs: u64) -> Option<Session> {
    let mut reg = Registry::new(registry::default_dir());
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    while std::time::Instant::now() < deadline {
        reg.refresh();
        if let Some(s) = reg
            .interactive()
            .find(|s| tmux::owns(*pane_pid, s.pid))
            .cloned()
        {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}

fn sessions(watch: bool) -> Result<()> {
    let dir = registry::default_dir();
    let projects = registry::default_projects_dir();
    let mut reg = Registry::new(&dir);
    reg.refresh();

    let mut live: Vec<_> = reg.interactive().collect();
    live.sort_by_key(|s| s.started_at);

    println!("{} live in {}", live.len(), dir.display());
    for s in live {
        let size = match s.transcript_path(&projects) {
            Some(p) => std::fs::metadata(&p)
                .map(|m| format!("{} KB", m.len() / 1024))
                .unwrap_or_default(),
            None => "no transcript".into(),
        };
        println!(
            "  {:<6} {:<28} {:<5} {:<24} {}",
            s.pid,
            s.name,
            s.status.as_str(),
            s.repo(),
            size
        );
    }

    if !watch {
        return Ok(());
    }
    println!("\nwatching — ctrl-c to stop");
    let watcher = Watcher::new(&dir)?;
    loop {
        // The timeout is the sweep: a crashed session produces no filesystem
        // event, so waiting on one alone would never notice it had gone.
        watcher.wait(Duration::from_secs(5));
        for change in reg.refresh() {
            match change {
                Change::Upserted(s) => {
                    println!("  + {:<28} {:<5} {}", s.name, s.status.as_str(), s.repo())
                }
                Change::Removed { name, pid } => println!("  - {name} ({pid})"),
            }
        }
    }
}

fn session(name: &str, watch: bool, lines: usize) -> Result<()> {
    let mut reg = Registry::new(registry::default_dir());
    reg.refresh();

    let Some(s) = reg.by_name(name) else {
        bail!("no live session named '{name}'");
    };
    let Some(path) = s.transcript_path(&registry::default_projects_dir()) else {
        bail!("'{name}' has no transcript yet");
    };

    let mut t = Transcript::open(&path, BACKFILL_BYTES)?;
    println!("{} · {} · {} entries\n", s.name, s.repo(), t.len());
    for entry in t.tail(lines) {
        println!("{}", render(entry));
    }

    if !watch {
        return Ok(());
    }
    loop {
        // Polled rather than watched: a transcript under an active session
        // changes constantly, and the pane redraws on a tick regardless.
        std::thread::sleep(Duration::from_millis(400));
        let added = t.poll()?;
        for entry in t.tail(added) {
            println!("{}", render(entry));
        }
    }
}

fn board(view: BoardView, path: Option<PathBuf>) -> Result<()> {
    let db = Db::open(path.unwrap_or_else(db::default_path))?;
    match view {
        BoardView::Ls => {
            for t in db.board()? {
                let waiting = if t.waiting_on.is_empty() {
                    String::new()
                } else {
                    format!("waits on {}", t.waiting_on.join(", "))
                };
                let note = t.blocked_on.clone().unwrap_or(waiting);
                println!(
                    "  {:<8} {:<12} {:<14} {:<40} {}",
                    t.state.as_str(),
                    t.key,
                    t.agent.as_deref().unwrap_or("-"),
                    t.title,
                    note
                );
            }
        }
        BoardView::Ready => {
            for t in db.ready()? {
                println!("  {:<12} {:<40} {}", t.key, t.title, t.repo);
            }
        }
        BoardView::Agents => {
            for a in db.agents()? {
                println!(
                    "  {:<14} {:<7} {:<12} {:<30} {} bg",
                    a.name,
                    a.role,
                    a.task_key.as_deref().unwrap_or("-"),
                    a.task_title.as_deref().unwrap_or(""),
                    a.bg_running
                );
            }
        }
        BoardView::Bg => {
            for b in db.background()? {
                let port = b.port.map(|p| format!(":{p}")).unwrap_or_default();
                println!(
                    "  {:<4} {:<8} {:<14} {:<30} {:<6} {}",
                    b.id,
                    b.state,
                    b.agent.as_deref().unwrap_or("-"),
                    b.command,
                    port,
                    b.detail.as_deref().unwrap_or("")
                );
            }
        }
        BoardView::Log { limit } => {
            for e in db.events(limit)? {
                let route = match (&e.from_agent, &e.to_agent) {
                    (Some(f), Some(t)) => format!("{f} -> {t}"),
                    (Some(f), None) => f.clone(),
                    _ => String::new(),
                };
                println!(
                    "  {:<20} {:<7} {:<24} {:<12} {}",
                    &e.ts[..e.ts.len().min(19)],
                    e.kind,
                    route,
                    e.task_key.as_deref().unwrap_or(""),
                    e.summary
                );
            }
        }
    }
    Ok(())
}

fn render(e: &Entry) -> String {
    let at = e.hhmm();
    match e {
        Entry::Prompt { text, .. } => format!("{at}  > {}", first_line(text)),
        Entry::CrossSessionMessage { from, text, .. } => {
            format!("{at}  <- {from}: {}", first_line(text))
        }
        Entry::Say { text, .. } => format!("{at}     {}", first_line(text)),
        Entry::Thought { chars, .. } => format!("{at}     ... thought {chars} chars"),
        Entry::Tool {
            name,
            target,
            outcome,
            sidechain,
            ..
        } => {
            let mark = if *sidechain { "  ~" } else { "  *" };
            let result = match outcome {
                Outcome::Pending => "running".into(),
                Outcome::Ok(s) => s.clone(),
                Outcome::Failed(s) => format!("failed: {s}"),
                Outcome::Background(id) => format!("bg {id}"),
            };
            format!("{at}{mark} {name:<6} {target:<44} {result}")
        }
        Entry::Turn { secs, .. } => format!("{at}     --- turn {secs:.1}s"),
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() > 76 {
        let cut: String = line.chars().take(75).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}
