//! Where an agent's terminal lives: a herdr tab, or a pane beside the view.
//!
//! herdr draws the agent's pane; fleet only asks it to open one, type a
//! command into it, and knock with a message. Everything else — the board,
//! the briefs, the runs — does not care where the terminal is.
//!
//! A board row remembers the agent by its target, `herdr:<name>`. herdr's
//! name is the board's qualified with the fleet, `acme-chief`: herdr wants
//! live agent names unique across all its workspaces, and every fleet has a
//! chief.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::brief::{self, Brief};
use crate::herdr::{Herdr, Pane};

const HERDR_PREFIX: &str = "herdr:";

/// How much of the width the chief takes, on the left of the fleet view:
/// half. Without the rail, the view's graph and board fit in the other.
pub const CHIEF_SHARE: f32 = 0.5;

/// herdr, and the workspace new agents open in: the one fleet itself runs
/// in, so the crew sits together in one herdr sidebar entry.
#[derive(Debug, Clone)]
pub struct Host {
    pub herdr: Herdr,
    pub workspace: String,
    /// The fleet the agents belong to, which their herdr names carry.
    pub fleet: Option<String>,
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
    /// Where it is, said the way herdr says it.
    pub place: String,
    /// The terminal's shell. The agent's session descends from it, which is
    /// how the registry entry is matched to the pane.
    pub pid: i32,
    /// herdr's pane id, for naming what appears in it.
    pane: String,
    /// And the name to give it there.
    herdr_name: String,
}

impl Host {
    /// The same host, starting agents for the fleet whose board this is.
    pub fn in_fleet(mut self, board: Option<&Path>) -> Host {
        self.fleet = board.and_then(crate::scope::fleet_of);
        self
    }

    /// The herdr fleet is running in, or why it is not.
    pub fn detect() -> Result<Host> {
        Host::from_env().context("fleet runs inside herdr: start it from a herdr pane, or with the plugin's fleet.open")
    }

    /// The command line for `what`, with the brief read from files and the
    /// `/board` skill loaded: see `brief::launch_line`. An agent is still
    /// worth starting without the skill, if it cannot be written; its brief
    /// says how to use the board.
    pub fn line(&self, what: &What, stem: &str) -> Result<String> {
        let plugin = || brief::write_plugin(&brief::plugin_dir()).ok();
        Ok(match what {
            What::Command(c) => c.to_string(),
            What::Brief { brief, program } => {
                brief::launch_line(program, brief, &brief::default_dir(), stem, plugin().as_deref())?
            }
            What::Resume { brief, session, program } => {
                brief::resume_line(program, brief, session, &brief::default_dir(), stem, plugin().as_deref())?
            }
        })
    }

    /// Once the agent is up: herdr names only the agents it starts itself,
    /// so the one fleet started gets its name here, and a message addressed
    /// to it can find it.
    pub fn settle(&self, placed: &Placed, timeout: Duration) -> bool {
        self.herdr.name_when_up(&placed.pane, &placed.herdr_name, timeout).is_some()
    }

    /// The conversation running at `target`, as herdr knows it.
    pub fn session_at(&self, target: &str) -> Option<String> {
        self.herdr.agent(herdr_name(target)?)?.session_id().map(String::from)
    }

    /// Put a message in front of the agent at `target`. False when nobody
    /// is there.
    pub fn deliver(&self, target: &str, text: &str) -> Result<bool> {
        let Some(name) = herdr_name(target) else { return Ok(false) };
        if self.herdr.agent(name).is_none() {
            return Ok(false);
        }
        match self.herdr.agent_prompt(name, text) {
            Ok(()) => Ok(true),
            // At a question or permission dialog: herdr refused before
            // typing anything, and the message is on the board for when the
            // dialog is answered.
            Err(e) if e.code == "agent_blocked" => {
                bail!("{name} is waiting at a dialog; it is on the board")
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Bring the agent at `target` to the front.
    pub fn focus(&self, target: &str) -> Result<()> {
        let name = herdr_name(target).context("not a herdr agent")?;
        self.herdr.agent_focus(name)?;
        Ok(())
    }

    /// Close the terminal of the agent at `target`, which has been retired:
    /// its tab, or only its pane when the tab holds something else of the
    /// crew's. Says what it did, or why it left the terminal open.
    pub fn close(&self, target: &str) -> Result<String> {
        self.close_with(target, false)
    }

    /// [`Host::close`], working or not: for a handoff the user asked to
    /// make now, whatever the agent is in the middle of.
    pub fn close_now(&self, target: &str) -> Result<String> {
        self.close_with(target, true)
    }

    /// Whether the agent at `target` is mid-turn, as herdr sees it.
    pub fn is_working(&self, target: &str) -> bool {
        herdr_name(target)
            .and_then(|name| self.herdr.agent(name))
            .is_some_and(|a| a.agent_status == "working")
    }

    fn close_with(&self, target: &str, now: bool) -> Result<String> {
        let name = herdr_name(target).context("not a herdr agent")?;
        let Some(agent) = self.herdr.agent(name) else {
            return Ok("nothing was running in its tab".into());
        };
        let own = std::env::var("HERDR_PANE_ID").ok();
        let panes = self.herdr.panes(&agent.workspace_id).unwrap_or_default();
        let agents = self.herdr.agents().unwrap_or_default();
        let shared = agents.iter().any(|a| a.tab_id == agent.tab_id && a.pane_id != agent.pane_id)
            || panes.iter().any(|p| p.tab_id == agent.tab_id && p.label == crate::plugin::TAB);
        let status = if now { "idle" } else { agent.agent_status.as_str() };
        Ok(match closing(&agent.pane_id, status, own.as_deref(), shared) {
            Closing::Tab => {
                self.herdr.tab_close(&agent.tab_id)?;
                "closed its tab".into()
            }
            Closing::Pane => {
                self.herdr.pane_close(&agent.pane_id)?;
                "closed its pane; the rest of the tab is still in use".into()
            }
            Closing::Leave(why) => format!("its tab is left open: {why}"),
        })
    }
}

/// What to close of a retired agent's terminal.
#[derive(Debug, PartialEq)]
enum Closing {
    Tab,
    /// The tab holds another agent, or the fleet view: the chief's does.
    Pane,
    Leave(&'static str),
}

fn closing(pane: &str, status: &str, own_pane: Option<&str>, shared: bool) -> Closing {
    if own_pane == Some(pane) {
        // An agent retiring itself would end the command doing it.
        Closing::Leave("it is the session retiring it")
    } else if status == "working" {
        // Mid-turn, with whatever it is doing half done. Its conversation
        // would survive, its edit in progress might not.
        Closing::Leave("it is still working; close it when it stops")
    } else if shared {
        Closing::Pane
    } else {
        Closing::Tab
    }
}

impl Host {
    /// The herdr session and workspace this pane is in, when it is in one.
    pub fn from_env() -> Option<Host> {
        if !crate::herdr::inside() {
            return None;
        }
        let herdr = Herdr::from_env()?;
        let workspace = std::env::var("HERDR_WORKSPACE_ID").ok().filter(|w| !w.is_empty())?;
        Some(Host { herdr, workspace, fleet: None })
    }

    /// A tab of its own per agent, labelled with its name. A Claude Code
    /// session wants the width: split beside the others, each would be a
    /// column too narrow to read a diff in. The one exception is the chief,
    /// split on the fleet view's left: `beside_view`, since it is talked to
    /// while the board is watched.
    pub fn open(&self, name: &str, repo: &Path, command: &str, beside_view: bool) -> Result<Placed> {
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
        let herdr_name = herdr_agent_name(self.fleet.as_deref(), name);
        Ok(Placed {
            target: format!("{HERDR_PREFIX}{herdr_name}"),
            herdr_name,
            place: format!("tab {tab} ({pane})"),
            pid,
            pane,
        })
    }
}

impl Host {
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

/// What herdr calls an agent of `fleet`: the board's name, qualified so two
/// fleets' chiefs are two agents to herdr.
pub fn herdr_agent_name(fleet: Option<&str>, name: &str) -> String {
    match fleet {
        Some(f) => herdr_safe(&format!("{f}-{name}")),
        None => herdr_safe(name),
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
    fn a_retired_agent_takes_its_tab_with_it() {
        assert_eq!(closing("p2", "idle", Some("p1"), false), Closing::Tab);
        assert_eq!(closing("p2", "done", None, false), Closing::Tab);
        assert_eq!(closing("p2", "blocked", Some("p1"), false), Closing::Tab);
    }

    #[test]
    fn a_tab_the_crew_shares_loses_only_the_agents_pane() {
        assert_eq!(closing("p2", "idle", Some("p1"), true), Closing::Pane);
    }

    #[test]
    fn a_working_agent_and_the_one_retiring_are_left_open() {
        assert!(matches!(closing("p2", "working", Some("p1"), false), Closing::Leave(_)));
        assert!(matches!(closing("p1", "idle", Some("p1"), false), Closing::Leave(_)));
    }

    #[test]
    fn a_target_says_which_host_holds_it() {
        assert!(is_herdr("herdr:chief"));
        assert!(!is_herdr("fleet:chief"), "a tmux window, from before fleet ran only in herdr");
        assert_eq!(herdr_name("herdr:eng-2155"), Some("eng-2155"));
        assert_eq!(herdr_name("herdr:"), None);
    }

    #[test]
    fn herdr_names_an_agent_by_its_fleet_as_well() {
        assert_eq!(herdr_agent_name(Some("acme"), "chief"), "acme-chief");
        assert_eq!(herdr_agent_name(Some("storefront"), "chief"), "storefront-chief");
        assert_eq!(herdr_agent_name(None, "chief"), "chief");
    }

    #[test]
    fn names_are_made_into_ones_herdr_accepts() {
        assert_eq!(herdr_safe("billing-service"), "billing-service");
        assert_eq!(herdr_safe("ENG-2155"), "eng-2155");
        assert_eq!(herdr_safe("2fa.api"), "a2fa-api");
        assert_eq!(herdr_safe(&"x".repeat(40)).len(), 32);
    }
}
