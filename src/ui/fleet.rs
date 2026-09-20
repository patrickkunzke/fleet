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

/// Lines one agent occupies: its name, its detail, and the gap after it.
/// The gap is what stops four agents reading as one block of text.
const ROW: u16 = 3;

/// Lines the rail spends on something other than agents: the heading and
/// its blank line at the top, the rule and the `+ new agent` strip at the
/// foot.
const CHROME: u16 = 4;

/// How many agents fit, after the header and the footer strip.
pub fn capacity(height: u16) -> usize {
    (height.saturating_sub(CHROME) / ROW) as usize
}

/// Which agents the rail shows, and how many are off each end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub first: usize,
    pub len: usize,
    pub above: usize,
    pub below: usize,
    /// Whether a line is set aside at each end for the markers.
    pub scrolls: bool,
}

/// Which slice of the rail is on screen.
///
/// The selected agent is always in it, and whole rows only: a two-line row
/// split across the top edge reads as a different agent. When it does not
/// all fit, a line goes to a marker at each end — reserved even when that
/// end is flush, so that rows keep their place as the selection moves. A
/// row that slides under the cursor between the look and the click is worse
/// than a row lost.
pub fn window(height: u16, count: usize, selected: usize) -> Window {
    let lines = height.saturating_sub(CHROME) as usize;
    if count <= capacity(height) {
        return Window {
            first: 0,
            len: count,
            above: 0,
            below: 0,
            scrolls: false,
        };
    }

    // Two lines for the markers, and the gap under the last row is not
    // spent: it separates rows from each other, and below the last one
    // there is nothing to separate it from.
    let len = ((lines.saturating_sub(1)) / ROW as usize).max(1).min(count);
    let first = selected
        .saturating_sub(len.saturating_sub(1))
        .min(count - len);
    Window {
        first,
        len,
        above: first,
        below: count - first - len,
        scrolls: true,
    }
}

/// Lines above the first agent: the heading, its blank line, and the marker
/// line when the rail scrolls. Shared so that a click, a fill and the rows
/// themselves cannot disagree about where the list starts.
fn head_lines(w: &Window) -> u16 {
    2 + u16::from(w.scrolls)
}

/// Where an agent is drawn, for a fill behind it. Its two lines only — the
/// gap below belongs to neither row, and filling it would join the selection
/// to whatever is under it.
fn row_rect(list: Rect, w: &Window, n: usize) -> Rect {
    let y = list.y + head_lines(w) + (n as u16 * ROW);
    let height = 2.min(list.y + list.height - y.min(list.y + list.height));
    Rect { y, height, ..list }
}

/// Which agent a click at screen row `y` landed on, if any.
///
/// Mirrors the layout in [`render`], scroll included — it takes the same
/// count and selection so that both reach the same [`window`]. A rail that
/// selects the wrong agent when clicked is worse than one that ignores the
/// mouse, and it did exactly that on any rail long enough to scroll.
pub fn row_at(area: Rect, y: u16, count: usize, selected: usize) -> Option<usize> {
    if y <= area.y || y >= area.y + area.height {
        return None;
    }
    let w = window(area.height, count, selected);
    let offset = (y - area.y).checked_sub(head_lines(&w))?;
    let n = (offset / ROW) as usize;
    (n < w.len).then_some(w.first + n)
}

pub fn render(frame: &mut Frame, area: Rect, rows: &[Row], selected: usize) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(theme::BORDER));
    let whole = block.inner(area);
    frame.render_widget(block, area);

    // The rail ends in its own strip, ruled off, saying how to add to it.
    let [list, foot_rule, foot] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(whole);
    theme::rule(frame, foot_rule);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("+", theme::accent()),
            Span::styled(" new agent  ", theme::faint()),
            Span::styled("n", theme::dim()),
        ])),
        theme::pad(foot),
    );

    let inner = theme::pad(list);
    let mut lines = vec![
        Line::from(Span::styled("AGENTS", theme::label())),
        Line::raw(""),
    ];

    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "no agents yet",
            theme::faint(),
        )));
        lines.push(Line::from(Span::styled("n  spawn one", theme::faint())));
    }

    let w = window(area.height, rows.len(), selected);
    if w.scrolls {
        // Blank when this end is flush: the line stays reserved so the rows
        // below it do not move as the selection travels.
        lines.push(match w.above {
            0 => Line::raw(""),
            n => theme::more("↑", n),
        });
    }

    for (i, row) in rows.iter().enumerate().skip(w.first).take(w.len) {
        // The gap goes between rows rather than after each one. Below the
        // last row it separates nothing, and it is the line the ↓ marker
        // needs on a short rail.
        if i > w.first {
            lines.push(Line::raw(""));
        }
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

    if w.scrolls && w.below > 0 {
        lines.push(Line::raw(""));
        lines.push(theme::more("↓", w.below));
    }

    frame.render_widget(Paragraph::new(lines), inner);

    // After the text, and across the whole rail rather than the padded
    // middle: the design fills the row edge to edge, and the gutters are
    // where a Paragraph never draws.
    if selected >= w.first && selected < w.first + w.len {
        theme::fill(
            frame,
            row_rect(list, &w, selected - w.first),
            theme::SELECTED_BG,
        );
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
        // Four agents in a rail that holds four: nothing scrolls.
        let area = Rect::new(0, 0, 26, 16);
        let at = |y| row_at(area, y, 4, 0);
        assert_eq!(at(0), None, "the heading is not an agent");
        assert_eq!(at(1), None, "nor is the blank line under it");
        assert_eq!(at(2), Some(0));
        assert_eq!(at(3), Some(0), "its detail line is still it");
        assert_eq!(at(4), Some(0), "and so is the gap under it");
        assert_eq!(at(5), Some(1));
        assert_eq!(at(16), None, "past the bottom edge");
    }

    #[test]
    fn a_click_on_a_scrolled_rail_lands_on_what_is_drawn_there() {
        // The rail holds three of eight. It selected whatever was at that
        // position in the full list instead, so every click on a rail long
        // enough to scroll went to the wrong agent.
        let area = Rect::new(0, 0, 26, 15);
        let w = window(area.height, 8, 7);
        assert!(w.scrolls && w.first > 0, "this rail has to scroll: {w:?}");

        // One line lower than before, because the ↑ marker holds a line.
        assert_eq!(row_at(area, 2, 8, 7), None, "the marker is not an agent");
        assert_eq!(row_at(area, 3, 8, 7), Some(w.first));
        assert_eq!(row_at(area, 6, 8, 7), Some(w.first + 1));
        assert_eq!(
            row_at(area, 3 + 3 * w.len as u16, 8, 7),
            None,
            "the ↓ marker is not an agent either"
        );
    }

    #[test]
    fn the_window_always_holds_the_selection() {
        for selected in 0..8 {
            let w = window(15, 8, selected);
            assert!(
                (w.first..w.first + w.len).contains(&selected),
                "{selected} is off the rail: {w:?}"
            );
            assert_eq!(w.above + w.len + w.below, 8, "every agent is accounted for");
        }
    }

    #[test]
    fn rows_keep_their_place_as_the_selection_travels_inside_the_window() {
        // The markers hold their lines even when an end is flush. Otherwise
        // the rows shift by one the moment the top marker appears, which is
        // between looking at a row and clicking it.
        let flush = window(15, 8, 0);
        let scrolled = window(15, 8, 7);
        assert_eq!(flush.above, 0, "nothing above when the selection is first");
        assert!(flush.scrolls, "but the rail still scrolls, so the line stays");
        assert_eq!(flush.len, scrolled.len, "and the same number of rows fit");
    }

    #[test]
    fn the_marker_fits_on_a_rail_barely_tall_enough_to_scroll() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // The gap under the last row is the line the ↓ marker needs. Spent
        // on a separator below the last thing there is to separate, the
        // marker was pushed off the bottom and the rail went back to hiding
        // rows without saying so.
        let agents: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|n| agent(n, "worker", Some(n), None))
            .collect();
        let rows = merge(&agents, &[]);

        let mut term = Terminal::new(TestBackend::new(26, 9)).unwrap();
        term.draw(|f| render(f, f.area(), &rows, 0)).unwrap();
        let out = format!("{}", term.backend());
        assert!(out.contains("↓ 2 more"), "{out}");
    }

    #[test]
    fn the_selected_agent_is_filled_across_the_whole_rail() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let agents: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|n| agent(n, "worker", Some(n), None))
            .collect();
        let rows = merge(&agents, &[]);

        let mut term = Terminal::new(TestBackend::new(26, 20)).unwrap();
        term.draw(|f| render(f, f.area(), &rows, 1)).unwrap();
        let buf = term.backend().buffer();
        let bg = |x: u16, y: u16| buf.cell((x, y)).unwrap().bg;

        // Agents start under the heading and its blank line; the second one
        // is two rows further down.
        let name = 2 + ROW;
        for y in [name, name + 1] {
            // The gutters too: the design fills the row edge to edge, and a
            // Paragraph never draws there.
            for x in [0, 1, 12, 24] {
                assert_eq!(
                    bg(x, y),
                    theme::SELECTED_BG,
                    "({x}, {y}) is inside the selected row"
                );
            }
        }
        assert_ne!(bg(2, name + 2), theme::SELECTED_BG, "the gap below it is not");
        assert_ne!(bg(2, name - 1), theme::SELECTED_BG, "nor the gap above");
        assert_ne!(bg(2, 2), theme::SELECTED_BG, "nor the agent above it");
    }

    #[test]
    fn the_fill_follows_the_selection_down_a_scrolled_rail() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let agents: Vec<_> = ["a", "b", "c", "d", "e", "f"]
            .iter()
            .map(|n| agent(n, "worker", Some(n), None))
            .collect();
        let rows = merge(&agents, &[]);
        let area = Rect::new(0, 0, 26, 15);
        let w = window(area.height, rows.len(), 5);
        assert!(w.scrolls, "this rail has to scroll");

        let mut term = Terminal::new(TestBackend::new(26, 15)).unwrap();
        term.draw(|f| render(f, area, &rows, 5)).unwrap();
        let buf = term.backend().buffer();

        // Where the last drawn row is, marker line included — the same sum
        // row_at uses, so a click and the fill cannot point at different
        // agents.
        let y = head_lines(&w) + (5 - w.first) as u16 * ROW;
        assert_eq!(buf.cell((2, y)).unwrap().bg, theme::SELECTED_BG);
        assert_eq!(row_at(area, y, rows.len(), 5), Some(5));
    }

    #[test]
    fn a_rail_that_fits_says_nothing_about_scrolling() {
        let w = window(22, 4, 0);
        assert!(!w.scrolls);
        assert_eq!((w.first, w.len, w.above, w.below), (0, 4, 0, 0));
    }

    #[test]
    fn a_rail_with_more_agents_than_it_can_show_says_so_at_both_ends() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let agents: Vec<_> = ["a", "b", "c", "d", "e", "f", "g", "h"]
            .iter()
            .map(|n| agent(n, "worker", Some(n), None))
            .collect();
        let rows = merge(&agents, &[]);

        let mut term = Terminal::new(TestBackend::new(26, 15)).unwrap();
        term.draw(|f| render(f, f.area(), &rows, 4)).unwrap();
        let out = format!("{}", term.backend());

        assert!(out.contains("↑"), "nothing says rows are above: {out}");
        assert!(out.contains("more"), "{out}");
        assert!(out.contains("↓"), "nothing says rows are below: {out}");
    }

    #[test]
    fn capacity_leaves_room_for_the_header_and_the_footer() {
        assert_eq!(capacity(4), 0);
        assert_eq!(capacity(7), 1);
        assert_eq!(capacity(22), 6);
    }

    #[test]
    fn the_rail_draws_its_agents_and_says_so_when_there_are_none() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut term = Terminal::new(TestBackend::new(26, 12)).unwrap();
        term.draw(|f| render(f, f.area(), &[], 0)).unwrap();
        let empty = format!("{}", term.backend());
        assert!(empty.contains("no agents yet"), "{empty}");
        assert!(empty.contains("new agent"), "the way to add one is always there: {empty}");
        assert!(empty.contains("spawn one"), "{empty}");

        let agents = [agent("billing-svc", "worker", Some("sess-1"), Some("ENG-2553-2"))];
        let live = [session("billing-service-50", "sess-1", "/repo/content", Status::Busy)];
        let rows = merge(&agents, &live);

        let mut term = Terminal::new(TestBackend::new(26, 12)).unwrap();
        term.draw(|f| render(f, f.area(), &rows, 0)).unwrap();
        let drawn = format!("{}", term.backend());

        assert!(drawn.contains("AGENTS"), "{drawn}");
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
