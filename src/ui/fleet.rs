//! The left rail: the agents this fleet created, and what each is doing.
//!
//! Only agents the fleet started. Not repositories, and not every Claude
//! Code session on the machine: this is the crew working on the thing in
//! front of you, and anything else in the list is noise competing with it.

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
    pub tmux_target: Option<String>,
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
            (_, Presence::Gone | Presence::Unlinked) => theme::dim(),
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
            tmux_target: a.tmux_target.clone(),
            pid: live.map(|s| s.pid),
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
/// sense, produces a number wide enough to push the agent's name off the
/// rail. Past a fortnight it is not an uptime, so it is not shown.
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

/// How many rows fit, given two lines each plus the header.
pub fn capacity(height: u16) -> usize {
    (height.saturating_sub(2) / 2) as usize
}

/// Which agent a click at screen row `y` landed on, if any.
///
/// Mirrors the layout in [`render`]: one header line, then two lines per
/// agent. Kept beside it so the two cannot drift, because a rail that
/// selects the wrong agent when clicked is worse than one that ignores the
/// mouse.
pub fn row_at(area: Rect, y: u16) -> Option<usize> {
    if y <= area.y || y >= area.y + area.height {
        return None;
    }
    Some(((y - area.y - 1) / 2) as usize)
}

pub fn render(frame: &mut Frame, area: Rect, rows: &[Row], selected: usize) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(theme::BORDER));
    let inner = theme::pad(block.inner(area));
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
        lines.push(Line::from(Span::styled("n  spawn one", theme::faint())));
    }

    // Keep the selected row on screen by scrolling whole rows, never halves:
    // a two-line row split across the top edge reads as a different agent.
    let fits = capacity(inner.height).max(1);
    let first = selected.saturating_sub(fits.saturating_sub(1));

    for (i, row) in rows.iter().enumerate().skip(first).take(fits) {
        let is_selected = i == selected;
        let (glyph, colour) = row.glyph();
        let bar = if is_selected { "▌" } else { " " };

        // Right-hand column first, so the name knows how much room is left.
        let right = match (row.bg_running, &row.uptime) {
            (0, Some(up)) => up.clone(),
            (n, Some(up)) => format!("{n}bg {up}"),
            (0, None) => String::new(),
            (n, None) => format!("{n}bg"),
        };
        let name = truncate(
            &row.name,
            inner.width.saturating_sub(right.chars().count() as u16 + 4),
        );
        let gap = (inner.width as usize)
            .saturating_sub(3 + name.chars().count() + right.chars().count());
        let head = vec![
            Span::styled(bar, theme::accent()),
            Span::styled(glyph, Style::default().fg(colour)),
            Span::raw(" "),
            Span::styled(name, row.name_style()),
            Span::raw(" ".repeat(gap)),
            Span::styled(
                right,
                if row.bg_running > 0 {
                    Style::default().fg(theme::BUSY)
                } else {
                    theme::faint()
                },
            ),
        ];

        let detail = Line::from(vec![
            Span::styled(bar, theme::accent()),
            Span::raw("  "),
            Span::styled(truncate(&row.detail, inner.width.saturating_sub(3)), theme::faint()),
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
    fn a_long_name_is_cut_with_an_ellipsis_not_silently() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let agents = [agent("dev-tools", "worker", None, None)];
        let rows = merge(&agents, &[]);

        let mut term = Terminal::new(TestBackend::new(26, 8)).unwrap();
        term.draw(|f| render(f, f.area(), &rows, 0)).unwrap();
        let drawn = format!("{}", term.backend());

        assert!(drawn.contains('…'), "a cut name must say it was cut: {drawn}");
        assert!(!drawn.contains("booster-9000-98"), "{drawn}");
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

    #[test]
    fn a_click_lands_on_the_agent_it_looks_like() {
        let area = Rect::new(0, 0, 26, 12);
        assert_eq!(row_at(area, 0), None, "the header is not an agent");
        assert_eq!(row_at(area, 1), Some(0));
        assert_eq!(row_at(area, 2), Some(0), "both of an agent's two lines");
        assert_eq!(row_at(area, 3), Some(1));
        assert_eq!(row_at(area, 4), Some(1));
        assert_eq!(row_at(area, 12), None, "past the bottom edge");
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
        let rows = merge(&agents, &live);

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
        let rows = merge(&agents, &[]);

        // Room for three rows; select the last one.
        let mut term = Terminal::new(TestBackend::new(26, 8)).unwrap();
        term.draw(|f| render(f, f.area(), &rows, rows.len() - 1))
            .unwrap();
        let drawn = format!("{}", term.backend());

        assert!(drawn.contains("agent-11"), "the selection is visible: {drawn}");
        assert!(!drawn.contains("agent-00"), "and the top has scrolled off: {drawn}");
    }
}
