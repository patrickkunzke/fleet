//! Where an agent's terminal lives: a tmux window, or a herdr tab.
//!
//! Fleet started out owning its terminals through tmux, and drew them itself
//! — which is where the mouse, the selection, pasting and scrolling all had
//! to be rebuilt by hand, and never felt native. Inside herdr none of that is
//! fleet's: herdr draws the agent's pane, and fleet only asks it to open one,
//! type a command into it, and knock with a message. Everything else — the
//! board, the briefs, the runs — does not care which of the two it is.
//!
//! A board row remembers which by its target: `herdr:<name>` for an agent
//! herdr holds, a tmux `session:window` otherwise.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::brief::{self, Brief};
use crate::herdr::{Herdr, Pane};
use crate::tmux::Tmux;

const HERDR_PREFIX: &str = "herdr:";

/// How much of the width the chief takes, on the left of the fleet view:
/// half. Without the rail, the view's graph and board fit in the other.
pub const CHIEF_SHARE: f32 = 0.5;

#[derive(Clone)]
pub enum Host {
    Tmux(Tmux),
    Herdr(Hosted),
}

/// herdr, and the workspace new agents open in: the one fleet itself runs
/// in, so the crew sits together in one herdr sidebar entry.
#[derive(Debug, Clone)]
pub struct Hosted {
    pub herdr: Herdr,
    pub workspace: String,
}

/// What to run in the new terminal.
pub enum What<'a> {
    /// A fresh agent, briefed.
    Brief { brief: &'a Brief, program: &'a str },
    /// An agent back into its own conversation.
    Resume { brief: &'a Brief, session: &'a str, program: &'a str },
    /// A given command line, unbriefed — the plumbing without an agent.
    Command(&'a str),
}

/// A terminal opened for an agent.
#[derive(Debug, Clone)]
pub struct Placed {
    /// What the board records, and what a message is delivered to.
    pub target: String,
    /// Where it is, said the way the host says it.
    pub place: String,
    /// The terminal's shell. The agent's session descends from it, which is
    /// how the registry entry is matched to the pane.
    pub pid: i32,
    /// herdr's pane id, for naming what appears in it.
    pane: Option<String>,
}

impl Host {
    /// herdr when fleet runs in a herdr pane, tmux otherwise.
    pub fn detect(tmux_session: Option<&str>) -> Result<Host> {
        if let Some(hosted) = Hosted::from_env() {
            return Ok(Host::Herdr(hosted));
        }
        Ok(Host::Tmux(Tmux::detect(tmux_session)?))
    }

    /// The host a recorded target belongs to — which is not always the one
    /// fleet is running under: an agent in herdr can message one fleet
    /// started earlier in tmux.
    pub fn for_target(target: &str) -> Option<Host> {
        if is_herdr(target) {
            Hosted::from_env().map(Host::Herdr)
        } else {
            Tmux::detect(None).ok().map(Host::Tmux)
        }
    }

    pub fn is_herdr(&self) -> bool {
        matches!(self, Host::Herdr(_))
    }

    /// The command line for `what`, in the form this host's terminal takes.
    pub fn line(&self, what: &What, stem: &str) -> Result<String> {
        Ok(match (self, what) {
            (_, What::Command(c)) => c.to_string(),
            (Host::Tmux(_), What::Brief { brief, .. }) => brief::command(brief),
            (Host::Tmux(_), What::Resume { brief, session, program }) => {
                brief::resume_command_for(program, brief, session)
            }
            (Host::Herdr(_), What::Brief { brief, program }) => {
                brief::launch_line(program, brief, &brief::default_dir(), stem)?
            }
            (Host::Herdr(_), What::Resume { brief, session, program }) => {
                brief::resume_line(program, brief, session, &brief::default_dir(), stem)?
            }
        })
    }

    /// Open a terminal named `name` in `repo`, and run `command` in it.
    /// `beside_view` puts it next to the fleet view rather than in a tab of
    /// its own, where the host can: the chief, who is talked to while the
    /// board is watched.
    pub fn open(&self, name: &str, repo: &Path, command: &str, beside_view: bool) -> Result<Placed> {
        match self {
            Host::Tmux(tmux) => {
                let pane = tmux.spawn(name, repo, command)?;
                Ok(Placed {
                    target: format!("{}:{}", pane.session, pane.window_name),
                    place: format!("pane {} in session {}", pane.id, pane.session),
                    pid: pane.pid,
                    pane: None,
                })
            }
            Host::Herdr(h) => h.open(name, repo, command, beside_view),
        }
    }

    /// Once the agent is up: herdr names only the agents it starts itself,
    /// so the one fleet started gets its name here, and a message addressed
    /// to it can find it. Nothing to do in tmux, whose window has the name.
    pub fn settle(&self, placed: &Placed, name: &str, timeout: Duration) -> bool {
        match (self, placed.pane.as_deref()) {
            (Host::Herdr(h), Some(pane)) => h.herdr.name_when_up(pane, name, timeout).is_some(),
            _ => true,
        }
    }

    /// The conversation running at `target`, as the host knows it. Only herdr
    /// does; in tmux it is found through the session registry instead.
    pub fn session_at(&self, target: &str) -> Option<String> {
        match self {
            Host::Herdr(h) => h.herdr.agent(herdr_name(target)?)?.session_id().map(String::from),
            Host::Tmux(_) => None,
        }
    }

    /// Whether anything is running at `target` now.
    pub fn is_live(&self, target: &str) -> bool {
        match self {
            Host::Tmux(tmux) => tmux.find(target).ok().flatten().is_some(),
            Host::Herdr(h) => herdr_name(target).is_some_and(|n| h.herdr.agent(n).is_some()),
        }
    }

    /// Put a message in front of the agent at `target`. False when nobody
    /// is there.
    pub fn deliver(&self, target: &str, text: &str) -> Result<bool> {
        match self {
            Host::Tmux(tmux) => crate::msg::deliver(tmux, target, text),
            Host::Herdr(h) => {
                let Some(name) = herdr_name(target) else { return Ok(false) };
                if h.herdr.agent(name).is_none() {
                    return Ok(false);
                }
                match h.herdr.agent_prompt(name, text) {
                    Ok(()) => Ok(true),
                    // At a question or permission dialog: herdr refused
                    // before typing anything, and the message is on the
                    // board for when the dialog is answered.
                    Err(e) if e.code == "agent_blocked" => {
                        bail!("{name} is waiting at a dialog; it is on the board")
                    }
                    Err(e) => Err(e.into()),
                }
            }
        }
    }

    /// Bring the agent at `target` to the front.
    pub fn focus(&self, target: &str) -> Result<()> {
        match self {
            Host::Herdr(h) => {
                let name = herdr_name(target).context("not a herdr agent")?;
                h.herdr.agent_focus(name)?;
                Ok(())
            }
            Host::Tmux(tmux) => {
                let pane = tmux.find(target)?.context("nothing running there")?;
                tmux.select(&pane)
            }
        }
    }
}

impl Hosted {
    /// The herdr session and workspace this pane is in, when it is in one.
    pub fn from_env() -> Option<Hosted> {
        if !crate::herdr::inside() {
            return None;
        }
        let herdr = Herdr::from_env()?;
        let workspace = std::env::var("HERDR_WORKSPACE_ID").ok().filter(|w| !w.is_empty())?;
        Some(Hosted { herdr, workspace })
    }

    /// A tab of its own per agent, labelled with its name. A Claude Code
    /// session wants the width: split beside the others, each would be a
    /// column too narrow to read a diff in. The one exception is the chief,
    /// split on the fleet view's left when it asks to be.
    fn open(&self, name: &str, repo: &Path, command: &str, beside_view: bool) -> Result<Placed> {
        let cwd = repo.to_string_lossy();
        // A terminal of the agent's own that is still there is used again:
        // after herdr restarts, every pane comes back with a shell where the
        // agent was, and a resume that opened a second one beside it would
        // leave the crew doubled.
        let cd_then = |c: &str| format!("cd {} && {c}", brief::quote(&cwd));
        let view = if beside_view { self.view_pane() } else { None };
        let (tab, pane, command) = match view {
            Some(view) => match self.vacant_pane(&view.tab_id, name) {
                Some(pane) => (view.tab_id, pane, cd_then(command)),
                None => {
                    // herdr splits only to the right, and a split's ratio is
                    // the share of the pane on the left: split, then trade
                    // places, and the chief is on the left at its share.
                    let pane = self.herdr.pane_split(&view.pane_id, &cwd, CHIEF_SHARE, true)?;
                    self.herdr.pane_swap(&pane, &view.pane_id)?;
                    // Labelled, so the shell herdr brings back here after a
                    // restart is found again.
                    self.herdr.pane_rename(&pane, name)?;
                    (view.tab_id, pane, command.to_string())
                }
            },
            None => match self.vacant_tab(name) {
                Some((tab, pane)) => (tab, pane, cd_then(command)),
                None => {
                    let created = self.herdr.tab_create(&self.workspace, &cwd, name, false)?;
                    (created.tab_id, created.pane_id, command.to_string())
                }
            },
        };
        let command = command.as_str();
        // A new pane is not a shell at its prompt for a moment, and a command
        // typed before it is lands in the shell's startup rather than at the
        // prompt.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.herdr.pane_idle(&pane) {
            if Instant::now() >= deadline {
                bail!("the new pane {pane} never came to a prompt");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let pid = self.herdr.shell_pid(&pane)?;
        self.herdr.pane_run(&pane, command)?;
        Ok(Placed {
            target: format!("{HERDR_PREFIX}{name}"),
            place: format!("tab {tab} ({pane})"),
            pid,
            pane: Some(pane),
        })
    }
}

impl Hosted {
    /// The pane the fleet view runs in, in this workspace.
    pub fn view_pane(&self) -> Option<Pane> {
        self.herdr.panes(&self.workspace).ok()?.into_iter().find(|p| p.label == crate::plugin::TAB)
    }

    /// A pane labelled `name` in `tab`, at its prompt with nothing running.
    fn vacant_pane(&self, tab: &str, name: &str) -> Option<String> {
        let taken: Vec<String> = self.herdr.agents().ok()?.into_iter().map(|a| a.pane_id).collect();
        self.herdr
            .panes(&self.workspace)
            .ok()?
            .into_iter()
            .find(|p| p.tab_id == tab && p.label == name && !taken.contains(&p.pane_id) && self.herdr.pane_idle(&p.pane_id))
            .map(|p| p.pane_id)
    }

    /// A tab labelled `name` in this workspace with a shell in it at its
    /// prompt, and nothing else running there.
    fn vacant_tab(&self, name: &str) -> Option<(String, String)> {
        let tab = self.herdr.tabs(&self.workspace).ok()?.into_iter().find(|t| t.label == name)?;
        let panes = self.herdr.panes(&self.workspace).ok()?;
        let in_tab: Vec<_> = panes.iter().filter(|p| p.tab_id == tab.tab_id).collect();
        // Another agent in the tab means it is somebody's, whatever it is
        // called.
        if self.herdr.agents().ok()?.iter().any(|a| a.tab_id == tab.tab_id) {
            return None;
        }
        // The tab's own pane before one another plugin labelled as its own
        // (herdr-sidebar's narrow file list), which comes back as a shell too.
        let pane = in_tab
            .iter()
            .filter(|p| p.label.is_empty() || p.label == name)
            .chain(in_tab.iter())
            .find(|p| self.herdr.pane_idle(&p.pane_id))?;
        Some((tab.tab_id, pane.pane_id.clone()))
    }
}

pub fn is_herdr(target: &str) -> bool {
    target.starts_with(HERDR_PREFIX)
}

fn herdr_name(target: &str) -> Option<&str> {
    target.strip_prefix(HERDR_PREFIX).filter(|n| !n.is_empty())
}

/// A name herdr will take: `[a-z][a-z0-9_-]{0,31}`. Fleet names are already
/// most of the way there — a repository's directory name — and are made the
/// rest of the way rather than refused.
pub fn herdr_safe(name: &str) -> String {
    let mut out: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    if !out.starts_with(|c: char| c.is_ascii_lowercase()) {
        out.insert(0, 'a');
    }
    out.truncate(32);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_says_which_host_holds_it() {
        assert!(is_herdr("herdr:chief"));
        assert!(!is_herdr("fleet:chief"));
        assert_eq!(herdr_name("herdr:eng-2155"), Some("eng-2155"));
        assert_eq!(herdr_name("herdr:"), None);
    }

    #[test]
    fn names_are_made_into_ones_herdr_accepts() {
        assert_eq!(herdr_safe("billing-service"), "billing-service");
        assert_eq!(herdr_safe("ENG-2155"), "eng-2155");
        assert_eq!(herdr_safe("2fa.api"), "a2fa-api");
        assert_eq!(herdr_safe(&"x".repeat(40)).len(), 32);
    }
}
