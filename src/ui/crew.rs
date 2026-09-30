//! The crew: the agents this fleet created, and what each is doing.
//!
//! Only agents the fleet started. Not repositories, and not every Claude
//! Code session on the machine: this is the crew working on the thing in
//! front of you, and anything else in the graph is noise competing with it.

use std::path::Path;

use crate::db;
use crate::registry::{self, Status};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Live and mid-turn.
    Working,
    /// Live and waiting for you.
    Waiting,
    /// On the board, but its process is not there any more.
    Gone,
    /// On the board and never linked to a session — registered by hand, or
    /// spawned and never adopted. Not the same as having died, and saying so
    /// matters: one needs restarting, the other needs finding.
    Unlinked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Chief,
    Worker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub role: Role,
    pub repo: String,
    pub presence: Presence,
    /// The second line: the task it holds, or why there is none.
    pub detail: String,
    pub bg_running: i64,
    /// How long the session has been up, already formatted.
    pub uptime: Option<String>,
    pub session_id: Option<String>,
    pub branch: Option<String>,
    pub target: Option<String>,
    pub pid: Option<i32>,
    /// Stopped at a question or permission dialog, as herdr sees it: waiting
    /// on you in the strongest sense, since nothing moves until it is
    /// answered. Only herdr can tell; the registry cannot.
    pub asking: bool,
}

/// Join what the board knows to what is actually running.
///
/// The board says which agents exist and what they hold; the registry says
/// which are alive right now. Neither alone is the crew: an agent whose
/// process died still has a task assigned, and a session nobody registered is
/// still doing work.
pub fn merge(agents: &[db::Agent], sessions: &[registry::Session]) -> Vec<Row> {
    let mut rows = Vec::new();

    for a in agents {
        let live = a
            .session_id
            .as_deref()
            .and_then(|id| sessions.iter().find(|s| s.session_id == id));
        let presence = match (live.map(|s| &s.status), a.session_id.is_some()) {
            (Some(Status::Busy), _) => Presence::Working,
            (Some(_), _) => Presence::Waiting,
            // It had a session id and the process is gone, versus it never
            // had one at all.
            (None, true) => Presence::Gone,
            (None, false) => Presence::Unlinked,
        };
        let detail = match (&a.task_key, &a.task_title) {
            (Some(key), Some(title)) => format!("{key} · {title}"),
            (Some(key), None) => key.clone(),
            _ => match presence {
                Presence::Gone => "session ended".to_string(),
                Presence::Unlinked => "no session yet".to_string(),
                _ => "no task".to_string(),
            },
        };
        rows.push(Row {
            name: a.name.clone(),
            role: if a.role == "chief" {
                Role::Chief
            } else {
                Role::Worker
            },
            repo: a
                .repo
                .as_deref()
                .map(short_repo)
                .unwrap_or_default()
                .to_string(),
            presence,
            detail,
            bg_running: a.bg_running,
            uptime: live.and_then(|s| s.started_at).and_then(since),
            session_id: a.session_id.clone(),
            branch: a.branch.clone(),
            target: a.target.clone(),
            pid: live.map(|s| s.pid),
            asking: false,
        });
    }

    // The chief first — it is the one you brief.
    rows.sort_by(|a, b| {
        let rank = |r: &Row| if r.role == Role::Chief { 0 } else { 1 };
        rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name))
    });

    rows
}

/// "14m", "4h12m" — the same shape the background list uses.
///
/// A clock that disagrees with the registry, or a timestamp that never made
/// sense, produces a number wide enough to push the agent's name off its
/// card. Past a fortnight it is not an uptime, so it is not shown.
fn since(started: std::time::SystemTime) -> Option<String> {
    let secs = started.elapsed().ok()?.as_secs();
    if secs > 14 * 24 * 3600 {
        return None;
    }
    Some(match secs {
        s if s >= 3600 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    })
}

fn short_repo(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn agent(name: &str, role: &str, session: Option<&str>, task: Option<&str>) -> db::Agent {
        db::Agent {
            name: name.into(),
            role: role.into(),
            repo: Some(format!("/repo/{name}")),
            session_id: session.map(str::to_string),
            target: None,
            branch: None,
            task_key: task.map(str::to_string),
            task_title: task.map(|_| "consume the param".to_string()),
            bg_running: 0,
        }
    }

    fn session(name: &str, id: &str, cwd: &str, status: Status) -> registry::Session {
        registry::Session {
            pid: 4242,
            session_id: id.into(),
            cwd: PathBuf::from(cwd),
            name: name.into(),
            status,
            kind: "interactive".into(),
            version: "2.1.278".into(),
            socket: None,
            started_at: Some(SystemTime::UNIX_EPOCH),
            updated_at: None,
        }
    }

    #[test]
    fn the_chief_comes_first_then_workers_by_name() {
        let agents = [
            agent("storefront", "worker", None, None),
            agent("chief", "chief", None, None),
            agent("billing-svc", "worker", None, None),
        ];
        let names: Vec<_> = merge(&agents, &[])
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, vec!["chief", "billing-svc", "storefront"]);
    }

    #[test]
    fn presence_comes_from_the_registry_not_the_board() {
        let agents = [agent("billing-svc", "worker", Some("sess-1"), Some("ENG-1"))];
        let live = [session("billing-service-50", "sess-1", "/repo/content", Status::Busy)];

        let rows = merge(&agents, &live);
        assert_eq!(rows[0].presence, Presence::Working);
        assert_eq!(rows[0].pid, Some(4242));
        assert_eq!(rows[0].detail, "ENG-1 · consume the param");
    }

    #[test]
    fn never_linked_is_not_the_same_as_died() {
        // Registered by hand, never spawned: nothing has been lost.
        let rows = merge(&[agent("chief", "chief", None, None)], &[]);
        assert_eq!(rows[0].presence, Presence::Unlinked);
        assert_eq!(rows[0].detail, "no session yet");

        // Spawned, adopted, and then the process went away: something has.
        let rows = merge(&[agent("billing-svc", "worker", Some("sess-1"), None)], &[]);
        assert_eq!(rows[0].presence, Presence::Gone);
        assert_eq!(rows[0].detail, "session ended");
    }

    #[test]
    fn an_agent_whose_process_died_is_shown_as_gone_not_dropped() {
        let agents = [agent("billing-svc", "worker", Some("sess-1"), None)];
        let rows = merge(&agents, &[]);

        assert_eq!(rows.len(), 1, "it still holds a row on the board");
        assert_eq!(rows[0].presence, Presence::Gone);
        assert_eq!(rows[0].detail, "session ended");
    }

    #[test]
    fn a_session_the_fleet_did_not_start_is_not_in_the_rail() {
        let agents = [agent("billing-svc", "worker", Some("sess-1"), None)];
        let live = [
            session("billing-service-50", "sess-1", "/repo/content", Status::Idle),
            // Someone's own terminal, in the same workspace. Not our crew.
            session("scratch", "sess-2", "/repo/workspace", Status::Idle),
        ];

        let names: Vec<_> = merge(&agents, &live).into_iter().map(|r| r.name).collect();
        assert_eq!(names, vec!["billing-svc"]);
    }
}
