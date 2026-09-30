//! How the agents are talking to each other.
//!
//! Two views of one thing, because they answer different questions. The graph
//! answers "who is talking to whom, and how much" at a glance and says
//! nothing about order. The log answers "what actually happened, in what
//! order" and says nothing about shape. Neither is a superset of the other,
//! so both exist and `g` and `l` switch between them.
//!
//! Both read the events table, which is written by every state change and by
//! `fleet board msg`. A message sent with SendMessage and never logged does not
//! appear here — the board is the record, and an agent that does not report
//! is invisible to it by construction.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::db::Event;
use crate::ui::crew::Row;
use crate::ui::{graph, theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Graph,
    Log,
}

#[allow(clippy::too_many_arguments)]
pub fn render(
    frame: &mut Frame,
    area: Rect,
    view: View,
    events: &[Event],
    ages: &[f64],
    rows: &[Row],
    selected: usize,
    agent: Option<&str>,
    bordered: bool,
) -> Rect {
    let block = Block::default()
        .borders(if bordered { Borders::RIGHT } else { Borders::NONE })
        .border_style(Style::default().fg(theme::BORDER));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Header, then the rule that closes it — the same two rows the session
    // pane uses, so switching views does not move the content or drop a
    // divider the eye was following across the frame.
    let [head, head_rule, body] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);

    let what = match view {
        View::Graph => "topology",
        View::Log => "chronological",
    };
    let head_inner = theme::pad(head);
    frame.render_widget(
        Paragraph::new(theme::spread(
            vec![
                Span::styled("◈ ", theme::accent()),
                Span::styled(
                    "flow",
                    Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(what, theme::faint()),
            ],
            vec![Span::styled(
                format!("{} events", events.len()),
                theme::faint(),
            )],
            head_inner.width,
        )),
        head_inner,
    );
    theme::rule(frame, head_rule);

    // Where the graph went, for a click to be read against. Nothing in the
    // log is clicked.
    match view {
        View::Graph => {
            let at = theme::pad(body);
            graph::render(frame.buffer_mut(), at, rows, events, ages, agent);
            at
        }
        View::Log => {
            log(frame, body, events, selected);
            Rect::ZERO
        }
    }
}

fn log(frame: &mut Frame, area: Rect, events: &[Event], selected: usize) {
    // The detail half is fixed: a message worth reading is worth reading in
    // full, and a list that grows until the body has one line is no use.
    let detail_height = 8.min(area.height / 2).max(3);
    let [list, detail] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(detail_height)]).areas(area);

    if events.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("nothing has happened yet", theme::faint()))),
            theme::pad(list),
        );
        return;
    }

    let room = list.height as usize;
    let first = selected.saturating_sub(room.saturating_sub(1));
    // Fixed columns, so the summaries line up. The route is clipped to its
    // width rather than merely padded to it: "accounts-svc ▸ billing-svc" is
    // longer than the column, and padding alone lets it run into the summary.
    let inner = list.width.saturating_sub(theme::GUTTER * 2) as usize;
    // The route gives way in a narrow pane. At a fixed 24 it left nothing
    // for the summary, which is the half you are actually reading.
    let route_width = 24.min(inner / 3).max(8);
    // The trailing 1 is a space that has to exist even when the route fills
    // its column exactly, or a clipped route runs straight into the summary.
    let summary_width = inner.saturating_sub(1 + 5 + 1 + 1 + 1 + route_width + 1) as u16;
    let mut lines = Vec::new();
    for (i, e) in events.iter().enumerate().skip(first).take(room) {
        let is_selected = i == selected;
        let (glyph, colour) = match e.kind.as_str() {
            "message" => ("▸", theme::OK),
            "bg" => ("▪", theme::BUSY),
            "note" => ("·", theme::FAINT),
            _ => ("•", theme::DIM),
        };
        let (from, to) = principals(e);
        let route = match (from, to) {
            (Some(f), Some(t)) => format!("{f} ▸ {t}"),
            (Some(f), None) => f.to_string(),
            (None, Some(t)) => format!("▸ {t}"),
            (None, None) => String::new(),
        };
        let route = clip(&route, route_width as u16);
        lines.push(
            Line::from(vec![
                Span::styled(if is_selected { "▌" } else { " " }, theme::accent()),
                Span::styled(hhmm(&e.ts), theme::faint()),
                Span::raw(" "),
                Span::styled(glyph, Style::default().fg(colour)),
                Span::raw(" "),
                Span::styled(format!("{route:<route_width$} "), theme::dim()),
                Span::styled(
                    clip(&e.summary, summary_width),
                    if is_selected {
                        Style::default().fg(theme::TEXT)
                    } else {
                        theme::dim()
                    },
                ),
            ])
            .style(if is_selected {
                theme::selected()
            } else {
                Style::default()
            }),
        );
    }
    frame.render_widget(Paragraph::new(lines), theme::pad(list));

    let divider = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme::BORDER));
    let inner = divider.inner(detail);
    frame.render_widget(divider, detail);

    let Some(e) = events.get(selected) else { return };
    let width = inner.width.saturating_sub(theme::GUTTER * 2);

    // Most events are not one agent addressing another — every state change
    // and every background process has a sender and nobody at the other end.
    // Drawing the arrow anyway pointed each of them at an em dash, which
    // reads as a recipient whose name went missing.
    let arrow = Style::default().fg(theme::BORDER);
    let mut route = Vec::new();
    match principals(e) {
        (Some(f), Some(t)) => {
            route.push(Span::styled(f.to_string(), Style::default().fg(theme::OK)));
            route.push(Span::styled(" ──▶ ", arrow));
            route.push(Span::styled(t.to_string(), theme::accent()));
        }
        (Some(f), None) => route.push(Span::styled(f.to_string(), Style::default().fg(theme::OK))),
        // Addressed by the board rather than by an agent: the CLI queuing a
        // task, or a process whose owner has since been retired.
        (None, Some(t)) => {
            route.push(Span::styled("──▶ ", arrow));
            route.push(Span::styled(t.to_string(), theme::accent()));
        }
        (None, None) => {}
    }
    if let Some(task) = e.task_key.as_deref().filter(|s| !s.is_empty()) {
        if !route.is_empty() {
            route.push(Span::raw("  "));
        }
        route.push(Span::styled(task.to_string(), theme::faint()));
    }

    let mut body = vec![theme::spread(
        route,
        // Time only: the date is the same one, and a truncated timestamp
        // reads as a broken value rather than an abbreviated one.
        vec![Span::styled(
            e.ts.get(11..19).unwrap_or_default().to_string(),
            theme::faint(),
        )],
        width,
    )];
    body.push(Line::raw(""));
    // The summary is the one line; the body is what it would not fit.
    for line in wrap(e.body.as_deref().unwrap_or(&e.summary), width) {
        body.push(Line::from(Span::styled(
            line,
            Style::default().fg(theme::TEXT),
        )));
    }
    frame.render_widget(Paragraph::new(body), theme::pad(inner));
}

/// Who an event is from and who it is to, with a blank name counted as no
/// name. The two spellings reach the table from different writers — the shell
/// board wrote empty strings where the binary writes NULL — and a route drawn
/// from one of them would start with a space.
fn principals(e: &Event) -> (Option<&str>, Option<&str>) {
    fn some(s: &Option<String>) -> Option<&str> {
        s.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
    (some(&e.from_agent), some(&e.to_agent))
}

fn hhmm(ts: &str) -> String {
    ts.get(11..16).unwrap_or("     ").to_string()
}

fn clip(s: &str, width: u16) -> String {
    let width = width as usize;
    if s.chars().count() <= width {
        return s.to_string();
    }
    let cut: String = s.chars().take(width.saturating_sub(1)).collect();
    format!("{cut}…")
}

fn wrap(text: &str, width: u16) -> Vec<String> {
    let width = (width as usize).max(8);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if line.is_empty() {
            line.push_str(word);
        } else if line.chars().count() + 1 + word.chars().count() <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            out.push(std::mem::take(&mut line));
            line.push_str(word);
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::crew::{Presence, Role};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn event(kind: &str, from: &str, to: &str, summary: &str) -> Event {
        Event {
            ts: "2026-09-20T14:11:32.000Z".into(),
            kind: kind.into(),
            from_agent: (!from.is_empty()).then(|| from.to_string()),
            to_agent: (!to.is_empty()).then(|| to.to_string()),
            task_key: Some("ENG-2553-2".into()),
            summary: summary.into(),
            body: None,
        }
    }

    fn rows() -> Vec<Row> {
        ["chief", "accounts-svc", "billing-svc"]
            .iter()
            .enumerate()
            .map(|(i, n)| Row {
                name: n.to_string(),
                role: if i == 0 { Role::Chief } else { Role::Worker },
                repo: "repo".into(),
                presence: Presence::Working,
                detail: String::new(),
                bg_running: 0,
                uptime: None,
                session_id: None,
                branch: None,
                target: None,
                pid: None,
                asking: false,
            })
            .collect()
    }

    fn drawn(view: View, events: &[Event], selected: usize, w: u16, h: u16) -> String {
        let rows = rows();
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let ages = vec![f64::MAX; events.len()];
        term.draw(|f| { render(f, f.area(), view, events, &ages, &rows, selected, None, true); })
            .unwrap();
        format!("{}", term.backend())
    }

    #[test]
    fn the_header_is_closed_by_a_rule_in_both_views() {
        // Switching views must not drop a divider the eye was following across the frame.
        for view in [View::Graph, View::Log] {
            let out = drawn(view, &[event("message", "chief", "billing-svc", "go")], 0, 60, 14);
            let second = out.lines().nth(1).unwrap_or_default();
            assert!(
                second.contains("───"),
                "no rule under the {view:?} header: {out}"
            );
        }
    }

    #[test]
    fn an_event_with_nobody_at_the_far_end_draws_no_arrow() {
        // Every state change and every background process has a sender and
        // no recipient, which is most of the table. An arrow there pointed
        // at an em dash, reading as a name that had gone missing.
        let out = drawn(View::Log, &[event("bg", "accounts-svc", "", "started: gradlew")], 0, 60, 14);
        assert!(out.contains("accounts-svc"), "{out}");
        assert!(!out.contains("──▶"), "an arrow to nobody: {out}");
        assert!(!out.contains('—'), "an em dash standing in for a name: {out}");
    }

    #[test]
    fn an_event_from_nobody_says_so_by_leaving_the_column_empty() {
        // The board writes these itself: a task queued from the CLI.
        let out = drawn(View::Log, &[event("task", "", "", "queued: consume param")], 0, 60, 14);
        assert!(out.contains("queued: consume param"), "{out}");
        assert!(!out.contains("──▶"), "{out}");
        assert!(!out.contains('—'), "{out}");
    }

    #[test]
    fn a_message_still_shows_both_ends_and_the_arrow_between_them() {
        let out = drawn(View::Log, &[event("message", "chief", "accounts-svc", "take it")], 0, 70, 14);
        assert!(out.contains("chief"), "{out}");
        assert!(out.contains("──▶"), "a message is the case the arrow is for: {out}");
    }

    #[test]
    fn a_blank_name_counts_as_no_name() {
        // The shell board wrote empty strings where the binary writes NULL,
        // and one flow log reads both.
        let mut e = event("task", "", "", "done");
        e.from_agent = Some("  ".into());
        e.to_agent = Some(String::new());
        assert_eq!(principals(&e), (None, None));
    }

    #[test]
    fn the_log_shows_the_selected_event_in_full_underneath() {
        let mut long = event("message", "billing-svc", "chief", "blocked: needs the param");
        long.body = Some(
            "ENG-2553-2 needs the optional service param from !412 before \
             AccountClient.kt compiles."
                .into(),
        );
        let events = [event("message", "chief", "billing-svc", "start -2"), long];

        let out = drawn(View::Log, &events, 1, 70, 20);
        assert!(out.contains("billing-svc ▸ chief"), "the route: {out}");
        assert!(out.contains("AccountClient.kt"), "the body, not just the summary: {out}");
        assert!(out.contains('▌'), "the selection is marked: {out}");
    }

    #[test]
    fn a_long_route_is_clipped_rather_than_allowed_to_eat_the_summary() {
        let events = [event(
            "message",
            "accounts-service-worker",
            "billing-service-worker",
            "the param is named service",
        )];
        let out = drawn(View::Log, &events, 0, 70, 16);

        assert!(
            !out.contains("workerthe"),
            "the route must not run into the summary: {out}"
        );
        assert!(out.contains('…'), "and must say it was clipped: {out}");
    }

    #[test]
    fn an_event_with_no_body_falls_back_to_its_summary() {
        let events = [event("task", "accounts-svc", "", "done")];
        let out = drawn(View::Log, &events, 0, 70, 16);
        assert!(out.contains("done"), "{out}");
    }

    #[test]
    fn a_selection_past_the_end_does_not_panic_the_detail_half() {
        let events = [event("message", "chief", "billing-svc", "start")];
        let _ = drawn(View::Log, &events, 99, 70, 16);
    }

    #[test]
    fn both_views_draw_in_a_narrow_pane_without_panicking() {
        let events: Vec<_> = (0..30)
            .map(|i| event("message", "chief", "billing-svc", &format!("message {i}")))
            .collect();
        for (w, h) in [(24, 6), (40, 10), (120, 50)] {
            let _ = drawn(View::Graph, &events, 0, w, h);
            let _ = drawn(View::Log, &events, 5, w, h);
        }
    }
}
