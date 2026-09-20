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

use anyhow::{Context, Result};

use crate::db::Db;
use crate::registry::{self, Registry, Session};
use crate::tmux::{self, Pane, Tmux};

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
    pub pane: Pane,
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
pub fn start(
    tmux: &Tmux,
    db: &Db,
    name: &str,
    repo: &Path,
    command: &str,
    naming: Naming,
) -> Result<Spawned> {
    let repo = repo
        .canonicalize()
        .with_context(|| format!("no such repository: {}", repo.display()))?;
    let name = match naming {
        Naming::Unique => unique_name(db, name)?,
        Naming::Exact => name.to_string(),
    };
    let pane = tmux.spawn(&name, &repo, command)?;
    let target = format!("{}:{}", pane.session, pane.window_name);

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

    Ok(Spawned { name, pane })
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

/// Record which session an agent turned out to be.
pub fn link(db: &Db, name: &str, session_id: &str) -> Result<()> {
    db.upsert_agent(name, None, None, Some(session_id), None, None)
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
