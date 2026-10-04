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

/// One run of cells in one style: what the mod draws as one `Text`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Span {
    pub t: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dim: bool,
}

/// The graph the fleet view draws, at `width` by at most `height` cells, as
/// lines of styled spans: the same picture, from the same code, for a pane
/// that draws text rather than a terminal frame. Blank lines at the bottom
/// are left off, so the pane can put more beneath it.
pub fn graph(db: &Db, width: u16, height: u16, run: Option<i64>) -> anyhow::Result<Vec<Vec<Span>>> {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Modifier;

    let agents = db.agents()?;
    let mut reg = Registry::new(registry::default_dir());
    reg.refresh();
    let sessions: Vec<_> = reg.sessions().cloned().collect();
    let rows = crew::merge(&agents, &sessions);
    let events = match run {
        Some(id) => db.run_events(id, 200)?,
        None => db.events(200)?,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let ages: Vec<f64> = events
        .iter()
        .map(|e| (now - crate::ui::graph::epoch(&e.ts).unwrap_or(0.0)).max(0.0))
        .collect();

    let area = Rect::new(0, 0, width.max(20), height.max(4));
    let mut buf = Buffer::empty(area);
    crate::ui::graph::render(&mut buf, area, &rows, &events, &ages, None);

    let mut lines: Vec<Vec<Span>> = Vec::new();
    for y in 0..area.height {
        let mut line: Vec<Span> = Vec::new();
        for x in 0..area.width {
            let cell = &buf[(x, y)];
            let fg = colour(cell.fg);
            let bold = cell.modifier.contains(Modifier::BOLD);
            let dim = cell.modifier.contains(Modifier::DIM);
            match line.last_mut() {
                Some(last) if last.fg == fg && last.bold == bold && last.dim == dim => last.t.push_str(cell.symbol()),
                _ => line.push(Span { t: cell.symbol().to_string(), fg, bold, dim }),
            }
        }
        // Trailing blanks carry nothing a pane needs to draw.
        while line.last().is_some_and(|s| s.t.trim().is_empty()) {
            line.pop();
        }
        lines.push(line);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    Ok(lines)
}

/// A cell's colour as the mod's `Text` takes it, or none for the default.
fn colour(c: ratatui::style::Color) -> Option<String> {
    use ratatui::style::Color;
    match c {
        Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
        Color::Reset => None,
        other => Some(other.to_string().to_lowercase()),
    }
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

    #[test]
    fn the_graph_comes_as_styled_lines_the_size_of_the_pane() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_agent("chief", Some("chief"), None, Some("sid-chief"), None, None).unwrap();
        db.upsert_agent("billing", None, Some("/w/billing"), Some("sid-1"), None, None).unwrap();
        let lines = graph(&db, 70, 30, None).unwrap();
        let text: Vec<String> = lines.iter().map(|l| l.iter().map(|s| s.t.as_str()).collect()).collect();
        assert!(text.iter().any(|l| l.contains("◆ chief")), "the chief's card:\n{}", text.join("\n"));
        assert!(text.iter().any(|l| l.contains("billing")), "and the worker's:\n{}", text.join("\n"));
        assert!(text.iter().all(|l| l.chars().count() <= 70), "no wider than asked");
        assert!(lines.len() < 30, "blank lines at the bottom are left off: {}", lines.len());
        let chief = lines.iter().flatten().find(|s| s.t.contains("chief") && s.bold).unwrap();
        assert!(chief.bold && chief.fg.as_deref().is_some_and(|c| c.starts_with('#')), "{chief:?}");
    }
}
