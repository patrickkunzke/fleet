//! Spawning and reaching agent sessions through tmux.
//!
//! tmux is the process manager, deliberately: a spawned agent runs a real,
//! unmodified `claude` REPL in a pane, and zooming to it hands over the actual
//! terminal rather than a re-rendering of it. That is what keeps "I did not
//! lose the terminal experience" true.
//!
//! Everything here addresses panes and windows by tmux *id* (`%12`, `@3`),
//! never by name or index. Names collide and indexes shift when a window is
//! closed; ids never do, which matters because these calls type into live
//! terminals.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// The tmux session the fleet spawns into, unless told otherwise.
pub const DEFAULT_SESSION: &str = "fleet";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    /// `%12` — stable for the life of the pane.
    pub id: String,
    /// `@3` — the window holding it.
    pub window: String,
    pub session: String,
    pub window_name: String,
    /// The process tmux started. For a pane running `claude`, the session
    /// itself is usually a child of this rather than this pid.
    pub pid: i32,
    pub cwd: PathBuf,
}

pub struct Tmux {
    bin: PathBuf,
    session: String,
}

impl Tmux {
    /// Locate tmux and settle on a session name.
    pub fn detect(session: Option<&str>) -> Result<Tmux> {
        let bin = which("tmux").context(
            "tmux is not on PATH — fleet spawns agents into tmux panes, so it is required",
        )?;
        Ok(Tmux {
            bin,
            session: session
                .map(str::to_string)
                // Running inside tmux already, spawn alongside rather than
                // starting a second session the user would have to attach to.
                .or_else(current_session)
                .unwrap_or_else(|| DEFAULT_SESSION.to_string()),
        })
    }

    #[allow(dead_code)]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Are we running inside tmux right now? Decides whether selecting a
    /// window can actually move the user, or only marks it for later.
    pub fn inside() -> bool {
        std::env::var_os("TMUX").is_some()
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.bin)
            .args(args)
            .output()
            .with_context(|| format!("running tmux {}", args.join(" ")))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("tmux {}: {}", args.join(" "), err.trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    pub fn has_session(&self) -> bool {
        self.run(&["has-session", "-t", &format!("={}", self.session)])
            .is_ok()
    }

    /// Create the session if it is not there. Detached: creating it must not
    /// yank the user out of whatever they are doing.
    pub fn ensure_session(&self) -> Result<()> {
        if self.has_session() {
            return Ok(());
        }
        self.run(&["new-session", "-d", "-s", &self.session])?;
        Ok(())
    }

    /// Open a new window running `command` in `cwd`, without switching to it.
    ///
    /// The caller decides when to move the user, so this never steals focus:
    /// a dispatch that jumped the terminal to a new agent every time would be
    /// unusable with more than two.
    pub fn spawn(&self, window_name: &str, cwd: &Path, command: &str) -> Result<Pane> {
        self.ensure_session()?;
        if !cwd.is_dir() {
            bail!("cannot spawn in '{}': not a directory", cwd.display());
        }
        let target = format!("={}", self.session);
        let cwd = cwd.to_string_lossy().to_string();
        let printed = self.run(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            &target,
            "-n",
            window_name,
            "-c",
            &cwd,
            command,
        ])?;
        let id = printed.trim().to_string();
        self.pane(&id)?
            .with_context(|| format!("tmux reported pane {id} but it is not listed"))
    }

    /// Every pane tmux knows about, across sessions.
    pub fn panes(&self) -> Result<Vec<Pane>> {
        let out = self.run(&[
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{window_id}\t#{session_name}\t#{window_name}\t#{pane_pid}\t#{pane_current_path}",
        ])?;
        Ok(out.lines().filter_map(parse_pane).collect())
    }

    pub fn pane(&self, id: &str) -> Result<Option<Pane>> {
        Ok(self.panes()?.into_iter().find(|p| p.id == id))
    }

    // Reached from the TUI's keybindings, which are not written yet.
    /// Bring a pane to the front. Only moves the user when fleet is itself
    /// running inside tmux; otherwise it marks the window as current so that
    /// attaching later lands there.
    #[allow(dead_code)]
    pub fn select(&self, pane: &Pane) -> Result<()> {
        self.run(&["select-window", "-t", &pane.window])?;
        self.run(&["select-pane", "-t", &pane.id])?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn zoom(&self, pane: &Pane) -> Result<()> {
        self.select(pane)?;
        self.run(&["resize-pane", "-Z", "-t", &pane.id])?;
        Ok(())
    }

    /// Type a line into a pane and press Enter.
    ///
    /// The text goes with `-l` so tmux takes it literally: without it a
    /// message containing "Enter", "C-c" or a semicolon would be read as key
    /// names and sent as keystrokes into a live session.
    #[allow(dead_code)]
    pub fn send_line(&self, pane: &Pane, text: &str) -> Result<()> {
        if text.contains('\n') {
            bail!("send_line takes one line; a newline would submit it early");
        }
        self.run(&["send-keys", "-t", &pane.id, "-l", "--", text])?;
        self.run(&["send-keys", "-t", &pane.id, "Enter"])?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn kill(&self, pane: &Pane) -> Result<()> {
        self.run(&["kill-pane", "-t", &pane.id])?;
        Ok(())
    }
}

fn parse_pane(line: &str) -> Option<Pane> {
    let mut f = line.split('\t');
    let pane = Pane {
        id: f.next()?.to_string(),
        window: f.next()?.to_string(),
        session: f.next()?.to_string(),
        window_name: f.next()?.to_string(),
        pid: f.next()?.parse().ok()?,
        cwd: PathBuf::from(f.next()?),
    };
    Some(pane)
}

fn current_session() -> Option<String> {
    if !Tmux::inside() {
        return None;
    }
    let out = Command::new("tmux")
        .args(["display-message", "-p", "#{session_name}"])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() { None } else { Some(name) }
}

fn which(bin: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("'{bin}' not found on PATH")
}

/// Which live session belongs to a pane we spawned.
///
/// tmux runs a window's command through a shell, so the pane's own pid is
/// that shell and `claude` is a child of it. Matching on the working
/// directory instead would pick the wrong agent as soon as two of them share
/// a repo, so this walks the process tree.
pub fn owns(pane_pid: i32, session_pid: i32) -> bool {
    let parents = parent_map();
    let mut pid = session_pid;
    // A tmux pane is a handful of processes deep at most; the bound is
    // there so a cycle in a malformed ps table cannot hang the UI.
    for _ in 0..32 {
        if pid == pane_pid {
            return true;
        }
        match parents.get(&pid) {
            Some(&parent) if parent > 1 => pid = parent,
            _ => return false,
        }
    }
    false
}

fn parent_map() -> HashMap<i32, i32> {
    let mut map = HashMap::new();
    let Ok(out) = Command::new("ps").args(["-ax", "-o", "pid=,ppid="]).output() else {
        return map;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut f = line.split_whitespace();
        if let (Some(pid), Some(ppid)) = (f.next(), f.next())
            && let (Ok(pid), Ok(ppid)) = (pid.parse(), ppid.parse())
        {
            map.insert(pid, ppid);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway tmux session that always tears itself down.
    struct Scratch {
        tmux: Tmux,
    }

    impl Scratch {
        fn new(tag: &str) -> Option<Scratch> {
            let name = format!("fleet-test-{tag}-{}", std::process::id());
            let tmux = Tmux::detect(Some(&name)).ok()?;
            tmux.ensure_session().ok()?;
            Some(Scratch { tmux })
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = self
                .tmux
                .run(&["kill-session", "-t", &format!("={}", self.tmux.session)]);
        }
    }

    #[test]
    fn spawns_a_window_without_switching_to_it() {
        let Some(s) = Scratch::new("spawn") else {
            return; // tmux is not installed here
        };
        let dir = tempfile::tempdir().unwrap();

        let pane = s
            .tmux
            .spawn("billing-svc", dir.path(), "sleep 30")
            .unwrap();

        assert!(pane.id.starts_with('%'), "a pane id, not a name: {}", pane.id);
        assert!(pane.window.starts_with('@'));
        assert_eq!(pane.window_name, "billing-svc");
        assert_eq!(pane.session, s.tmux.session);
        assert!(pane.pid > 0);
        // macOS hands out /private/var for /var, so compare what tmux resolved.
        assert!(
            pane.cwd.ends_with(dir.path().file_name().unwrap()),
            "spawned in {:?}, expected under {:?}",
            pane.cwd,
            dir.path()
        );

        assert!(s.tmux.panes().unwrap().iter().any(|p| p.id == pane.id));
    }

    #[test]
    fn a_spawned_pane_owns_the_process_it_started() {
        let Some(s) = Scratch::new("owns") else { return };
        let dir = tempfile::tempdir().unwrap();

        let pane = s.tmux.spawn("worker", dir.path(), "sleep 30").unwrap();

        assert!(owns(pane.pid, pane.pid), "a pane owns its own process");
        assert!(
            !owns(pane.pid, 1),
            "and does not own init, which is nobody's child"
        );
    }

    #[test]
    fn refuses_to_spawn_somewhere_that_is_not_a_directory() {
        let Some(s) = Scratch::new("nodir") else { return };
        let err = s
            .tmux
            .spawn("nowhere", Path::new("/definitely/not/here"), "sleep 1")
            .unwrap_err();
        assert!(err.to_string().contains("not a directory"), "{err}");
    }

    #[test]
    fn sends_a_line_literally_rather_than_as_key_names() {
        let Some(s) = Scratch::new("send") else { return };
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("typed.txt");

        // `cat` writes back whatever is typed at it, which is as close to a
        // REPL as a test can get without running one.
        let pane = s
            .tmux
            .spawn(
                "typist",
                dir.path(),
                &format!("cat > {}", out.to_string_lossy()),
            )
            .unwrap();

        // Every one of these would be a keystroke, not text, without -l.
        let tricky = "status; Enter C-c \"quoted\" $HOME";
        s.tmux.send_line(&pane, tricky).unwrap();

        // Give cat a moment, then close its stdin so the file is flushed.
        std::thread::sleep(std::time::Duration::from_millis(300));
        s.tmux.run(&["send-keys", "-t", &pane.id, "C-d"]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));

        let typed = std::fs::read_to_string(&out).unwrap_or_default();
        assert_eq!(
            typed.trim_end(),
            tricky,
            "the line must arrive as written, with no shell or key expansion"
        );
    }

    #[test]
    fn a_multiline_send_is_refused_rather_than_submitted_early() {
        let Some(s) = Scratch::new("multi") else { return };
        let dir = tempfile::tempdir().unwrap();
        let pane = s.tmux.spawn("worker", dir.path(), "sleep 30").unwrap();

        let err = s.tmux.send_line(&pane, "first\nsecond").unwrap_err();
        assert!(err.to_string().contains("one line"), "{err}");
    }

    #[test]
    fn killing_a_pane_removes_it() {
        let Some(s) = Scratch::new("kill") else { return };
        let dir = tempfile::tempdir().unwrap();
        let pane = s.tmux.spawn("doomed", dir.path(), "sleep 30").unwrap();

        s.tmux.kill(&pane).unwrap();
        assert!(s.tmux.pane(&pane.id).unwrap().is_none());
    }

    #[test]
    fn selecting_and_zooming_a_live_pane_succeed() {
        let Some(s) = Scratch::new("zoom") else { return };
        let dir = tempfile::tempdir().unwrap();
        let pane = s.tmux.spawn("focus", dir.path(), "sleep 30").unwrap();

        s.tmux.select(&pane).unwrap();
        s.tmux.zoom(&pane).unwrap();
    }

    #[test]
    fn a_pane_line_parses_into_its_parts() {
        let p = parse_pane("%12\t@3\tfleet\tbilling-svc\t4242\t/repo/content").unwrap();
        assert_eq!(p.id, "%12");
        assert_eq!(p.window, "@3");
        assert_eq!(p.session, "fleet");
        assert_eq!(p.window_name, "billing-svc");
        assert_eq!(p.pid, 4242);
        assert_eq!(p.cwd, PathBuf::from("/repo/content"));

        assert!(parse_pane("nonsense").is_none());
    }
}
