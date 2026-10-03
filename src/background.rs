//! Agents as Claude Code background sessions, for a fleet run outside herdr.
//!
//! `claude --bg` starts a session that a supervisor keeps running with no
//! terminal open; `claude agents` lists them, `claude attach` opens one, and
//! `claude stop` ends it. fleet starts each agent that way, in its
//! repository, with the same brief, plugin and tools it would type into a
//! herdr tab.
//!
//! A board row remembers such an agent by its target, `bg:<id>`, where the
//! id is the short one `claude --bg` prints. Claude Code picks it: a
//! `--session-id` given with `--bg` is ignored. Messages reach a background
//! session only through fleet's mod: there is no pane to type them into.
//!
//! `claude agents --json` is the documented way to read their state. The
//! files under `~/.claude/jobs` are not, and are not read here.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::brief;
use crate::host::{Placed, What};

pub const PREFIX: &str = "bg:";

/// The `claude` that starts and lists background sessions.
#[derive(Debug, Clone)]
pub struct Background {
    program: String,
}

/// A session as `claude agents --json` lists it.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Listed {
    /// The short id, for background sessions only.
    pub id: Option<String>,
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
    pub name: Option<String>,
    pub kind: Option<String>,
    /// `busy`, `waiting` or `idle`, while it is alive.
    pub status: Option<String>,
    /// `working`, `blocked`, `done`, `failed` or `stopped`.
    pub state: Option<String>,
    /// What it is waiting for: `permission prompt`, `input needed`, ...
    #[serde(rename = "waitingFor")]
    pub waiting_for: Option<String>,
}

impl Background {
    pub fn new(program: impl Into<String>) -> Background {
        Background { program: program.into() }
    }

    /// With the `claude` fleet starts its agents with.
    pub fn detect() -> Background {
        Background::new(brief::claude_program())
    }

    /// Whether this Claude Code runs background sessions at all.
    pub fn available(&self) -> bool {
        self.list().is_ok()
    }

    /// Every session `claude agents` knows, background and interactive.
    pub fn list(&self) -> Result<Vec<Listed>> {
        let out = Command::new(&self.program)
            .args(["agents", "--json", "--all"])
            .output()
            .with_context(|| format!("running {} agents", self.program))?;
        if !out.status.success() {
            bail!("{} agents: {}", self.program, String::from_utf8_lossy(&out.stderr).trim());
        }
        parse_list(&String::from_utf8_lossy(&out.stdout))
    }

    /// The background session at `target`, as `claude agents` lists it.
    pub fn find(&self, target: &str) -> Option<Listed> {
        let id = short_id(target)?;
        self.list().ok()?.into_iter().find(|l| l.id.as_deref() == Some(id))
    }

    /// Start `what` as a background session in `repo`, under `name`, with
    /// `env` set: the run, the board and fleet on PATH, which a background
    /// session takes from the process that starts it.
    pub fn open(&self, name: &str, repo: &Path, what: &What, env: &[(String, String)]) -> Result<Placed> {
        let plugin = brief::write_plugin(&brief::plugin_dir()).ok();
        let mut args = vec!["--bg".to_string(), "--name".to_string(), name.to_string()];
        match what {
            What::Brief { brief, .. } => args.extend(brief::launch_args(brief, plugin.as_deref())),
            What::Resume { brief, session, .. } => {
                // A session that ran in the background before wakes with the
                // options it was started with. Given them again, Claude Code
                // starts a copy under a new id instead.
                if self.list()?.iter().any(|l| l.session_id.as_deref() == Some(session) && l.kind.as_deref() == Some("background")) {
                    args = vec!["--resume".into(), session.to_string(), "--bg".into()];
                } else {
                    args.extend(brief::resume_args(brief, session, plugin.as_deref()));
                }
            }
            What::Command(_) => bail!("a background session runs Claude Code, not a given command"),
        }
        let out = Command::new(&self.program)
            .args(&args)
            .current_dir(repo)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .output()
            .with_context(|| format!("running {} --bg", self.program))?;
        let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        let id = started(&said)?;
        Ok(Placed::background(format!("{PREFIX}{id}"), format!("background session {id}")))
    }

    /// Wait until the session at `target` is listed with its conversation.
    pub fn settle(&self, target: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.find(target).is_some_and(|l| l.session_id.is_some()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        false
    }

    /// Whether it is mid-turn.
    pub fn is_working(&self, target: &str) -> bool {
        self.find(target).is_some_and(|l| l.status.as_deref() == Some("busy"))
    }

    /// End the session at `target`. Its conversation stays on disk, so the
    /// run can still bring it back.
    pub fn stop(&self, target: &str) -> Result<String> {
        let id = short_id(target).context("not a background session")?;
        let out = Command::new(&self.program).args(["stop", id]).output()?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok("stopped its background session".into())
    }

    /// How to open the session at `target`: there is no tab to switch to.
    pub fn attach_hint(target: &str) -> String {
        match short_id(target) {
            Some(id) => format!("`claude attach {id}` opens it, or find it in `claude agents`"),
            None => "find it in `claude agents`".into(),
        }
    }
}

pub fn is_background(target: &str) -> bool {
    target.starts_with(PREFIX)
}

fn short_id(target: &str) -> Option<&str> {
    target.strip_prefix(PREFIX).filter(|id| !id.is_empty())
}

fn parse_list(json: &str) -> Result<Vec<Listed>> {
    serde_json::from_str(json).context("reading claude agents --json")
}

/// The id `claude --bg` started, from what it printed: `backgrounded ·
/// 5cba7f8f · billing`. It exits 0 when it refuses, too, so the words are
/// what say whether it started.
fn started(said: &str) -> Result<String> {
    if let Some(id) = said
        .lines()
        .find_map(|l| l.trim().strip_prefix("backgrounded · "))
        .and_then(|rest| rest.split(" · ").next())
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        return Ok(id.to_string());
    }
    if said.contains("Workspace not trusted") {
        bail!(
            "Claude Code has not been trusted in this repository, or a folder above it. \
             Run `claude` in the workspace once and accept the trust prompt: that covers every repository in it"
        );
    }
    bail!("claude --bg did not start a session: {}", said.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_is_read_from_what_claude_bg_printed() {
        let said = "backgrounded · 5cba7f8f · billing\n  claude agents             list sessions\n  claude attach 5cba7f8f    open in this terminal\n";
        assert_eq!(started(said).unwrap(), "5cba7f8f");
        let woke = "note: woke session 5cba7f8f with its saved options (--name, --plugin-dir).\nbackgrounded · 5cba7f8f · billing\n";
        assert_eq!(started(woke).unwrap(), "5cba7f8f", "a wake says so first");
    }

    #[test]
    fn an_untrusted_repository_is_said_plainly_though_claude_exits_0() {
        let err = started("Workspace not trusted. Run `claude` in /w/x once and accept the trust prompt, then retry.\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("trust prompt"), "{err}");
        assert!(started("something else\n").is_err());
    }

    #[test]
    fn the_listing_reads_status_and_what_a_session_waits_for() {
        let json = r#"[
          {"pid":48807,"id":"5cba7f8f","cwd":"/w","kind":"background","startedAt":1,"sessionId":"5cba7f8f-44ed","name":"billing","status":"waiting","waitingFor":"permission prompt","state":"blocked"},
          {"pid":1,"cwd":"/w","kind":"interactive","startedAt":1,"sessionId":"x","name":"me","status":"busy"}
        ]"#;
        let listed = parse_list(json).unwrap();
        assert_eq!(listed[0].id.as_deref(), Some("5cba7f8f"));
        assert_eq!(listed[0].waiting_for.as_deref(), Some("permission prompt"));
        assert_eq!(listed[1].id, None, "an interactive session has no short id");
    }

    #[test]
    fn a_target_says_which_host_it_belongs_to() {
        assert!(is_background("bg:5cba7f8f"));
        assert!(!is_background("herdr:billing"));
        assert_eq!(short_id("bg:"), None);
        assert!(Background::attach_hint("bg:5cba7f8f").contains("claude attach 5cba7f8f"));
    }
}
