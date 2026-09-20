//! The left rail: the agents this fleet created, and what each is doing.
//!
//! Deliberately not a list of repositories. A repo with no agent in it is not
//! shown, because a rail of twenty-five idle names is a rail nobody reads.
//!
//! What it does show beyond the fleet's own agents is a live Claude Code
//! session running somewhere the fleet is not tracking. Hiding a running
//! agent would defeat the point of the view, so those appear at the bottom,
//! dimmed, ready to be adopted.

use std::path::Path;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::db;
use crate::registry::{self, Status};
use crate::ui::theme;

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
    /// A live session the fleet did not start.
    Unmanaged,
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
    pub session_id: Option<String>,
    pub pid: Option<i32>,
}

impl Row {
    fn glyph(&self) -> (&'static str, Color) {
        match (self.role, self.presence) {
            (Role::Chief, Presence::Working | Presence::Waiting) => ("◆", theme::ACCENT),
            (Role::Chief, _) => ("◇", theme::FAINT),
            (_, Presence::Working) => ("●", theme::BUSY),
            (_, Presence::Waiting) => ("○", theme::OK),
            (_, Presence::Gone) => ("×", theme::FAINT),
            (_, Presence::Unlinked) => ("·", theme::FAINT),
        }
    }

    fn name_style(&self) -> Style {
        match (self.role, self.presence) {
            (Role::Unmanaged, _) | (_, Presence::Gone | Presence::Unlinked) => theme::dim(),
            _ => Style::default().fg(theme::TEXT),
        }
    }
}

/// Join what the board knows to what is actually running.
///
/// The board says which agents exist and what they hold; the registry says
/// which are alive right now. Neither alone is the rail: an agent whose
/// process died still has a task assigned, and a session nobody registered is
/// still doing work.
pub fn merge(
    agents: &[db::Agent],
    sessions: &[registry::Session],
    root: Option<&Path>,
) -> Vec<Row> {
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
            session_id: a.session_id.clone(),
            pid: live.map(|s| s.pid),
        });
    }

    rows.sort_by(|a, b| {
        // The chief first — it is the one you brief.
        let rank = |r: &Row| match r.role {
            Role::Chief => 0,
            Role::Worker => 1,
            Role::Unmanaged => 2,
        };
        rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name))
    });

    let claimed: Vec<&str> = rows.iter().filter_map(|r| r.session_id.as_deref()).collect();
    let mut loose: Vec<Row> = sessions
        .iter()
        .filter(|s| s.is_interactive())
        .filter(|s| !claimed.contains(&s.session_id.as_str()))
        .filter(|s| root.is_none_or(|r| s.cwd.starts_with(r)))
        .map(|s| Row {
            name: s.name.clone(),
            role: Role::Unmanaged,
            repo: s.repo().to_string(),
            presence: if s.status == Status::Busy {
                Presence::Working
            } else {
                Presence::Waiting
            },
            detail: "not on the board".to_string(),
            bg_running: 0,
            session_id: Some(s.session_id.clone()),
            pid: Some(s.pid),
        })
        .collect();
    loose.sort_by(|a, b| a.name.cmp(&b.name));
    rows.append(&mut loose);

    rows
}

fn short_repo(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
}

/// How many rows fit, given two lines each plus the header.
pub fn capacity(height: u16) -> usize {
    (height.saturating_sub(2) / 2) as usize
}

pub fn render(frame: &mut Frame, area: Rect, rows: &[Row], selected: usize) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(theme::BORDER));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let working = rows.iter().filter(|r| r.presence == Presence::Working).count();
    let mut lines = vec![Line::from(vec![
        Span::styled("FLEET", theme::label()),
        Span::raw("  "),
        Span::styled(format!("{working} working"), theme::faint()),
    ])];

    if rows.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "no agents yet",
            theme::faint(),
        )));
        lines.push(Line::from(Span::styled(
            "n  spawn one",
            theme::faint(),
        )));
    }

    // Keep the selected row on screen by scrolling whole rows, never halves:
    // a two-line row split across the top edge reads as a different agent.
    let fits = capacity(inner.height).max(1);
    let first = selected.saturating_sub(fits.saturating_sub(1));

    for (i, row) in rows.iter().enumerate().skip(first).take(fits) {
        let is_selected = i == selected;
        let (glyph, colour) = row.glyph();
        let bar = if is_selected { "▌" } else { " " };

        let mut head = vec![
            Span::styled(bar, theme::accent()),
            Span::styled(glyph, Style::default().fg(colour)),
            Span::raw(" "),
            Span::styled(
                truncate(&row.name, inner.width.saturating_sub(bg_width(row) + 3)),
                row.name_style(),
            ),
        ];
        if row.bg_running > 0 {
            head.push(Span::styled(
                format!("  {}bg", row.bg_running),
                Style::default().fg(theme::BUSY),
            ));
        }

        let detail = Line::from(vec![
            Span::styled(bar, theme::accent()),
            Span::raw("  "),
            Span::styled(truncate(&row.detail, inner.width.saturating_sub(4)), theme::faint()),
        ]);

        let style = if is_selected {
            theme::selected()
        } else {
            Style::default()
        };
        lines.push(Line::from(head).style(style));
        lines.push(detail.style(style));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Room the background-task badge will want on the same line.
fn bg_width(row: &Row) -> u16 {
    if row.bg_running > 0 {
        4 + row.bg_running.to_string().len() as u16
    } else {
        0
    }
}

fn truncate(s: &str, width: u16) -> String {
    let width = width as usize;
    if s.chars().count() <= width {
        return s.to_string();
    }
    let cut: String = s.chars().take(width.saturating_sub(1)).collect();
    format!("{cut}…")
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
            tmux_target: None,
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
        let names: Vec<_> = merge(&agents, &[], None)
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, vec!["chief", "billing-svc", "storefront"]);
    }

    #[test]
    fn presence_comes_from_the_registry_not_the_board() {
        let agents = [agent("billing-svc", "worker", Some("sess-1"), Some("ENG-1"))];
        let live = [session("billing-service-50", "sess-1", "/repo/content", Status::Busy)];

        let rows = merge(&agents, &live, None);
        assert_eq!(rows[0].presence, Presence::Working);
        assert_eq!(rows[0].pid, Some(4242));
        assert_eq!(rows[0].detail, "ENG-1 · consume the param");
    }

    #[test]
    fn never_linked_is_not_the_same_as_died() {
        // Registered by hand, never spawned: nothing has been lost.
        let rows = merge(&[agent("chief", "chief", None, None)], &[], None);
        assert_eq!(rows[0].presence, Presence::Unlinked);
        assert_eq!(rows[0].detail, "no session yet");

        // Spawned, adopted, and then the process went away: something has.
        let rows = merge(&[agent("billing-svc", "worker", Some("sess-1"), None)], &[], None);
        assert_eq!(rows[0].presence, Presence::Gone);
        assert_eq!(rows[0].detail, "session ended");
    }

    #[test]
    fn a_long_name_is_cut_with_an_ellipsis_not_silently() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let agents = [agent("dev-tools", "worker", None, None)];
        let rows = merge(&agents, &[], None);

        let mut term = Terminal::new(TestBackend::new(26, 8)).unwrap();
        term.draw(|f| render(f, f.area(), &rows, 0)).unwrap();
        let drawn = format!("{}", term.backend());

        assert!(drawn.contains('…'), "a cut name must say it was cut: {drawn}");
        assert!(!drawn.contains("booster-9000-98"), "{drawn}");
    }

    #[test]
    fn an_agent_whose_process_died_is_shown_as_gone_not_dropped() {
        let agents = [agent("billing-svc", "worker", Some("sess-1"), None)];
        let rows = merge(&agents, &[], None);

        assert_eq!(rows.len(), 1, "it still holds a row on the board");
        assert_eq!(rows[0].presence, Presence::Gone);
        assert_eq!(rows[0].detail, "session ended");
    }

    #[test]
    fn a_live_session_nobody_registered_is_listed_last_as_unmanaged() {
        let agents = [agent("billing-svc", "worker", Some("sess-1"), None)];
        let live = [
            session("billing-service-50", "sess-1", "/repo/content", Status::Idle),
            session("scratch", "sess-2", "/repo/workspace", Status::Idle),
        ];

        let rows = merge(&agents, &live, None);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].name, "scratch");
        assert_eq!(rows[1].role, Role::Unmanaged);
        assert_eq!(rows[1].detail, "not on the board");
    }

    #[test]
    fn a_session_outside_the_workspace_is_someone_elses_business() {
        let live = [
            session("acme-de", "sess-1", "/work/acme/ui", Status::Idle),
            session("elsewhere", "sess-2", "/tmp/other", Status::Idle),
        ];
        let rows = merge(&[], &live, Some(Path::new("/work/acme")));

        let names: Vec<_> = rows.into_iter().map(|r| r.name).collect();
        assert_eq!(names, vec!["acme-de"]);
    }

    #[test]
    fn rows_are_two_lines_so_capacity_is_half_the_space() {
        assert_eq!(capacity(2), 0);
        assert_eq!(capacity(4), 1);
        assert_eq!(capacity(20), 9);
    }

    #[test]
    fn the_rail_draws_its_agents_and_says_so_when_there_are_none() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut term = Terminal::new(TestBackend::new(26, 12)).unwrap();
        term.draw(|f| render(f, f.area(), &[], 0)).unwrap();
        let empty = format!("{}", term.backend());
        assert!(empty.contains("no agents yet"), "{empty}");
        assert!(empty.contains("spawn one"), "{empty}");

        let agents = [agent("billing-svc", "worker", Some("sess-1"), Some("ENG-2553-2"))];
        let live = [session("billing-service-50", "sess-1", "/repo/content", Status::Busy)];
        let rows = merge(&agents, &live, None);

        let mut term = Terminal::new(TestBackend::new(26, 12)).unwrap();
        term.draw(|f| render(f, f.area(), &rows, 0)).unwrap();
        let drawn = format!("{}", term.backend());

        assert!(drawn.contains("FLEET"), "{drawn}");
        assert!(drawn.contains("1 working"), "{drawn}");
        assert!(drawn.contains("billing-svc"), "{drawn}");
        assert!(drawn.contains("ENG-2553-2"), "{drawn}");
        assert!(drawn.contains('▌'), "the selected row is marked: {drawn}");
    }

    #[test]
    fn the_selected_row_stays_on_screen_when_the_list_is_long() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let agents: Vec<_> = (0..12)
            .map(|i| agent(&format!("agent-{i:02}"), "worker", None, None))
            .collect();
        let rows = merge(&agents, &[], None);

        // Room for three rows; select the last one.
        let mut term = Terminal::new(TestBackend::new(26, 8)).unwrap();
        term.draw(|f| render(f, f.area(), &rows, rows.len() - 1))
            .unwrap();
        let drawn = format!("{}", term.backend());

        assert!(drawn.contains("agent-11"), "the selection is visible: {drawn}");
        assert!(!drawn.contains("agent-00"), "and the top has scrolled off: {drawn}");
    }
}
