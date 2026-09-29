//! Starting an agent, and joining it back up with the board.
//!
//! Three things have to agree afterwards: a tmux pane, the Claude Code
//! session that appears inside it, and a row on the board. Creating the pane
//! is instant; the session takes seconds to register itself. So the two are
//! separated here — [`start`] returns as soon as the pane exists, and
//! [`adopt`] is what waits.
//!
//! The CLI does both in sequence. The TUI does the first on the keystroke and
//! the second on a worker thread, because a UI that freezes for ten seconds
//! after every spawn is a UI nobody spawns from.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::brief;
use crate::db::{Db, RunAgent};
use crate::host::{self, Host, Placed, What};
use crate::registry::{self, Registry, Session};
use crate::tmux;

/// What a repository offers as an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub name: String,
    /// Whether the board already has an agent working here.
    pub taken: bool,
}

pub struct Spawned {
    pub name: String,
    pub placed: Placed,
}

/// Whether a clashing name is worked around or reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Naming {
    /// A second agent in a repo gets its own name.
    Unique,
    /// Take the name as given, replacing whatever row holds it. What
    /// restarting a named agent — the chief — has to do, or every launch
    /// would leave another chief-2 behind.
    Exact,
}

/// Open a pane and put the agent on the board, without waiting for it.
///
/// `run` is the run it joins. Its id also goes into the agent's environment
/// as `FLEET_RUN`, so that an agent the chief starts with `fleet spawn`
/// lands in the chief's run rather than in whichever one is newest.
#[allow(clippy::too_many_arguments)]
pub fn start(
    host: &Host,
    db: &Db,
    name: &str,
    repo: &Path,
    what: &What,
    naming: Naming,
    role: &str,
    run: Option<i64>,
) -> Result<Spawned> {
    let repo = repo
        .canonicalize()
        .with_context(|| format!("no such repository: {}", repo.display()))?;
    // herdr takes a narrower set of names than tmux, and the board's name is
    // the one a message is addressed to, so they have to be the same.
    let wanted = if host.is_herdr() { host::herdr_safe(name) } else { name.to_string() };
    let name = match naming {
        Naming::Unique => unique_name(db, &wanted)?,
        Naming::Exact => wanted,
    };
    let command = format!("{}{}", environment(run, db.path()), host.line(what, &name)?);
    let placed = host.open(&name, &repo, &command, role == "chief")?;
    if let Some(id) = run {
        db.join_run(id, &name, role, &repo.to_string_lossy())?;
    }
    let target = placed.target.clone();

    // The row goes in before the session exists. An agent that never reports
    // is still one somebody has to deal with, and a board that omits it is
    // worse than a board that shows it unlinked.
    db.upsert_agent(
        &name,
        None,
        Some(&repo.to_string_lossy()),
        None,
        Some(&target),
        None,
    )?;
    db.log_event("note", Some(&name), None, None, &format!("spawned in {target}"), None, None)?;

    Ok(Spawned { name, placed })
}

/// What the agent's shell is told before the command: the run it joins, and
/// its fleet's board. The board is what makes it part of the fleet: a
/// session without one is refused by the board, which is how a session
/// fleet did not start is kept off it. A new terminal gets its environment
/// from the multiplexer's server, not from fleet, so it has to be said here.
fn environment(run: Option<i64>, board: Option<&Path>) -> String {
    let mut out = String::new();
    if let Some(id) = run {
        out.push_str(&format!("FLEET_RUN={id} "));
    }
    if let Some(board) = board {
        out.push_str(&format!("FLEET_DB={} ", brief::quote(&board.to_string_lossy())));
    }
    out
}

/// Two agents in one repo is ordinary here, so a name is made unique rather
/// than refused — but only when it is actually in use by a live agent.
fn unique_name(db: &Db, wanted: &str) -> Result<String> {
    let taken: Vec<String> = db.agents()?.into_iter().map(|a| a.name).collect();
    if !taken.iter().any(|n| n == wanted) {
        return Ok(wanted.to_string());
    }
    for n in 2..100 {
        let candidate = format!("{wanted}-{n}");
        if !taken.iter().any(|name| *name == candidate) {
            return Ok(candidate);
        }
    }
    Ok(wanted.to_string())
}

/// Wait for the Claude Code session that appears inside a pane.
///
/// Matched by process tree rather than by working directory: two agents in
/// the same repo are ordinary, and a cwd match would adopt the wrong one.
pub fn adopt(pane_pid: i32, timeout: Duration) -> Option<Session> {
    let mut reg = Registry::new(registry::default_dir());
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        reg.refresh();
        if let Some(found) = reg
            .interactive()
            .find(|s| tmux::owns(pane_pid, s.pid))
            .cloned()
        {
            return Some(found);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}

/// Record which session an agent turned out to be — on the board, and in
/// its run, which is what a later resume reads.
pub fn link(db: &Db, name: &str, session_id: &str, run: Option<i64>) -> Result<()> {
    db.upsert_agent(name, None, None, Some(session_id), None, None)?;
    match run {
        Some(id) => db.set_run_session(id, name, session_id)?,
        None => db.set_session_in_latest_run(name, session_id)?,
    }
    Ok(())
}

/// Bring an agent from an earlier run back, into its own conversation.
///
/// `claude --resume` continues a session under the same id, so the agent
/// comes back as itself: its history, its context, the work it was halfway
/// through. The role goes back into the system prompt, and the chief's
/// editing tools stay withheld — both are set at launch, not kept with the
/// conversation, so a resume that skipped them would be a chief that could
/// write code again.
pub fn resume(host: &Host, db: &Db, run: i64, member: &RunAgent) -> Result<Spawned> {
    // herdr's panes start claude by its full path; see `brief::claude_program`.
    let program = if host.is_herdr() { brief::claude_program() } else { "claude".into() };
    resume_from(host, db, run, member, &registry::default_projects_dir(), &program)
}

/// `resume`, with Claude Code's projects directory and program given rather
/// than found — for a test, which must not start the real thing.
pub fn resume_from(
    host: &Host,
    db: &Db,
    run: i64,
    member: &RunAgent,
    projects: &Path,
    program: &str,
) -> Result<Spawned> {
    let session = member
        .session_id
        .as_deref()
        .with_context(|| format!("{} never reported a session to resume", member.name))?;
    let repo = Path::new(&member.repo);
    if !conversation_in(projects, repo, session) {
        bail!("{}'s conversation is no longer on disk", member.name);
    }
    let brief = if member.role == "chief" {
        brief::chief(repo)
    } else {
        brief::worker(&member.name, repo, None, None)
    };
    let what = What::Resume { brief: &brief, session, program };
    let spawned = start(
        host,
        db,
        &member.name,
        repo,
        &what,
        Naming::Exact,
        &member.role,
        Some(run),
    )?;
    db.upsert_agent(&member.name, Some(&member.role), None, Some(session), None, None)?;
    db.set_run_session(run, &member.name, session)?;
    db.log_event("note", Some(&member.name), None, None, "resumed", None, None)?;
    Ok(spawned)
}

/// Whether Claude Code still has the conversation. `--resume` on one that is
/// gone opens an empty session that looks, at a glance, like the old one.
pub fn conversation_exists(repo: &Path, session: &str) -> bool {
    conversation_in(&registry::default_projects_dir(), repo, session)
}

fn conversation_in(projects: &Path, repo: &Path, session: &str) -> bool {
    projects
        .join(registry::project_slug(repo))
        .join(format!("{session}.jsonl"))
        .is_file()
}

/// Repositories under `root` that an agent could be started in.
///
/// Depth-limited and pruned: a workspace holds a great many directories and
/// almost none of them are repositories. Stops descending into a repository
/// once found, so a vendored checkout inside one does not become a candidate
/// of its own.
pub fn candidates(root: &Path, existing: &[String]) -> Vec<Candidate> {
    const MAX_DEPTH: usize = 3;
    const SKIP: [&str; 6] = ["node_modules", "target", "vendor", "dist", "build", ".git"];

    fn walk(dir: &Path, depth: usize, max: usize, skip: &[&str], out: &mut Vec<PathBuf>) {
        if depth > max {
            return;
        }
        if dir.join(".git").exists() {
            out.push(dir.to_path_buf());
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if skip.contains(&name) || name.starts_with('.') {
                continue;
            }
            walk(&path, depth + 1, max, skip, out);
        }
    }

    let mut found = Vec::new();
    walk(root, 0, MAX_DEPTH, &SKIP, &mut found);
    found.sort();

    found
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("repo")
                .to_string();
            let taken = existing.iter().any(|e| Path::new(e) == path);
            Candidate { path, name, taken }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_at(root: &Path, rel: &str) {
        let dir = root.join(rel);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
    }

    #[test]
    fn it_finds_repositories_at_the_depths_a_workspace_uses() {
        let root = tempfile::tempdir().unwrap();
        repo_at(root.path(), "admin");
        repo_at(root.path(), "service/billing-service");
        repo_at(root.path(), "ui/storefront");
        // Too deep to be a repo anyone organises this way.
        repo_at(root.path(), "a/b/c/d/buried");
        // Not a repository at all.
        std::fs::create_dir_all(root.path().join("docs")).unwrap();

        let names: Vec<_> = candidates(root.path(), &[])
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["admin", "billing-service", "storefront"]);
    }

    #[test]
    fn it_does_not_descend_into_a_repository_it_has_already_found() {
        let root = tempfile::tempdir().unwrap();
        repo_at(root.path(), "storefront");
        // A checkout inside a checkout is not a second candidate.
        repo_at(root.path(), "storefront/vendor/thing");
        repo_at(root.path(), "storefront/packages/inner");

        let names: Vec<_> = candidates(root.path(), &[])
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["storefront"]);
    }

    #[test]
    fn a_repo_that_already_has_an_agent_is_marked_not_hidden() {
        let root = tempfile::tempdir().unwrap();
        repo_at(root.path(), "billing-service");
        repo_at(root.path(), "storefront");
        let existing = vec![
            root.path()
                .join("billing-service")
                .to_string_lossy()
                .to_string(),
        ];

        let found = candidates(root.path(), &existing);
        let taken: Vec<_> = found.iter().filter(|c| c.taken).map(|c| &c.name).collect();
        assert_eq!(taken, vec!["billing-service"]);
        assert_eq!(found.len(), 2, "a second agent in one repo is allowed");
    }

    fn member(name: &str, role: &str, repo: &Path, session: Option<&str>) -> RunAgent {
        RunAgent {
            name: name.into(),
            role: role.into(),
            repo: repo.to_string_lossy().to_string(),
            session_id: session.map(str::to_string),
            retired: false,
        }
    }

    #[test]
    fn an_agent_that_never_reported_a_session_is_not_resumed() {
        let db = Db::open_in_memory().unwrap();
        let run = db.start_run("/w").unwrap();
        let Ok(tmux) = crate::tmux::Tmux::detect(Some("unused")) else { return };
        let dir = tempfile::tempdir().unwrap();
        let err = resume_from(&Host::Tmux(tmux.clone()), &db, run, &member("x", "worker", dir.path(), None), dir.path(), "false")
            .err()
            .expect("refused");
        assert!(err.to_string().contains("never reported a session"), "{err}");
    }

    #[test]
    fn an_agent_whose_conversation_is_gone_is_not_resumed_into_an_empty_one() {
        // `claude --resume` on a missing conversation does not come back as
        // the old agent, and a pane that looks like it did is worse.
        let db = Db::open_in_memory().unwrap();
        let run = db.start_run("/w").unwrap();
        let Ok(tmux) = crate::tmux::Tmux::detect(Some("unused")) else { return };
        let dir = tempfile::tempdir().unwrap();
        let err = resume_from(&Host::Tmux(tmux.clone()), &db, run, &member("x", "worker", dir.path(), Some("s")), dir.path(), "false")
            .err()
            .expect("refused");
        assert!(err.to_string().contains("no longer on disk"), "{err}");
    }

    #[test]
    fn a_resumed_agent_comes_back_into_its_own_conversation_and_its_run() {
        use std::time::Duration;

        let socket = format!("fleet-resume-{}", std::process::id());
        let Ok(tmux) = crate::tmux::Tmux::detect(Some(&socket)) else { return };
        let tmux = tmux.on_socket(&socket);
        if tmux.ensure_session().is_err() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("billing-service");
        std::fs::create_dir_all(&repo).unwrap();
        let repo = repo.canonicalize().unwrap();

        // The conversation, where Claude Code keeps it.
        let projects = dir.path().join("projects");
        let slug = projects.join(registry::project_slug(&repo));
        std::fs::create_dir_all(&slug).unwrap();
        std::fs::write(slug.join("sess-abc.jsonl"), "{}\n").unwrap();

        // A stand-in for Claude Code that records what it was started with.
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let argv = dir.path().join("argv");
        let env = dir.path().join("env");
        std::fs::write(
            bin.join("claude"),
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\000' \"$a\"; done > {}\nprintf %s \"$FLEET_RUN\" > {}\n",
                argv.display(),
                env.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let db = Db::open_in_memory().unwrap();
        let run = db.start_run("/w").unwrap();
        let who = member("billing-svc", "worker", &repo, Some("sess-abc"));
        let program = bin.join("claude").to_string_lossy().to_string();
        let spawned = resume_from(&Host::Tmux(tmux.clone()), &db, run, &who, &projects, &program);
        for _ in 0..40 {
            if env.is_file() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let got = std::fs::read_to_string(&argv).unwrap_or_default();
        let fleet_run = std::fs::read_to_string(&env).unwrap_or_default();
        let _ = tmux.kill_server();

        assert!(spawned.is_ok(), "{:?}", spawned.err());
        let args: Vec<&str> = got.split('\0').filter(|a| !a.is_empty()).collect();
        assert_eq!(args.get(..2), Some(&["--resume", "sess-abc"][..]), "its own conversation: {args:?}");
        assert!(args.contains(&"--append-system-prompt"), "with its role: {args:?}");
        assert_eq!(fleet_run, run.to_string(), "and knows which run it is in");

        // Back on the rail, as itself.
        let a = db.agents().unwrap().into_iter().find(|a| a.name == "billing-svc").unwrap();
        assert_eq!(a.session_id.as_deref(), Some("sess-abc"));
        let r = db.run(run).unwrap().unwrap();
        assert_eq!(r.agents[0].session_id.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn a_name_already_in_use_gets_a_suffix_rather_than_a_refusal() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_agent("storefront", None, None, None, None, None)
            .unwrap();
        assert_eq!(unique_name(&db, "storefront").unwrap(), "storefront-2");

        db.upsert_agent("storefront-2", None, None, None, None, None)
            .unwrap();
        assert_eq!(unique_name(&db, "storefront").unwrap(), "storefront-3");
        assert_eq!(unique_name(&db, "billing-svc").unwrap(), "billing-svc");
    }
}
