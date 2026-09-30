//! The Claude Code session registry.
//!
//! Every running `claude` process writes `~/.claude/sessions/<pid>.json` and
//! keeps it current — name, cwd, busy/idle, and the socket peers talk over.
//! That file is what `ListAgents` reads, and it is what lets the fleet rail
//! populate itself: we never register a session by hand.
//!
//! Two things the directory does not tell us, which this module handles:
//!
//! 1. A crashed process leaves its file behind, so presence is not liveness —
//!    every entry is checked against the actual pid.
//! 2. A file caught mid-write fails to parse, and no further event may ever
//!    arrive for it. So the watcher only signals "something moved" and
//!    [`Registry::refresh`] rescans; a missed write is picked up by the next
//!    refresh rather than lost until the session next changes state.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use serde::Deserialize;

/// Where Claude Code keeps the registry, unless `CLAUDE_CONFIG_DIR` moves it.
pub fn default_dir() -> PathBuf {
    claude_home().join("sessions")
}

/// Where transcripts live, one directory per project.
pub fn default_projects_dir() -> PathBuf {
    claude_home().join("projects")
}

fn claude_home() -> PathBuf {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    PathBuf::from(home).join(".claude")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Idle,
    Busy,
    /// Anything the registry starts writing that we do not know yet. Kept as
    /// written rather than collapsed into Idle, so a new state shows up in the
    /// UI as itself instead of silently looking finished.
    Other(String),
}

impl Status {
    fn parse(s: Option<&str>) -> Status {
        match s {
            Some("idle") => Status::Idle,
            Some("busy") => Status::Busy,
            Some(other) => Status::Other(other.to_string()),
            None => Status::Other(String::new()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub pid: i32,
    pub session_id: String,
    pub cwd: PathBuf,
    pub name: String,
    pub status: Status,
    /// "interactive", "sdk", … — subagents and headless runs are not
    /// interactive and should not get a pane in the fleet rail.
    pub kind: String,
    pub version: String,
    pub socket: Option<PathBuf>,
    pub started_at: Option<SystemTime>,
    pub updated_at: Option<SystemTime>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    pid: i32,
    session_id: String,
    cwd: PathBuf,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    messaging_socket_path: Option<PathBuf>,
    #[serde(default)]
    started_at: Option<i64>,
    #[serde(default)]
    updated_at: Option<i64>,
}

fn from_millis(ms: Option<i64>) -> Option<SystemTime> {
    let ms = ms?;
    u64::try_from(ms)
        .ok()
        .map(|ms| UNIX_EPOCH + Duration::from_millis(ms))
}

impl Session {
    /// Read one registry entry. `Ok(None)` means "not a session file" — the
    /// directory also holds `<pid>.<hash>.key` peer tokens.
    pub fn from_file(path: &Path) -> Result<Option<Session>> {
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            return Ok(None);
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let raw: Raw = serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;

        Ok(Some(Session {
            pid: raw.pid,
            name: raw.name.unwrap_or_else(|| {
                // Nameless sessions are rare but real; fall back to something
                // addressable rather than showing a blank row.
                format!("session-{}", &raw.session_id[..8.min(raw.session_id.len())])
            }),
            session_id: raw.session_id,
            cwd: raw.cwd,
            status: Status::parse(raw.status.as_deref()),
            kind: raw.kind.unwrap_or_default(),
            version: raw.version.unwrap_or_default(),
            socket: raw.messaging_socket_path,
            started_at: from_millis(raw.started_at),
            updated_at: from_millis(raw.updated_at),
        }))
    }

    /// Is the process actually still there? A file in the directory is not
    /// proof: a killed session never gets to clean up after itself.
    pub fn is_alive(&self) -> bool {
        if self.pid <= 0 {
            return false;
        }
        // Signal 0 performs the permission and existence checks without
        // sending anything. EPERM means it exists but belongs to someone else.
        if unsafe { libc::kill(self.pid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    pub fn is_interactive(&self) -> bool {
        self.kind == "interactive"
    }
}

/// Claude Code's project-directory name for a working directory: every
/// character that is not a letter, digit or dash becomes a dash.
///
/// `/Users/p/.claude-mem/obs` becomes `-Users-p--claude-mem-obs` — note the
/// doubled dash where the dot was. That collision is in Claude Code's scheme,
/// not ours, which is why a lookup checks the file is there before trusting
/// the result.
pub fn project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A session appeared, or one we already knew about changed.
    Upserted(Session),
    /// The file went away, or the process behind it did.
    Removed { pid: i32, name: String },
}

/// The live set of sessions, refreshed on demand.
pub struct Registry {
    dir: PathBuf,
    sessions: HashMap<i32, Session>,
}

impl Registry {
    pub fn new(dir: impl Into<PathBuf>) -> Registry {
        Registry {
            dir: dir.into(),
            sessions: HashMap::new(),
        }
    }

    /// Rescan the directory and report what moved since last time.
    ///
    /// Cheap enough to call on every UI tick: the directory holds one small
    /// file per running session, and a machine with fifty live sessions would
    /// be a surprise.
    pub fn refresh(&mut self) -> Vec<Change> {
        let mut changes = Vec::new();
        let mut seen: HashMap<i32, Session> = HashMap::new();

        // A missing directory is the ordinary state before the first `claude`
        // has ever run here, not an error worth surfacing — it simply means
        // there is nothing live, so anything we held becomes a removal below.
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let session = match Session::from_file(&entry.path()) {
                    Ok(Some(s)) => s,
                    // Not a session file, or caught mid-write. Either way the
                    // next refresh will see it properly.
                    Ok(None) | Err(_) => continue,
                };
                if !session.is_alive() {
                    continue;
                }
                seen.insert(session.pid, session);
            }
        }

        for (pid, session) in &seen {
            match self.sessions.get(pid) {
                Some(known) if known == session => {}
                _ => changes.push(Change::Upserted(session.clone())),
            }
        }
        for (pid, gone) in &self.sessions {
            if !seen.contains_key(pid) {
                changes.push(Change::Removed {
                    pid: *pid,
                    name: gone.name.clone(),
                });
            }
        }

        self.sessions = seen;
        changes
    }

    /// Live sessions, chief-of-staff ordering left to the caller.
    pub fn sessions(&self) -> impl Iterator<Item = &Session> {
        self.sessions.values()
    }

    pub fn interactive(&self) -> impl Iterator<Item = &Session> {
        self.sessions.values().filter(|s| s.is_interactive())
    }
}

/// Wakes the caller when the registry directory changes.
///
/// It deliberately carries no payload. Diffing lives in [`Registry::refresh`]
/// so there is exactly one code path that decides what changed, whether the
/// trigger was a filesystem event or a periodic tick.
pub struct Watcher {
    _inner: RecommendedWatcher,
    rx: Receiver<()>,
}

impl Watcher {
    pub fn new(dir: &Path) -> Result<Watcher> {
        Watcher::only(dir, |_| true)
    }

    /// Watch a directory, but wake only for files `keep` accepts.
    ///
    /// For the board, whose directory holds files fleet itself touches just
    /// by reading: SQLite readers write to `-shm`, so a watcher woken by it
    /// would refresh, read, touch `-shm`, and wake itself again for ever.
    pub fn only(dir: &Path, keep: impl Fn(&Path) -> bool + Send + 'static) -> Result<Watcher> {
        let (tx, rx) = channel();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                if !event.paths.iter().any(|p| keep(p)) {
                    return;
                }
                match event.kind {
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                        // A full mailbox means a refresh is already pending;
                        // dropping the extra wake-up is the right answer.
                        let _ = tx.send(());
                    }
                    _ => {}
                }
            }
        })?;
        watcher
            .watch(dir, RecursiveMode::NonRecursive)
            .with_context(|| format!("watching {}", dir.display()))?;
        Ok(Watcher {
            _inner: watcher,
            rx,
        })
    }

    /// Block until something moves, or `timeout` passes. Returns true if the
    /// wake-up came from the filesystem rather than the timeout — useful only
    /// for diagnostics; either way the caller should refresh.
    pub fn wait(&self, timeout: Duration) -> bool {
        match self.rx.recv_timeout(timeout) {
            Ok(()) => {
                // Drain anything that piled up behind it.
                while self.rx.try_recv().is_ok() {}
                true
            }
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_session(dir: &Path, pid: i32, name: &str, status: &str) {
        let body = format!(
            r#"{{"pid":{pid},"sessionId":"aaaaaaaa-bbbb-cccc-dddd-{pid:012}",
                 "cwd":"/tmp/repo","startedAt":1789824747123,"version":"2.1.278",
                 "messagingSocketPath":"/tmp/cc-socks/{pid}.sock","name":"{name}",
                 "kind":"interactive","status":"{status}","updatedAt":1789824867158}}"#
        );
        let mut f = fs::File::create(dir.join(format!("{pid}.json"))).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn slug_matches_claude_codes_scheme() {
        assert_eq!(
            project_slug(Path::new("/Users/me/Code/acme")),
            "-Users-me-Documents-Code-acme"
        );
        // The dot in a hidden directory becomes a second dash.
        assert_eq!(
            project_slug(Path::new("/Users/me/.claude-mem/observer-sessions")),
            "-Users-me--claude-mem-observer-sessions"
        );
        // Spaces and tildes go the same way.
        assert_eq!(
            project_slug(Path::new("/a/Mobile Documents/iCloud~md~obsidian")),
            "-a-Mobile-Documents-iCloud-md-obsidian"
        );
    }

    #[test]
    fn reads_a_session_and_ignores_key_files() {
        let dir = tempfile::tempdir().unwrap();
        write_session(dir.path(), std::process::id() as i32, "scratch", "busy");
        fs::write(dir.path().join("123.abc.key"), r#"{"peerToken":"x"}"#).unwrap();

        let mut reg = Registry::new(dir.path());
        let changes = reg.refresh();

        assert_eq!(changes.len(), 1, "the .key file must not produce a session");
        let s = reg.sessions().find(|s| s.name == "scratch").expect("session by name");
        assert_eq!(s.status, Status::Busy);
        assert!(s.is_interactive());
    }

    #[test]
    fn dead_processes_are_not_live_sessions() {
        let dir = tempfile::tempdir().unwrap();
        // pid 1 exists but is not ours; a very high pid almost certainly is not
        // running at all. Both paths through is_alive get exercised.
        write_session(dir.path(), 4_194_301, "ghost", "idle");

        let mut reg = Registry::new(dir.path());
        assert!(reg.refresh().is_empty(), "a stale file is not a session");
        assert_eq!(reg.sessions().count(), 0);
    }

    #[test]
    fn refresh_reports_only_what_moved() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id() as i32;
        write_session(dir.path(), me, "billing-svc", "idle");

        let mut reg = Registry::new(dir.path());
        assert_eq!(reg.refresh().len(), 1);
        assert!(reg.refresh().is_empty(), "an unchanged directory is quiet");

        write_session(dir.path(), me, "billing-svc", "busy");
        let changes = reg.refresh();
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            Change::Upserted(s) => assert_eq!(s.status, Status::Busy),
            other => panic!("expected an upsert, got {other:?}"),
        }

        fs::remove_file(dir.path().join(format!("{me}.json"))).unwrap();
        let changes = reg.refresh();
        assert_eq!(
            changes,
            vec![Change::Removed {
                pid: me,
                name: "billing-svc".into()
            }]
        );
    }

    #[test]
    fn a_half_written_file_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("999.json"), r#"{"pid":999,"sess"#).unwrap();

        let mut reg = Registry::new(dir.path());
        assert!(reg.refresh().is_empty());
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        let mut reg = Registry::new("/tmp/fleet-does-not-exist-9e3a1");
        assert!(reg.refresh().is_empty());
    }
}
