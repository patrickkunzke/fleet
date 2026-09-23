//! Until the TUI lands, this binary is how the pieces get exercised against
//! real data: live sessions, one session's transcript, and the board.
//!
//! Board *writes* still belong to cli/board.sh. The write path exists in
//! [`db`] and is tested, but moving half the subcommands over would leave a
//! CLI where some verbs write and others do not — worse than either end state.
//! It migrates in one step when the TUI needs it.

mod agent;
mod brief;
mod db;
mod msg;
mod registry;
mod tmux;
mod transcript;
mod ui;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::db::Db;
use crate::registry::{Change, Registry, Watcher};
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
    /// The fleet view. This is what running `fleet` with no arguments does.
    Tui {
        /// The workspace: which sessions to show, and where `n` looks for
        /// repositories. Defaults to the directory you are standing in.
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long, env = "FLEET_DB")]
        db: Option<PathBuf>,
        /// Draw one frame to stdout instead of taking over the terminal.
        #[arg(long, value_name = "WIDTHxHEIGHT")]
        snapshot: Option<String>,
        /// Which centre view to draw: session (default), graph, or log.
        #[arg(long)]
        view: Option<String>,
    },
    /// Draw one frame of an invented fleet and hand the shell back.
    ///
    /// For working on the layout: no real agents, no real board, and nothing
    /// to restart. `dev.sh` runs it on every save.
    Preview {
        /// How big to draw it. Must fit the window.
        #[arg(long, value_name = "WIDTHxHEIGHT", default_value = "110x32")]
        size: String,
        /// Which centre view: session (default), graph, or log.
        #[arg(long)]
        view: Option<String>,
        /// Text instead of colour, for a diff or a pipe.
        #[arg(long)]
        plain: bool,
    },
    /// Show what each key arrives as, and what fleet sends on to an agent.
    /// For when a key does not do in fleet what it does in the terminal.
    Keys,
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
        /// The task it is for, as a board key. The agent opens already
        /// briefed on it, and the board records it as claimed.
        #[arg(long)]
        task: Option<String>,
        /// `chief` for the session that plans and dispatches. It is briefed
        /// differently and starts without the editing tools.
        #[arg(long, default_value = "worker")]
        role: String,
        /// What to run instead. Overriding it skips the briefing, which is
        /// how the plumbing is exercised without starting a real agent.
        #[arg(long)]
        command: Option<String>,
        /// tmux session to spawn into. Defaults to the current one, else "fleet".
        #[arg(long)]
        session: Option<String>,
        /// Seconds to wait for the agent to register itself.
        #[arg(long, default_value_t = 20)]
        timeout: u64,
        #[arg(long, env = "FLEET_DB")]
        db: Option<PathBuf>,
    },
    /// Repositories an agent could be started in — what `n` offers.
    Repos {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long, env = "FLEET_DB")]
        db: Option<PathBuf>,
    },
    /// Read and write the coordination database.
    Board {
        #[command(subcommand)]
        cmd: BoardCmd,
        /// Override the database path.
        #[arg(long, env = "FLEET_DB")]
        db: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum BoardCmd {
    /// Create the database.
    Init,
    /// Every task, with what it is waiting for.
    Ls {
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        epic: Option<String>,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Queued tasks whose dependencies are all done — the dispatch queue.
    Ready,
    /// One task, its brief, and everything that happened to it.
    Show { task: String },
    /// The flow log, newest first.
    Log {
        #[arg(default_value_t = 20)]
        limit: usize,
    },
    /// Create or retitle an epic.
    Epic { key: String, title: String },
    /// Queue a task.
    Add {
        task: String,
        repo: PathBuf,
        title: String,
        #[arg(long)]
        epic: Option<String>,
        /// What a fresh agent in that repo needs in order to start.
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "dep")]
        deps: Vec<String>,
        #[arg(long, default_value_t = 0)]
        pos: i64,
    },
    /// Record that one task waits on another.
    Dep { task: String, depends_on: String },
    /// Register or update an agent.
    Agent {
        name: String,
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        tmux: Option<String>,
        #[arg(long)]
        branch: Option<String>,
    },
    /// Live agents and what each holds.
    Agents,
    /// Mark an agent ended.
    Retire { name: String },
    /// Assign a task.
    Claim { task: String, agent: String },
    /// Pick a task up.
    Start { task: String },
    /// Stop, with a reason.
    Block { task: String, reason: String },
    /// Carry on.
    Unblock { task: String },
    /// Open for review rather than finished.
    Review {
        task: String,
        #[arg(long)]
        mr: Option<String>,
    },
    /// Finish, and report what it freed.
    Done {
        task: String,
        #[arg(long)]
        mr: Option<String>,
    },
    /// Abandon.
    Drop {
        task: String,
        reason: Option<String>,
    },
    /// Log a message between agents.
    Msg {
        from: String,
        to: String,
        summary: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        task: Option<String>,
    },
    /// Log something that is not a message.
    Note {
        summary: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        task: Option<String>,
    },
    /// Background processes: running, or finished in the last hour.
    Bg {
        #[command(subcommand)]
        what: Option<BgCmd>,
    },
    /// Read-only SQL, for what the commands do not cover.
    Sql { query: String },
}

#[derive(Subcommand)]
enum BgCmd {
    /// Record a process that outlives a turn. Prints its id.
    Start {
        agent: String,
        command: String,
        #[arg(long, default_value = "script")]
        kind: String,
        #[arg(long)]
        port: Option<i64>,
        #[arg(long)]
        log: Option<String>,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Close one out.
    End {
        id: i64,
        /// passed, failed or killed.
        state: String,
        #[arg(long)]
        detail: Option<String>,
    },
    /// List them.
    Ls,
}

/// Knock at the recipient's door, and say whether anyone was in.
///
/// Every outcome is reported rather than returned as an error: the message
/// is already on the board by the time this runs, and a sender who is told
/// "failed" will send it again.
fn knock(
    db: &Db,
    from: &str,
    to: &str,
    task: Option<&str>,
    summary: &str,
    body: Option<&str>,
) -> String {
    if from == to {
        return "not delivered: a message to yourself is a note".into();
    }
    let Ok(agents) = db.agents() else {
        return "not delivered: the board could not be read".into();
    };
    let Some(agent) = agents.iter().find(|a| a.name == to) else {
        // Almost always a typo in the name, and the flow log would show the
        // message going to an agent that does not exist.
        return format!("not delivered: no agent '{to}' on the board");
    };
    let Some(target) = agent.tmux_target.as_deref() else {
        return format!("not delivered: {to} has no pane fleet can reach");
    };
    let Ok(tmux) = Tmux::detect(None) else {
        return "not delivered: no tmux".into();
    };
    let text = msg::line(from, task, summary, body);
    match msg::deliver(&tmux, target, &text) {
        Ok(true) => format!("delivered to {to} in {target}"),
        Ok(false) => format!("not delivered: nothing running in {target}"),
        Err(e) => format!("not delivered: {e}"),
    }
}

/// `110x32`, as both `--snapshot` and `--preview` spell a size.
fn parse_size(size: &str) -> Result<(u16, u16)> {
    size.split_once('x')
        .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
        .with_context(|| format!("expected WIDTHxHEIGHT, such as 110x28, not '{size}'"))
}

fn main() -> Result<()> {
    // Rust ignores SIGPIPE, so `fleet repos | head` panics on the write that
    // follows head exiting. Every other unix tool dies quietly there.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Tui {
        root: None,
        db: None,
        snapshot: None,
        view: None,
    }) {
        Command::Tui {
            root,
            db,
            snapshot,
            view,
        } => {
            let path = db.unwrap_or_else(db::default_path);
            let db = Db::open(&path)?;
            // Standing somewhere is the usual way of saying which workspace
            // you mean, so it does not need a flag.
            let root = root.or_else(|| std::env::current_dir().ok());
            match snapshot {
                Some(size) => {
                    let (w, h) = parse_size(&size)?;
                    ui::snapshot(db, path, root, w, h, view.as_deref())
                }
                None => ui::run(db, path, root),
            }
        }
        Command::Preview { size, view, plain } => {
            let (w, h) = parse_size(&size)?;
            ui::preview::run(w, h, view.as_deref(), plain)
        }
        Command::Keys => ui::keys(),
        Command::Sessions { watch } => sessions(watch),
        Command::Session { name, watch, lines } => session(&name, watch, lines),
        Command::Board { cmd, db } => board(cmd, db),
        Command::Repos { root, db } => {
            let db = Db::open(db.unwrap_or_else(db::default_path))?;
            let taken: Vec<String> = db.agents()?.into_iter().filter_map(|a| a.repo).collect();
            let found = agent::candidates(&root, &taken);
            println!("{} repositories under {}", found.len(), root.display());
            for c in found {
                let note = if c.taken { "has an agent" } else { "" };
                println!("  {:<34} {:<14} {}", c.name, note, c.path.display());
            }
            Ok(())
        }
        Command::Spawn {
            name,
            repo,
            task,
            role,
            command,
            session,
            timeout,
            db,
        } => spawn(
            &name,
            &repo,
            task.as_deref(),
            &role,
            command.as_deref(),
            session.as_deref(),
            timeout,
            db,
        ),
    }
}

/// Start an agent and join the three sources back together: a tmux pane, the
/// Claude Code session that appears inside it, and the row on the board.
#[allow(clippy::too_many_arguments)]
fn spawn(
    name: &str,
    repo: &Path,
    task: Option<&str>,
    role: &str,
    command: Option<&str>,
    session: Option<&str>,
    timeout: u64,
    db_path: Option<PathBuf>,
) -> Result<()> {
    let tmux = Tmux::detect(session)?;
    let db = Db::open(db_path.unwrap_or_else(db::default_path))?;

    // Read the task before the pane exists: a key that is not on the board
    // is a typo worth refusing, not an agent to start and then correct.
    let assignment = task.map(|key| db.show(key)).transpose()?;
    let chief = role == "chief";
    let command = match command {
        Some(given) => given.to_string(),
        None if chief => brief::command(&brief::chief(repo)),
        None => {
            let brief = assignment.as_ref().map_or_else(
                || brief::worker(name, repo, None, None),
                |(t, body, _)| brief::worker(name, repo, Some(t), body.as_deref()),
            );
            brief::command(&brief)
        }
    };

    let naming = if chief {
        // One chief. Restarting it must replace the row rather than leave a
        // chief-2 behind that nothing dispatches through.
        agent::Naming::Exact
    } else {
        agent::Naming::Unique
    };
    let spawned = agent::start(&tmux, &db, name, repo, &command, naming)?;
    if chief {
        // The board has to agree, or the rail draws it as a worker and the
        // TUI starts a second chief alongside it.
        db.upsert_agent(&spawned.name, Some("chief"), None, None, None, None)?;
    }
    if let Some(key) = task {
        // The agent has been told; the board has to agree, or the chief
        // dispatches the same task twice.
        db.claim(key, &spawned.name)?;
    }
    println!(
        "{}  pane {} in session {}",
        spawned.name, spawned.pane.id, spawned.pane.session
    );

    match agent::adopt(spawned.pane.pid, Duration::from_secs(timeout)) {
        Some(found) => {
            agent::link(&db, &spawned.name, &found.session_id)?;
            println!("      adopted session {} (pid {})", found.session_id, found.pid);
        }
        None => println!(
            "      no session registered within {timeout}s — the pane is up, \
             the board row is unlinked"
        ),
    }
    Ok(())
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

/// True when output is meant for something that will parse it.
fn as_json() -> bool {
    std::env::var("FLEET_JSON").map(|v| v == "1").unwrap_or(false)
}

fn emit<T: serde::Serialize>(value: &T) -> Result<bool> {
    if as_json() {
        println!("{}", serde_json::to_string_pretty(value)?);
        return Ok(true);
    }
    Ok(false)
}

fn board(cmd: BoardCmd, path: Option<PathBuf>) -> Result<()> {
    let path = path.unwrap_or_else(db::default_path);
    let db = Db::open(&path)?;

    match cmd {
        BoardCmd::Init => println!("fleet: ready at {}", path.display()),

        BoardCmd::Ls { state, epic, repo } => {
            let wanted = state.as_deref().map(db::State::parse).transpose()?;
            let tasks: Vec<_> = db
                .board()?
                .into_iter()
                .filter(|t| wanted.is_none_or(|w| t.state == w))
                .filter(|t| epic.as_ref().is_none_or(|e| t.epic_key.as_ref() == Some(e)))
                .filter(|t| repo.as_ref().is_none_or(|r| t.repo.contains(r.as_str())))
                .collect();
            if emit(&tasks)? {
                return Ok(());
            }
            for t in tasks {
                let note = t.blocked_on.clone().unwrap_or_else(|| {
                    if t.waiting_on.is_empty() {
                        String::new()
                    } else {
                        format!("waits on {}", t.waiting_on.join(", "))
                    }
                });
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

        BoardCmd::Ready => {
            let ready = db.ready()?;
            if emit(&ready)? {
                return Ok(());
            }
            for t in ready {
                println!("  {:<12} {:<40} {}", t.key, t.title, t.repo);
            }
        }

        BoardCmd::Show { task } => {
            let (task, body, history) = db.show(&task)?;
            if as_json() {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "task": task, "body": body, "history": history
                    }))?
                );
                return Ok(());
            }
            println!("{}  {}  [{}]", task.key, task.title, task.state.as_str());
            println!("  repo    {}", task.repo);
            if let Some(agent) = &task.agent {
                println!("  agent   {agent}");
            }
            if let Some(why) = &task.blocked_on {
                println!("  blocked {why}");
            }
            if !task.waiting_on.is_empty() {
                println!("  waits   {}", task.waiting_on.join(", "));
            }
            if let Some(mr) = &task.mr_url {
                println!("  mr      {mr}");
            }
            if let Some(body) = body {
                println!("\n{body}");
            }
            println!();
            for e in history {
                println!("  {}  {:<10} {}", &e.ts[..e.ts.len().min(19)], e.kind, e.summary);
            }
        }

        BoardCmd::Log { limit } => {
            let events = db.events(limit)?;
            if emit(&events)? {
                return Ok(());
            }
            for e in events {
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

        BoardCmd::Epic { key, title } => {
            db.upsert_epic(&key, &title)?;
            println!("epic {key}");
        }

        BoardCmd::Add {
            task,
            repo,
            title,
            epic,
            body,
            deps,
            pos,
        } => {
            let repo = repo
                .canonicalize()
                .with_context(|| format!("no such repository: {}", repo.display()))?;
            let deps: Vec<&str> = deps.iter().map(String::as_str).collect();
            db.add_task(&db::NewTask {
                key: &task,
                title: &title,
                repo: &repo.to_string_lossy(),
                epic: epic.as_deref(),
                body: body.as_deref(),
                deps: &deps,
                position: pos,
            })?;
            println!("queued {task}  {title}");
        }

        BoardCmd::Dep { task, depends_on } => {
            db.add_dep(&task, &depends_on)?;
            println!("{task} waits on {depends_on}");
        }

        BoardCmd::Agent {
            name,
            role,
            repo,
            session,
            tmux,
            branch,
        } => {
            let repo = repo.map(|r| r.canonicalize()).transpose()?;
            db.upsert_agent(
                &name,
                role.as_deref(),
                repo.as_ref().map(|r| r.to_string_lossy()).as_deref(),
                session.as_deref(),
                tmux.as_deref(),
                branch.as_deref(),
            )?;
            println!("agent {name}");
        }

        BoardCmd::Agents => {
            let agents = db.agents()?;
            if emit(&agents)? {
                return Ok(());
            }
            for a in agents {
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

        BoardCmd::Retire { name } => {
            db.retire_agent(&name)?;
            println!("retired {name}");
        }

        BoardCmd::Claim { task, agent } => {
            db.claim(&task, &agent)?;
            println!("{task} -> {agent}");
        }

        BoardCmd::Start { task } => {
            db.transition(&task, db::State::Running, None)?;
            println!("running {task}");
        }

        BoardCmd::Block { task, reason } => {
            db.transition(&task, db::State::Blocked, Some(&reason))?;
            println!("blocked {task} — {reason}");
        }

        BoardCmd::Unblock { task } => {
            db.transition(&task, db::State::Running, None)?;
            println!("running {task}");
        }

        BoardCmd::Review { task, mr } => {
            db.transition(&task, db::State::Review, mr.as_deref())?;
            if let Some(mr) = &mr {
                db.set_mr(&task, mr)?;
            }
            println!("review {task}");
        }

        BoardCmd::Done { task, mr } => {
            if let Some(mr) = &mr {
                db.set_mr(&task, mr)?;
            }
            let freed = db.transition(&task, db::State::Done, mr.as_deref())?;
            println!("done {task}");
            // The chief's cue to dispatch again, so it is said rather than
            // left to be noticed.
            if !freed.is_empty() {
                println!("unblocked: {}", freed.join(" "));
            }
        }

        BoardCmd::Drop { task, reason } => {
            db.transition(&task, db::State::Dropped, reason.as_deref())?;
            println!("dropped {task}");
        }

        BoardCmd::Msg {
            from,
            to,
            summary,
            body,
            task,
        } => {
            // The record first. Delivery is best-effort and must never cost
            // us the event: an agent that has died still said this, and the
            // flow log is the only place that survives it.
            db.log_event(
                "message",
                Some(&from),
                Some(&to),
                task.as_deref(),
                &summary,
                body.as_deref(),
                None,
            )?;
            println!("{from} -> {to}: {summary}");
            println!("      {}", knock(&db, &from, &to, task.as_deref(), &summary, body.as_deref()));
        }

        BoardCmd::Note {
            summary,
            body,
            task,
        } => {
            db.log_event("note", None, None, task.as_deref(), &summary, body.as_deref(), None)?;
            println!("noted");
        }

        BoardCmd::Bg { what } => match what {
            Some(BgCmd::Start {
                agent,
                command,
                kind,
                port,
                log,
                repo,
            }) => {
                let id = db.bg_start(&agent, &command, &kind, port, log.as_deref(), repo.as_deref())?;
                // Only the id: callers capture this into a variable.
                println!("{id}");
            }
            Some(BgCmd::End { id, state, detail }) => {
                let command = db.bg_end(id, &state, detail.as_deref())?;
                println!("{state} {command}");
            }
            Some(BgCmd::Ls) | None => {
                let bg = db.background()?;
                if emit(&bg)? {
                    return Ok(());
                }
                for b in bg {
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
        },

        BoardCmd::Sql { query } => {
            let (columns, rows) = db.query(&query)?;
            println!("  {}", columns.join("  "));
            for row in rows {
                println!("  {}", row.join("  "));
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
