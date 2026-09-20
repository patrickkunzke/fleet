//! The right rail: what the chief wrote down, and what is running.
//!
//! Two halves with different jobs. The top is the plan — every task, grouped
//! by epic, deliberately *not* filtered to the selected agent: the reason the
//! chief exists is that a task in one repo blocks a task in another, and a
//! rail that showed only the agent you happen to be reading would hide
//! exactly that. The selected agent's own task is marked instead.
//!
//! The bottom is everything outliving a turn — dev servers, compose stacks,
//! test runs. It is there so nobody starts a second copy of a server that is
//! already up, which is the failure it exists to prevent.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::db::{BgTask, State, Task};
use crate::ui::theme;

/// Lines one background entry takes: its command, then its detail.
const BG_LINES: u16 = 2;

pub fn render(
    frame: &mut Frame,
    area: Rect,
    tasks: &[Task],
    background: &[BgTask],
    selected_agent: Option<&str>,
) {
    // Give the background half only what it needs, and never more than half
    // the rail: the plan is the thing you read, the processes are a glance.
    // The divider's own row counts, or the last line is always the one cut.
    let content = if background.is_empty() {
        1
    } else {
        BG_LINES * background.len().min(6) as u16
    };
    let wanted = 1 /* divider */ + 1 /* header */ + content;
    let bg_height = wanted.min(area.height / 2).max(3);

    let [top, bottom] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(bg_height)]).areas(area);

    render_tasks(frame, top, tasks, selected_agent);

    let divider = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme::BORDER));
    let inner = divider.inner(bottom);
    frame.render_widget(divider, bottom);
    render_background(frame, inner, background);
}

fn render_tasks(frame: &mut Frame, area: Rect, tasks: &[Task], selected_agent: Option<&str>) {
    let open = tasks.iter().filter(|t| t.state != State::Done).count();
    let mut lines = vec![Line::from(vec![
        Span::styled("TASKS", theme::label()),
        Span::raw("  "),
        Span::styled(format!("{open} open"), theme::faint()),
    ])];

    if tasks.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "nothing on the board",
            theme::faint(),
        )));
        lines.push(Line::from(Span::styled(
            "the chief writes it",
            theme::faint(),
        )));
        frame.render_widget(Paragraph::new(lines), theme::pad(area));
        return;
    }

    let width = area.width.saturating_sub(theme::GUTTER * 2);
    let room = area.height as usize;
    let mut epic: Option<&str> = None;
    // Counted rather than derived from the line count, which also holds the
    // epic headings and the blank lines between them — deriving it printed
    // "+0 more" on a list that fitted perfectly.
    let mut shown = 0usize;

    for task in tasks {
        if lines.len() + 1 >= room {
            lines.push(Line::from(Span::styled(
                format!("+{} more", tasks.len() - shown),
                theme::faint(),
            )));
            break;
        }

        let key = task.epic_key.as_deref();
        if key != epic {
            epic = key;
            lines.push(Line::raw(""));
            let done = tasks
                .iter()
                .filter(|t| t.epic_key.as_deref() == key && t.state == State::Done)
                .count();
            let total = tasks.iter().filter(|t| t.epic_key.as_deref() == key).count();
            lines.push(Line::from(vec![
                Span::styled(
                    key.unwrap_or("loose").to_string(),
                    theme::accent().add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(format!("{done}/{total}"), theme::faint()),
            ]));
        }

        let mine = selected_agent.is_some() && task.agent.as_deref() == selected_agent;
        let (glyph, colour) = match task.state {
            State::Done => ("✔", theme::OK),
            State::Running => ("▸", theme::BUSY),
            State::Blocked => ("■", theme::ACCENT),
            State::Review => ("◇", theme::OK),
            State::Dropped => ("–", theme::FAINT),
            State::Queued => ("·", theme::FAINT),
        };
        let body = match task.agent.as_deref() {
            Some(agent) => format!("{agent} · {}", task.title),
            None => task.title.clone(),
        };
        let style = if mine {
            Style::default().fg(theme::TEXT)
        } else if task.state == State::Done {
            theme::faint()
        } else {
            theme::dim()
        };

        lines.push(Line::from(vec![
            // The selected agent's task is marked, so the two rails are
            // visibly about the same thing.
            Span::styled(if mine { "▌" } else { " " }, theme::accent()),
            Span::styled(glyph, Style::default().fg(colour)),
            Span::raw(" "),
            Span::styled(clip(&body, width.saturating_sub(3)), style),
        ]));
        shown += 1;

        if let Some(why) = &task.blocked_on
            && lines.len() < room
        {
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(clip(why, width.saturating_sub(3)), theme::accent()),
            ]));
        } else if !task.waiting_on.is_empty() && lines.len() < room {
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    clip(&format!("waits on {}", task.waiting_on.join(", ")), width.saturating_sub(3)),
                    theme::faint(),
                ),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(lines), theme::pad(area));
}

fn render_background(frame: &mut Frame, area: Rect, background: &[BgTask]) {
    let running = background.iter().filter(|b| b.state == "running").count();
    let mut lines = vec![Line::from(vec![
        Span::styled("BACKGROUND", theme::label()),
        Span::raw("  "),
        Span::styled(format!("{running} running"), theme::faint()),
    ])];

    if background.is_empty() {
        lines.push(Line::from(Span::styled("nothing running", theme::faint())));
        frame.render_widget(Paragraph::new(lines), theme::pad(area));
        return;
    }

    let width = area.width.saturating_sub(theme::GUTTER * 2);
    let room = area.height as usize;

    for bg in background {
        if lines.len() + 2 > room {
            break;
        }
        // A distinct glyph per state, not just a colour: a failed build and
        // a running one must not be the same shape.
        let (glyph, colour) = match bg.state.as_str() {
            "running" => ("●", theme::BUSY),
            "passed" => ("✔", theme::OK),
            "failed" => ("✗", theme::ACCENT),
            _ => ("○", theme::FAINT),
        };
        let took = elapsed(bg.elapsed_secs);
        let command = clip(
            &bg.command,
            width.saturating_sub(took.chars().count() as u16 + 3),
        );
        // Right-align the time so it reads as a column rather than trailing
        // each command at a different place.
        let gap = (width as usize)
            .saturating_sub(2 + command.chars().count() + took.chars().count());
        lines.push(Line::from(vec![
            Span::styled(glyph, Style::default().fg(colour)),
            Span::raw(" "),
            Span::styled(command, Style::default().fg(theme::TEXT)),
            Span::raw(" ".repeat(gap)),
            Span::styled(took, theme::faint()),
        ]));

        // The second line carries whose it is and what it amounted to, which
        // is what makes a failed build actionable instead of just red.
        let mut detail = bg.agent.clone().unwrap_or_else(|| "?".into());
        if let Some(port) = bg.port {
            detail.push_str(&format!(" · :{port}"));
        }
        if let Some(note) = &bg.detail {
            detail.push_str(&format!(" · {note}"));
        }
        let detail_style = if bg.state == "failed" {
            theme::accent()
        } else {
            theme::faint()
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(clip(&detail, width.saturating_sub(2)), detail_style),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), theme::pad(area));
}

/// "4h12m", "1m02s", "8s" — fixed width enough to sit in a column.
fn elapsed(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        s if s >= 3600 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        s if s >= 60 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

fn clip(s: &str, width: u16) -> String {
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn task(key: &str, epic: &str, state: State, agent: Option<&str>, title: &str) -> Task {
        Task {
            key: key.into(),
            title: title.into(),
            state,
            repo: "/repo".into(),
            epic_key: Some(epic.into()),
            agent: agent.map(str::to_string),
            mr_url: None,
            blocked_on: None,
            waiting_on: Vec::new(),
        }
    }

    fn bg(id: i64, command: &str, state: &str, secs: i64) -> BgTask {
        BgTask {
            id,
            agent: Some("storefront".into()),
            repo: None,
            command: command.into(),
            kind: "script".into(),
            port: None,
            state: state.into(),
            detail: None,
            started_at: "2026-09-20T14:00:00Z".into(),
            elapsed_secs: secs,
        }
    }

    fn drawn(tasks: &[Task], background: &[BgTask], agent: Option<&str>, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render(f, f.area(), tasks, background, agent))
            .unwrap();
        format!("{}", term.backend())
    }

    #[test]
    fn tasks_are_grouped_by_epic_with_a_count() {
        let tasks = [
            task("ENG-2553-1", "ENG-2553", State::Done, Some("accounts-svc"), "shared column"),
            task("ENG-2553-2", "ENG-2553", State::Running, Some("billing-svc"), "consume param"),
            task("ENG-2601-1", "ENG-2601", State::Queued, None, "logger v9"),
        ];
        let out = drawn(&tasks, &[], None, 34, 24);

        assert!(out.contains("ENG-2553"), "{out}");
        assert!(out.contains("1/2"), "one of two done: {out}");
        assert!(out.contains("ENG-2601"), "{out}");
        assert!(out.contains("0/1"), "{out}");
        assert!(out.contains("2 open"), "done ones are not open: {out}");
    }

    #[test]
    fn the_selected_agents_task_is_marked_but_the_others_still_show() {
        let tasks = [
            task("ENG-2553-1", "ENG-2553", State::Running, Some("billing-svc"), "consume param"),
            task("ENG-2553-2", "ENG-2553", State::Queued, Some("workspace"), "UI toggle"),
        ];
        let out = drawn(&tasks, &[], Some("billing-svc"), 34, 24);

        assert!(out.contains('▌'), "the selected agent's task is marked: {out}");
        assert!(
            out.contains("UI toggle"),
            "the other repo's task is the whole point of the rail: {out}"
        );
    }

    #[test]
    fn a_blocker_is_spelled_out_under_its_task() {
        let mut blocked = task("ENG-2553-2", "ENG-2553", State::Blocked, Some("billing-svc"), "consume param");
        blocked.blocked_on = Some("needs !412 merged".into());
        let out = drawn(&[blocked], &[], None, 34, 24);

        assert!(out.contains("needs !412"), "{out}");
    }

    #[test]
    fn a_dependency_is_shown_when_there_is_no_blocker_of_its_own() {
        let mut waiting = task("ENG-2553-2", "ENG-2553", State::Queued, None, "consume param");
        waiting.waiting_on = vec!["ENG-2553-1".into()];
        let out = drawn(&[waiting], &[], None, 34, 24);

        assert!(out.contains("waits on ENG-2553-1"), "{out}");
    }

    #[test]
    fn background_shows_what_is_running_and_how_long_for() {
        let mut server = bg(1, "pnpm dev", "running", 15_120);
        server.port = Some(3000);
        let mut build = bg(2, "pnpm build", "failed", 121);
        build.detail = Some("2 type errors".into());

        let out = drawn(&[], &[server, build], None, 34, 24);
        assert!(out.contains("pnpm dev"), "{out}");
        assert!(out.contains("4h12m"), "{out}");
        assert!(out.contains(":3000"), "a server's port is the useful part: {out}");
        assert!(out.contains("2 type errors"), "a failure says why: {out}");
        assert!(out.contains("1 running"), "only one of the two is live: {out}");
        assert!(
            out.contains('✗'),
            "a failure is a different shape, not only a different colour: {out}"
        );
        assert!(out.contains('●'), "and a running one keeps the dot: {out}");
    }

    #[test]
    fn elapsed_reads_at_every_scale() {
        assert_eq!(elapsed(8), "8s");
        assert_eq!(elapsed(62), "1m02s");
        assert_eq!(elapsed(15_120), "4h12m");
        assert_eq!(elapsed(-1), "0s", "a clock skew must not print nonsense");
    }

    #[test]
    fn a_list_that_fits_does_not_claim_to_have_hidden_anything() {
        let tasks = [
            task("ENG-1-1", "ENG-1", State::Queued, None, "one"),
            task("ENG-1-2", "ENG-1", State::Queued, None, "two"),
        ];
        let out = drawn(&tasks, &[], None, 34, 24);
        assert!(!out.contains("more"), "{out}");
    }

    #[test]
    fn an_empty_rail_says_what_would_fill_it() {
        let out = drawn(&[], &[], None, 34, 24);
        assert!(out.contains("nothing on the board"), "{out}");
        assert!(out.contains("the chief writes it"), "{out}");
        assert!(out.contains("nothing running"), "{out}");
    }

    #[test]
    fn a_rail_too_short_for_everything_says_how_much_it_hid() {
        let tasks: Vec<_> = (0..20)
            .map(|i| task(&format!("ENG-1-{i}"), "ENG-1", State::Queued, None, "a task"))
            .collect();
        let out = drawn(&tasks, &[], None, 34, 12);
        assert!(out.contains("more"), "a truncated list must admit it: {out}");
        assert!(!out.contains("+0 more"), "and must not claim to hide nothing: {out}");
    }

    #[test]
    fn it_draws_in_a_short_rail_without_panicking() {
        let tasks = [task("ENG-1-1", "ENG-1", State::Queued, None, "a task")];
        for (w, h) in [(20, 4), (34, 6), (60, 60)] {
            let _ = drawn(&tasks, &[bg(1, "pnpm dev", "running", 30)], None, w, h);
        }
    }
}
