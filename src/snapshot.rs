//! What the chief's own session shows of the fleet, when there is no fleet
//! tab beside it: one JSON that fleet's mod reads every few seconds and
//! draws as a pane, a band above the prompt and toasts.
//!
//! The same sources the fleet view joins: the board, the session registry
//! for who is alive and busy, and, for an agent stopped at a dialog, what
//! `claude agents` says it waits for. herdr's own view of its agents is not
//! read: in herdr, the fleet tab is there for that.

use serde::Serialize;

use crate::background::{Background, Listed};
use crate::db::{self, Db};
use crate::registry::{self, Registry};
use crate::ui::{crew, hosting};

#[derive(Debug, Serialize)]
pub struct Snapshot {
    /// The agent this session is, when it is one of the fleet's.
    pub me: Option<Me>,
    pub agents: Vec<Agent>,
    pub tasks: Vec<Task>,
    /// The newest first: what the pane lists, and what the mod toasts once.
    pub events: Vec<Event>,
}

#[derive(Debug, Serialize)]
pub struct Me {
    pub name: String,
    pub role: String,
}

#[derive(Debug, Serialize)]
pub struct Agent {
    pub name: String,
    pub role: String,
    /// `working`, `waiting`, `gone` or `unlinked`.
    pub presence: &'static str,
    /// What it is stopped at, when it is: `permission prompt`, ...
    pub waiting_for: Option<String>,
    pub tool: Option<String>,
    pub context: Option<i64>,
    pub task: Option<String>,
    pub awaiting_go: Option<String>,
    /// Where it runs, so the pane can say how to open it.
    pub target: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Task {
    pub key: String,
    pub title: String,
    pub state: &'static str,
    pub agent: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Event {
    /// Stable across reads, for telling a new event from one already shown.
    pub key: String,
    pub kind: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub task: Option<String>,
    pub summary: String,
    /// The notification it is worth, if any: a task blocked, in review or
    /// done.
    pub notice: Option<Notice>,
}

#[derive(Debug, Serialize)]
pub struct Notice {
    pub title: String,
    pub body: String,
}

/// The fleet as `session` sees it.
pub fn take(db: &Db, session: Option<&str>) -> anyhow::Result<Snapshot> {
    let agents = db.agents()?;
    let mut reg = Registry::new(registry::default_dir());
    reg.refresh();
    let sessions: Vec<_> = reg.sessions().cloned().collect();
    let rows = crew::merge(&agents, &sessions);
    // Only asked when someone could be stopped at a dialog: it starts a
    // process, and most reads find everyone working or idle.
    let listed: Vec<Listed> = if rows.iter().any(|r| r.presence == crew::Presence::Waiting) {
        Background::detect().list().unwrap_or_default()
    } else {
        Vec::new()
    };

    let me = session.and_then(|sid| {
        agents
            .iter()
            .find(|a| a.session_id.as_deref() == Some(sid))
            .map(|a| Me { name: a.name.clone(), role: a.role.clone() })
    });

    let agents = rows
        .iter()
        .map(|r| {
            let board = agents.iter().find(|a| a.name == r.name);
            let waiting_for = r
                .session_id
                .as_deref()
                .and_then(|sid| listed.iter().find(|l| l.session_id.as_deref() == Some(sid)))
                .filter(|l| l.status.as_deref() == Some("waiting"))
                .and_then(|l| l.waiting_for.clone());
            Agent {
                name: r.name.clone(),
                role: if r.role == crew::Role::Chief { "chief".into() } else { "worker".into() },
                presence: match r.presence {
                    crew::Presence::Working => "working",
                    crew::Presence::Waiting => "waiting",
                    crew::Presence::Gone => "gone",
                    crew::Presence::Unlinked => "unlinked",
                },
                waiting_for,
                tool: r.tool.clone(),
                context: r.context,
                task: board.and_then(|a| a.task_key.clone()),
                awaiting_go: r.awaiting_go.clone(),
                target: r.target.clone(),
            }
        })
        .collect();

    let tasks = db
        .board()?
        .into_iter()
        .filter(|t| !matches!(t.state, db::State::Done | db::State::Dropped))
        .map(|t| Task {
            note: t.blocked_on.clone().or_else(|| {
                (!t.waiting_on.is_empty()).then(|| format!("waits on {}", t.waiting_on.join(", ")))
            }),
            key: t.key,
            title: t.title,
            state: t.state.as_str(),
            agent: t.agent,
        })
        .collect();

    let events = db
        .events(30)?
        .into_iter()
        .map(|e| Event {
            key: crate::ui::event_key(&e),
            notice: hosting::notice(&e).map(|(title, body)| Notice { title, body }),
            kind: e.kind,
            from: e.from_agent,
            to: e.to_agent,
            task: e.task_key,
            summary: e.summary,
        })
        .collect();

    Ok(Snapshot { me, agents, tasks, events })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_told_who_it_is_and_what_the_crew_holds() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_agent("chief", Some("chief"), None, Some("sid-chief"), None, None).unwrap();
        db.upsert_agent("billing", None, Some("/w/billing"), Some("sid-1"), Some("bg:5cba7f8f"), None).unwrap();
        db.add_task(&db::NewTask { key: "ENG-1-1", title: "write notes", repo: "/w/billing", ..Default::default() })
            .unwrap();
        db.claim("ENG-1-1", "billing").unwrap();
        db.transition("ENG-1-1", db::State::Blocked, Some("needs the flag")).unwrap();

        let snap = take(&db, Some("sid-chief")).unwrap();
        let me = snap.me.unwrap();
        assert_eq!((me.name.as_str(), me.role.as_str()), ("chief", "chief"));
        let billing = snap.agents.iter().find(|a| a.name == "billing").unwrap();
        assert_eq!(billing.target.as_deref(), Some("bg:5cba7f8f"));
        assert_eq!(snap.tasks[0].note.as_deref(), Some("needs the flag"));
        let blocked = snap.events.iter().find(|e| e.summary.starts_with("blocked")).unwrap();
        assert_eq!(blocked.notice.as_ref().unwrap().title, "fleet · ENG-1-1 is blocked");
        assert!(take(&db, Some("sid-x")).unwrap().me.is_none(), "a session the board does not know is nobody");
    }
}
