//! fleet, as a herdr plugin: the view the plugin's actions open, the
//! actions themselves, and the commands its agents use — `fleet spawn` for
//! the chief to delegate, and `fleet board` for everyone to coordinate.

mod agent;
mod background;
mod brief;
mod chief;
mod db;
mod herdr;
mod host;
mod msg;
mod plugin;
mod registry;
mod scope;
mod snapshot;
mod ui;
mod update;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::db::Db;
use crate::host::{HerdrHost, Host, What};

#[derive(Parser)]
#[command(name = "fleet", version, about = "Orchestrate Claude Code sessions across repos, inside herdr")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// The fleet view. This is what running `fleet` with no arguments does,
    /// in a herdr pane.
    Tui {
        /// The workspace: which agents to show, and where `n` looks for
        /// repositories. Defaults to the directory you are standing in.
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long, env = "FLEET_DB")]
        db: Option<PathBuf>,
    },
    /// Draw one frame of an invented fleet and hand the shell back.
    ///
    /// For working on the layout: no real agents, no real board, and no
    /// herdr. `dev.sh` runs it on every save.
    Preview {
        /// How big to draw it. Must fit the window.
        #[arg(long, value_name = "WIDTHxHEIGHT", default_value = "110x32")]
        size: String,
        /// Which view: graph (default) or log.
        #[arg(long)]
        view: Option<String>,
        /// Text instead of colour, for a diff or a pipe.
        #[arg(long)]
        plain: bool,
    },
    /// What the plugin's actions run, and a check of the setup they need.
    Herdr {
        #[command(subcommand)]
        cmd: HerdrCmd,
    },
    /// Start the chief of staff in this terminal, on this workspace's board.
    /// Outside herdr, how a fleet begins: the agents it starts run as
    /// Claude Code background sessions, and `/fleet` shows them.
    Chief {
        /// The workspace: the directory that holds the repositories. The
        /// directory it is run in, by default.
        #[arg(long)]
        root: Option<PathBuf>,
        /// Go back into the last chief's conversation, and its run, rather
        /// than starting a new one.
        #[arg(long, conflicts_with = "adopt")]
        resume: bool,
        /// Make a Claude Code session already running the chief, rather than
        /// starting one: what `/fleet start` runs. Prints, as JSON, what that
        /// session's mod needs.
        #[arg(long, value_name = "SESSION")]
        adopt: Option<String>,
    },
    /// Start an agent, in a herdr tab or as a background session, and
    /// register it on the board.
    Spawn {
        /// What to call it. Also the herdr tab's name.
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
        /// Seconds to wait for the agent to register itself.
        #[arg(long, default_value_t = 20)]
        timeout: u64,
        #[arg(long, env = "FLEET_DB")]
        db: Option<PathBuf>,
    },
    /// Hand an agent's task to a fresh session of it, for one whose context
    /// is nearly full. The new session starts in the same repository under
    /// the same name, briefed on the task, its messages and where the old
    /// one left off; the old one is retired and its tab closed.
    Handoff {
        /// The agent to hand off.
        agent: String,
        /// Anything the new session should know that the board does not say.
        #[arg(long)]
        note: Option<String>,
        /// Close the old session even mid-turn. Without it, an agent that is
        /// working is left to finish its turn first.
        #[arg(long)]
        now: bool,
        /// Seconds to wait for the new session to register itself.
        #[arg(long, default_value_t = 20)]
        timeout: u64,
        #[arg(long, env = "FLEET_DB")]
        db: Option<PathBuf>,
    },
    /// Read and write the coordination database: the board of the fleet
    /// this session was started by, or the one named.
    Board {
        #[command(subcommand)]
        cmd: BoardCmd,
        /// The board. Every agent a fleet starts carries its own.
        #[arg(long, env = "FLEET_DB", global = true)]
        db: Option<PathBuf>,
        /// Another fleet's board, by the name `fleet fleets` prints.
        #[arg(long, global = true)]
        fleet: Option<String>,
    },
    /// Every fleet there is, and the directory each one covers.
    Fleets,
    /// Install the newest release of fleet. herdr has no update command;
    /// this installs again, at the newest release, through herdr.
    Update {
        /// Only say which version is installed and which is the newest.
        #[arg(long)]
        check: bool,
        /// Skip herdr's look at what it is about to run.
        #[arg(long, short)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum HerdrCmd {
    /// Focus this workspace's fleet tab, opening it if there is none. What
    /// the plugin's `fleet.open` action runs.
    Open {
        /// The workspace fleet covers. Defaults to the herdr workspace's
        /// own directory.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Open a new workspace in fleet mode: the fleet view, with the chief
    /// beside it. What the plugin's `fleet.new` action runs.
    New {
        /// Where. Defaults to the focused pane's directory.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// What the plugin's `workspace.created` hook runs: fleet mode, for a
    /// workspace at a directory listed in ~/.claude-fleet/auto-open.
    Event,
    /// Check what fleet needs from herdr and from Claude Code.
    Doctor,
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
        /// Where a message reaches it: `herdr:<name>`.
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        branch: Option<String>,
    },
    /// Live agents and what each holds.
    Agents,
    /// Take an agent off the board, and close its herdr tab.
    ///
    /// Only its pane, when the tab holds the fleet view or another agent.
    /// A working agent's tab is left open, as is the one retiring it.
    Retire {
        name: String,
        /// Leave its tab open.
        #[arg(long)]
        keep_tab: bool,
    },
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
    /// Give a worker the go-ahead on its task, and tell it. Until a task has
    /// one, fleet's mod keeps its worker from editing.
    Go {
        task: String,
        /// What to send with it.
        #[arg(default_value = "go")]
        summary: String,
        #[arg(long)]
        body: Option<String>,
        /// Who gives it.
        #[arg(long, default_value = "chief")]
        from: String,
    },
    /// The fleet as the chief's own session draws it: the crew, the open
    /// tasks and recent events, as JSON. What fleet's mod reads every few
    /// seconds outside herdr.
    #[command(hide = true)]
    Snapshot {
        /// The session asking, so it is told which agent it is.
        #[arg(long)]
        session: Option<String>,
    },
    /// What fleet's mod asks before an edit: who this session is, its open
    /// tasks, and whether one has a go-ahead. With --user-approves, the
    /// person typed a prompt in this pane, which is a go-ahead. Always JSON.
    #[command(hide = true)]
    Gate {
        #[arg(long)]
        session: String,
        #[arg(long)]
        user_approves: bool,
    },
    /// What fleet's Claude Code mod checks every few seconds: who this
    /// session is on the board, and, with --take, the messages waiting for
    /// it. Always JSON.
    #[command(hide = true)]
    Inbox {
        #[arg(long)]
        session: String,
        #[arg(long)]
        take: bool,
        /// The tool the session is running now.
        #[arg(long)]
        tool: Option<String>,
        /// How full its context window is, in percent.
        #[arg(long)]
        context: Option<i64>,
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
    event: i64,
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
    // A session whose mod is checking in takes the message itself, once it
    // is idle, rather than having it typed into a prompt it may be busy at.
    if db.mailbox_open(to, db::MAILBOX_QUIET).unwrap_or(false) {
        return match db.queue_message(event, to) {
            Ok(()) => format!("queued for {to}: it arrives when {to} is next idle"),
            Err(e) => format!("not delivered: {e}"),
        };
    }
    let Some(target) = agent.target.as_deref() else {
        return format!("not delivered: {to} has no pane fleet can reach");
    };
    if background::is_background(target) {
        // No prompt to type at: a background session's messages go through
        // its mod, and the mailbox check above found it quiet.
        return format!("not delivered: {to} runs in the background, and its mod is not checking in; it is on the board");
    }
    if !host::is_herdr(target) {
        // A tmux window, from before fleet ran only in herdr.
        return format!("not delivered: {to} is not in herdr");
    }
    let Some(host) = HerdrHost::from_env() else {
        return format!("not delivered: {to} is in herdr, and this is not");
    };
    let text = msg::line(from, task, summary, body);
    match host.deliver(target, &text) {
        Ok(true) => format!("delivered to {to} in {target}"),
        Ok(false) => format!("not delivered: nothing running in {target}"),
        Err(e) => format!("not delivered: {e}"),
    }
}

/// `110x32`, as `--size` spells it.
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
    match cli.command.unwrap_or(Command::Tui { root: None, db: None }) {
        Command::Tui { root, db } => {
            let host = Host::detect()?;
            // Standing somewhere is the usual way of saying which workspace
            // you mean, so it does not need a flag. The workspace is the
            // fleet, and the fleet has its own board.
            let root = root.or_else(|| std::env::current_dir().ok());
            let path = match (db, &root) {
                (Some(p), _) => p,
                (None, Some(r)) => scope::board_for_root(r)?,
                (None, None) => bail!("no workspace: run fleet in one, or pass --root"),
            };
            let db = Db::open(&path)?;
            ui::run(db, path, root, host)
        }
        Command::Preview { size, view, plain } => {
            let (w, h) = parse_size(&size)?;
            ui::preview::run(w, h, view.as_deref(), plain)
        }
        Command::Herdr { cmd } => match cmd {
            HerdrCmd::Open { root } => plugin::open(root),
            HerdrCmd::New { root } => plugin::new(root),
            HerdrCmd::Event => plugin::event(),
            HerdrCmd::Doctor => plugin::doctor(),
        },
        Command::Board { cmd, db, fleet } => board(cmd, db, fleet),
        Command::Fleets => {
            let all = scope::fleets();
            if all.is_empty() {
                println!("no fleets yet: open fleet in a workspace's directory");
            }
            for (name, root) in all {
                println!("{name:<20} {}", root.display());
            }
            Ok(())
        }
        Command::Update { check, yes } => update::update(check, yes),
        Command::Spawn {
            name,
            repo,
            task,
            role,
            command,
            timeout,
            db,
        } => spawn(&name, &repo, task.as_deref(), &role, command.as_deref(), timeout, db, None),
        Command::Chief { root, resume, adopt } => match adopt {
            Some(session) => chief::adopt(root, &session),
            None => chief::run(root, resume),
        },
        Command::Handoff { agent, note, now, timeout, db } => handoff(&agent, note.as_deref(), now, timeout, db),
    }
}

/// Retire an agent and start a fresh session of it on the same task.
fn handoff(name: &str, note: Option<&str>, now: bool, timeout: u64, db_path: Option<PathBuf>) -> Result<()> {
    let path = scope::board_for_command(db_path.clone(), None)?;
    let host = Host::detect()?.in_fleet(Some(&path));
    let db = Db::open(&path)?;

    let agent = db
        .agents()?
        .into_iter()
        .find(|a| a.name == name)
        .with_context(|| format!("no agent '{name}' on the board"))?;
    if agent.role == "chief" {
        bail!("the chief is not handed off this way: restart it from the fleet tab, and it reads the board afresh");
    }
    let repo = PathBuf::from(agent.repo.as_deref().with_context(|| format!("{name} has no repository on the board"))?);
    if let Some(target) = agent.target.as_deref()
        && !now
        && host.is_working(target)
    {
        bail!("{name} is mid-turn. Run this again once it stops, or with --now to close it where it is");
    }

    // Its task: the one it is on, before one queued for later.
    let task = db
        .board()?
        .into_iter()
        .filter(|t| t.agent.as_deref() == Some(name))
        .filter(|t| !matches!(t.state, db::State::Done | db::State::Dropped))
        .min_by_key(|t| match t.state {
            db::State::Running | db::State::Blocked => 0,
            db::State::Review => 1,
            _ => 2,
        });
    let shown = task.as_ref().map(|t| db.show(&t.key)).transpose()?;
    let approved = match &task {
        Some(t) => db.approved(&t.key)?,
        None => true,
    };
    // What it said and was told, oldest first: the last fifteen.
    let mut messages: Vec<db::Event> = db
        .events(500)?
        .into_iter()
        .filter(|e| e.kind == "message")
        .filter(|e| e.from_agent.as_deref() == Some(name) || e.to_agent.as_deref() == Some(name))
        .take(15)
        .collect();
    messages.reverse();
    let words = agent.session_id.as_deref().and_then(agent::last_words);

    // The old session goes before the new one comes: herdr names a tab for
    // its agent, and two of one name is one too many.
    let closed = match agent.target.as_deref() {
        Some(t) if host::is_herdr(t) || background::is_background(t) => {
            let said = if now { host.close_now(t) } else { host.close(t) };
            said.unwrap_or_else(|e| format!("its tab could not be closed: {e}"))
        }
        _ => "it had no herdr tab".into(),
    };
    if closed.contains("left open") {
        bail!("{name}'s tab could not be closed ({closed}), so it was not handed off");
    }
    db.hand_off(name)?;
    db.log_event("note", Some(name), None, task.as_ref().map(|t| t.key.as_str()), "handed off to a fresh session", note, None)?;
    println!("{name}: old session retired; {closed}");

    let brief = brief::handoff(
        name,
        &repo,
        shown.as_ref().map(|(t, _, _)| t),
        shown.as_ref().and_then(|(_, body, _)| body.as_deref()),
        &brief::Takeover { last_words: words.as_deref(), messages: &messages, note, approved },
    );
    let key = task.as_ref().map(|t| t.key.clone());
    spawn(name, &repo, key.as_deref(), "worker", None, timeout, db_path, Some(brief))
}

/// Start an agent and join the three sources back together: a herdr tab, the
/// Claude Code session that appears inside it, and the row on the board.
#[allow(clippy::too_many_arguments)]
fn spawn(
    name: &str,
    repo: &Path,
    task: Option<&str>,
    role: &str,
    command: Option<&str>,
    timeout: u64,
    db_path: Option<PathBuf>,
    // A brief made elsewhere, for a session taking over from another.
    takeover: Option<brief::Brief>,
) -> Result<()> {
    let path = scope::board_for_command(db_path, None)?;
    let host = Host::detect()?.in_fleet(Some(&path));
    let db = Db::open(&path)?;

    // Read the task before the pane exists: a key that is not on the board
    // is a typo worth refusing, not an agent to start and then correct.
    let assignment = task.map(|key| db.show(key)).transpose()?;
    let chief = role == "chief";
    let brief = if let Some(given) = takeover {
        given
    } else if chief {
        brief::chief(repo)
    } else {
        assignment.as_ref().map_or_else(
            || brief::worker(name, repo, None, None),
            |(t, body, _)| {
                let approved = db.approved(&t.key).unwrap_or(false);
                brief::worker_with(name, repo, Some(t), body.as_deref(), approved)
            },
        )
    };
    let program = brief::claude_program();
    let what = match command {
        Some(given) => What::Command(given),
        None => What::Brief { brief: &brief, program: &program },
    };

    let naming = if chief {
        // One chief. Restarting it must replace the row rather than leave a
        // chief-2 behind that nothing dispatches through.
        agent::Naming::Exact
    } else {
        agent::Naming::Unique
    };
    // The chief's run, when the chief is the one running this: it put
    // FLEET_RUN in its own environment for exactly this. Otherwise the run of
    // whichever workspace the repository sits in.
    let within = repo
        .canonicalize()
        .with_context(|| format!("no such repository: {}", repo.display()))?;
    let run = match std::env::var("FLEET_RUN").ok().and_then(|v| v.parse::<i64>().ok()) {
        Some(id) => Some(id),
        None => db.run_for_repo(&within.to_string_lossy())?,
    };
    let role_name = if chief { "chief" } else { "worker" };
    let spawned = agent::start(&host, &db, name, repo, &what, naming, role_name, run)?;
    if chief {
        // The board has to agree, or the graph draws it as a worker and the
        // view starts a second chief alongside it.
        db.upsert_agent(&spawned.name, Some("chief"), None, None, None, None)?;
    }
    if let Some(key) = task {
        // The agent has been told; the board has to agree, or the chief
        // dispatches the same task twice.
        db.claim(key, &spawned.name)?;
    }
    println!("{}  {}", spawned.name, spawned.placed.place);

    match agent::found_session(&host, &spawned.placed, Duration::from_secs(timeout)) {
        Some(sid) => {
            agent::link(&db, &spawned.name, &sid, run)?;
            println!("      adopted session {sid}");
        }
        None if background::is_background(&spawned.placed.target) => println!(
            "      no session listed yet; `claude agents` shows it, and it is linked once it reports"
        ),
        None => println!(
            "      no session yet — it may be waiting at a dialog in its tab; \
             the fleet tab links it once it has one"
        ),
    }
    // Where to find it, when there is no tab to switch to.
    if background::is_background(&spawned.placed.target) {
        println!("      {}", background::Background::attach_hint(&spawned.placed.target));
    }
    Ok(())
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

fn board(cmd: BoardCmd, path: Option<PathBuf>, fleet: Option<String>) -> Result<()> {
    let path = scope::board_for_command(path, fleet.as_deref())?;
    let mut db = Db::open(&path)?;

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
            target,
            branch,
        } => {
            let repo = repo.map(|r| r.canonicalize()).transpose()?;
            db.upsert_agent(
                &name,
                role.as_deref(),
                repo.as_ref().map(|r| r.to_string_lossy()).as_deref(),
                session.as_deref(),
                target.as_deref(),
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

        BoardCmd::Retire { name, keep_tab } => {
            // Before the row is ended: an ended agent is off the list.
            let target = db.agents()?.into_iter().find(|a| a.name == name).and_then(|a| a.target);
            db.retire_agent(&name)?;
            // Its tab is going, so a resumed run must not bring it back.
            if let Some(run) = std::env::var("FLEET_RUN").ok().and_then(|v| v.parse::<i64>().ok()) {
                db.retire_in_run(run, &name)?;
            }
            let tab = match (keep_tab, target.as_deref()) {
                (true, _) => "its tab is left open".to_string(),
                (false, Some(t)) if host::is_herdr(t) || background::is_background(t) => match Host::detect() {
                    Ok(host) => host.close(t).unwrap_or_else(|e| format!("it could not be closed: {e}")),
                    Err(_) => "its tab is left open: this is not herdr".into(),
                },
                (false, _) => "it had no herdr tab".into(),
            };
            println!("retired {name}; {tab}");
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
            let id = db.log_message(&from, &to, task.as_deref(), &summary, body.as_deref())?;
            println!("{from} -> {to}: {summary}");
            println!("      {}", knock(&db, id, &from, &to, task.as_deref(), &summary, body.as_deref()));
        }

        BoardCmd::Go { task, summary, body, from } => {
            let owner = db.approve(&task, &from)?;
            println!("{task}: go from {from}");
            match owner {
                Some(to) => {
                    let id = db.log_message(&from, &to, Some(&task), &summary, body.as_deref())?;
                    println!("      {}", knock(&db, id, &from, &to, Some(&task), &summary, body.as_deref()));
                }
                // Approved ahead of time: whoever claims it may start at once.
                None => {
                    db.log_event("note", Some(&from), None, Some(&task), "go, before anyone claimed it", None, None)?;
                    println!("      nobody has claimed it yet; whoever does may start at once");
                }
            }
        }

        BoardCmd::Snapshot { session } => {
            println!("{}", serde_json::to_string(&snapshot::take(&db, session.as_deref())?)?);
        }

        BoardCmd::Gate { session, user_approves } => {
            println!("{}", serde_json::to_string(&db.gate(&session, user_approves)?)?);
        }

        BoardCmd::Inbox { session, take, tool, context } => {
            let inbox = db.check_in(&session, take, &db::Vitals { tool, context })?;
            let text = (!inbox.messages.is_empty()).then(|| msg::prompt(&inbox.messages));
            // The mod says this under the prompt, so the agent's own tab
            // shows what it is waiting for as well as the fleet view.
            let gate = db.gate(&session, false)?;
            let awaiting_go = (gate.role.as_deref() == Some("worker") && !gate.approved)
                .then(|| gate.tasks.first().cloned())
                .flatten();
            let out = serde_json::json!({
                "agent": inbox.agent,
                "text": text,
                "waiting": inbox.waiting,
                "awaiting_go": awaiting_go,
            });
            println!("{out}");
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
