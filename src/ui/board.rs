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

/// The tasks of every epic that still has work in it. An epic whose tasks
/// are all done or dropped is finished with, and last week's would otherwise
/// fill the rail above this week's. It stays on the board itself, in
/// `fleet board ls`.
fn unfinished(tasks: &[Task]) -> Vec<Task> {
    let finished = |key: Option<&str>| {
        tasks
            .iter()
            .filter(|t| t.epic_key.as_deref() == key)
            .all(|t| matches!(t.state, State::Done | State::Dropped))
    };
    tasks.iter().filter(|t| !finished(t.epic_key.as_deref())).cloned().collect()
}

pub fn render(
    frame: &mut Frame,
    area: Rect,
    tasks: &[Task],
    background: &[BgTask],
    selected_agent: Option<&str>,
    // The key that opens the log from here: `^a l` where every other key
    // is an agent's, `l` where none is.
    log_key: &str,
) {
    let open = unfinished(tasks);
    let tasks = open.as_slice();
    // Give the background half only what it needs, and never more than half
    // the rail: the plan is the thing you read, the processes are a glance.
    // The divider's own row counts, or the last line is always the one cut.
    let content = if background.is_empty() {
        1
    } else {
        BG_LINES * background.len().min(6) as u16
    };
    let wanted = 1 /* divider */ + 1 /* header */ + 1 /* its blank line */ + content;
    let bg_height = wanted.min(area.height / 2).max(4);

    // A strip at the foot, ruled off, the way the left rail has one.
    let [top, bottom, foot_rule, foot] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(bg_height),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    render_tasks(frame, top, tasks, selected_agent);

    let divider = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme::BORDER));
    let inner = divider.inner(bottom);
    frame.render_widget(divider, bottom);
    render_background(frame, inner, background);

    theme::rule(frame, foot_rule);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(log_key.to_string(), theme::dim()),
            Span::styled(" the flow log", theme::faint()),
        ])),
        theme::pad(foot),
    );
}

/// What the board along the foot shows under the tasks: only while
/// something is running. A pane in a split has no rows to spare for an
/// empty heading, or for last hour's finished build.
fn running_now(background: &[BgTask]) -> Vec<BgTask> {
    background.iter().filter(|b| b.state == "running").cloned().collect()
}

/// Rows the background section takes under the tasks: its divider, its
/// heading and blank line, and each entry.
fn stacked_bg_height(background: &[BgTask]) -> u16 {
    let running = running_now(background).len();
    if running == 0 {
        return 0;
    }
    3 + BG_LINES * running.min(6) as u16
}

/// Rows the board would like along the foot of a narrow pane: every open
/// task, and under them what is running.
pub fn stacked_height(tasks: &[Task], background: &[BgTask]) -> u16 {
    let open = unfinished(tasks);
    let mut epics: Vec<Option<&str>> = open.iter().map(|t| t.epic_key.as_deref()).collect();
    epics.dedup();
    let notes = open
        .iter()
        .filter(|t| t.blocked_on.is_some() || !t.waiting_on.is_empty())
        .count();
    let tasks = if open.is_empty() { 2 } else { open.len() + notes + 2 * epics.len() };
    // The heading and its blank line.
    (2 + tasks) as u16 + stacked_bg_height(background)
}

/// The board along the foot of the window rather than down its side: a pane
/// in a split is tall and narrow, and a column of 36 there both starves the
/// graph and clips every title. The tasks get the whole width, and what is
/// running goes under them, while anything is.
pub fn render_stacked(
    frame: &mut Frame,
    area: Rect,
    tasks: &[Task],
    background: &[BgTask],
    selected_agent: Option<&str>,
) {
    let open = unfinished(tasks);
    // The tasks keep at least half: they are what the board is for.
    let bg_height = stacked_bg_height(background).min(area.height / 2);
    let [top, bottom] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(bg_height)]).areas(area);
    render_tasks(frame, top, &open, selected_agent);
    if bg_height > 0 {
        let divider = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(theme::BORDER));
        let inner = divider.inner(bottom);
        frame.render_widget(divider, bottom);
        render_background(frame, inner, &running_now(background));
    }
}

fn render_tasks(frame: &mut Frame, area: Rect, tasks: &[Task], selected_agent: Option<&str>) {
    let open = tasks.iter().filter(|t| t.state != State::Done).count();
    let width = area.width.saturating_sub(theme::GUTTER * 2);
    let mut lines = vec![theme::spread(
        vec![Span::styled("TASKS", theme::label())],
        vec![Span::styled(format!("{open} open"), theme::faint())],
        width,
    )];
    lines.push(Line::raw(""));

    if tasks.is_empty() {
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

    let room = area.height as usize;
    // The heading's own blank line already separates it from the first epic.
    let mut epic: Option<&str> = Some("");
    // Counted rather than derived from the line count, which also holds the
    // epic headings and the blank lines between them — deriving it printed
    // "+0 more" on a list that fitted perfectly.
    let mut shown = 0usize;

    for task in tasks {
        if lines.len() + 1 >= room {
            lines.push(theme::more("↓", tasks.len() - shown));
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
            lines.push(theme::spread(
                vec![Span::styled(
                    key.unwrap_or("loose").to_string(),
                    theme::accent().add_modifier(Modifier::BOLD),
                )],
                vec![Span::styled(format!("{done}/{total}"), theme::faint())],
                width,
            ));
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
    let width = area.width.saturating_sub(theme::GUTTER * 2);
    let mut lines = vec![theme::spread(
        vec![Span::styled("BACKGROUND", theme::label())],
        vec![Span::styled(format!("{running} running"), theme::faint())],
        width,
    )];
    lines.push(Line::raw(""));

    if background.is_empty() {
        lines.push(Line::from(Span::styled("nothing running", theme::faint())));
        frame.render_widget(Paragraph::new(lines), theme::pad(area));
        return;
    }

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
            width.saturating_sub(took.chars().count() as u16 + 4),
        );
        // Right-align the time so it reads as a column rather than trailing
        // each command at a different place.
        let gap = (width as usize)
            .saturating_sub(3 + command.chars().count() + took.chars().count());
        lines.push(Line::from(vec![
            Span::raw(" "),
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
            Span::raw("   "),
            Span::styled(clip(&detail, width.saturating_sub(3)), detail_style),
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
        term.draw(|f| render(f, f.area(), tasks, background, agent, "^a l"))
            .unwrap();
        format!("{}", term.backend())
    }

    #[test]
    fn along_the_foot_only_what_is_running_is_shown_under_the_tasks() {
        let tasks = vec![task("ENG-1-1", "ENG-1", State::Running, None, "the work")];
        let stacked = |background: &[BgTask]| {
            let mut term = Terminal::new(TestBackend::new(80, 16)).unwrap();
            term.draw(|f| render_stacked(f, f.area(), &tasks, background, None)).unwrap();
            format!("{}", term.backend())
        };

        // A finished build is no reason to spend rows on the section.
        let finished = stacked(&[bg(1, "./gradlew test", "passed", 30)]);
        assert!(finished.contains("the work"));
        assert!(!finished.contains("BACKGROUND"));

        let out = stacked(&[bg(1, "./gradlew test", "passed", 30), bg(2, "pnpm dev", "running", 5)]);
        assert!(out.contains("BACKGROUND"));
        assert!(out.contains("pnpm dev"));
        assert!(!out.contains("gradlew"));
        // Under the tasks, not beside them.
        let row = |needle: &str| out.lines().position(|l| l.contains(needle)).unwrap();
        assert!(row("BACKGROUND") > row("the work"));
    }

    #[test]
    fn an_epic_with_nothing_left_to_do_leaves_the_rail() {
        let tasks = vec![
            task("ENG-1-1", "ENG-1", State::Done, None, "shipped"),
            task("ENG-1-2", "ENG-1", State::Dropped, None, "not needed"),
            task("ENG-2-1", "ENG-2", State::Done, None, "half of it"),
            task("ENG-2-2", "ENG-2", State::Queued, None, "the other half"),
        ];
        let out = drawn(&tasks, &[], None, 40, 16);
        assert!(!out.contains("ENG-1 "), "finished epic still shown:\n{out}");
        assert!(out.contains("ENG-2"), "{out}");
        assert!(out.contains("half of it"), "an open epic keeps its done tasks:\n{out}");
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
