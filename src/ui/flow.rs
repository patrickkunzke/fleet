//! How the agents are talking to each other.
//!
//! Two views of one thing, because they answer different questions. The graph
//! answers "who is talking to whom, and how much" at a glance and says
//! nothing about order. The log answers "what actually happened, in what
//! order" and says nothing about shape. Neither is a superset of the other,
//! so both exist and `g` and `l` switch between them.
//!
//! Both read the events table, which is written by every state change and by
//! `fleet msg`. A message sent with SendMessage and never logged does not
//! appear here — the board is the record, and an agent that does not report
//! is invisible to it by construction.

use std::collections::BTreeMap;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::db::Event;
use crate::ui::fleet::{Presence, Role, Row};
use crate::ui::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Graph,
    Log,
}

/// One direction of traffic between two agents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub count: usize,
}

/// Count who sent what to whom.
///
/// Only messages: a task moving to done is an event, but it is not one agent
/// telling another anything, and counting it as traffic would make the busiest
/// agent look like the most talkative one.
pub fn edges(events: &[Event]) -> Vec<Edge> {
    let mut tally: BTreeMap<(String, String), usize> = BTreeMap::new();
    for e in events.iter().filter(|e| e.kind == "message") {
        if let (Some(from), Some(to)) = (&e.from_agent, &e.to_agent)
            && !from.is_empty()
            && !to.is_empty()
        {
            *tally.entry((from.clone(), to.clone())).or_default() += 1;
        }
    }
    let mut edges: Vec<Edge> = tally
        .into_iter()
        .map(|((from, to), count)| Edge { from, to, count })
        .collect();
    edges.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.to.cmp(&b.to)));
    edges
}

pub fn render(
    frame: &mut Frame,
    area: Rect,
    view: View,
    events: &[Event],
    rows: &[Row],
    selected: usize,
    bordered: bool,
) {
    let block = Block::default()
        .borders(if bordered { Borders::RIGHT } else { Borders::NONE })
        .border_style(Style::default().fg(theme::BORDER));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [head, body] = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);

    let what = match view {
        View::Graph => "topology",
        View::Log => "chronological",
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("◈ ", theme::accent()),
            Span::styled(
                "flow",
                Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(what, theme::faint()),
            Span::raw("  "),
            Span::styled(format!("{} events", events.len()), theme::faint()),
        ])),
        theme::pad(head),
    );

    match view {
        View::Graph => graph(frame, theme::pad(body), events, rows),
        View::Log => log(frame, body, events, selected),
    }
}

fn graph(frame: &mut Frame, area: Rect, events: &[Event], rows: &[Row]) {
    let edges = edges(events);
    if edges.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("no messages logged yet", theme::faint())),
                Line::raw(""),
                Line::from(Span::styled(
                    "agents record them with: fleet msg <from> <to> …",
                    theme::faint(),
                )),
            ]),
            area,
        );
        return;
    }

    let chief = rows
        .iter()
        .find(|r| r.role == Role::Chief)
        .map(|r| r.name.clone())
        .unwrap_or_else(|| "chief".into());

    // Spokes off the chief, then anything that bypassed it. A hub drawn as a
    // column fits a narrow pane; boxes side by side do not.
    let mut spokes: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut peers: Vec<&Edge> = Vec::new();
    for edge in &edges {
        if edge.from == chief {
            spokes.entry(edge.to.clone()).or_default().0 += edge.count;
        } else if edge.to == chief {
            spokes.entry(edge.from.clone()).or_default().1 += edge.count;
        } else {
            peers.push(edge);
        }
    }

    let mut lines = vec![
        Line::from(vec![
            Span::styled(" ◆ ", theme::accent()),
            Span::styled(
                chief.clone(),
                Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  the one you brief", theme::faint()),
        ]),
        Line::from(Span::styled(" │", Style::default().fg(theme::BORDER))),
    ];

    let busiest = spokes.values().map(|(a, b)| a + b).max().unwrap_or(1).max(1);
    let last = spokes.len().saturating_sub(1);
    for (i, (name, (sent, received))) in spokes.iter().enumerate() {
        let elbow = if i == last { " └──▶ " } else { " ├──▶ " };
        let state = rows.iter().find(|r| &r.name == name);
        let (glyph, colour) = match state.map(|r| r.presence) {
            Some(Presence::Working) => ("●", theme::BUSY),
            Some(Presence::Waiting) => ("○", theme::OK),
            Some(Presence::Gone) => ("×", theme::FAINT),
            _ => ("·", theme::FAINT),
        };
        lines.push(Line::from(vec![
            Span::styled(elbow, Style::default().fg(theme::BORDER)),
            Span::styled(format!("{name:<16}"), Style::default().fg(theme::TEXT)),
            Span::styled(bar(sent + received, busiest), theme::accent()),
            Span::raw(" "),
            Span::styled(format!("{sent}▸ {received}◂"), theme::dim()),
            Span::raw("  "),
            Span::styled(glyph, Style::default().fg(colour)),
        ]));
    }

    if !peers.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "bypassing the chief",
            theme::label(),
        )));
        for edge in peers {
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(edge.from.clone(), theme::dim()),
                Span::styled(" ──▶ ", Style::default().fg(theme::BORDER)),
                Span::styled(format!("{:<16}", edge.to), theme::dim()),
                Span::styled(edge.count.to_string(), theme::faint()),
            ]));
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        " ▸ sent   ◂ received",
        theme::faint(),
    )));

    frame.render_widget(Paragraph::new(lines), area);
}

/// A width-4 bar, so volume is visible without reading the numbers.
fn bar(count: usize, busiest: usize) -> String {
    let filled = ((count * 4) as f64 / busiest as f64).round().clamp(0.0, 4.0) as usize;
    let filled = if count > 0 { filled.max(1) } else { 0 };
    format!("{}{}", "▇".repeat(filled), " ".repeat(4 - filled))
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
        let route = match (&e.from_agent, &e.to_agent) {
            (Some(f), Some(t)) if !t.is_empty() => format!("{f} ▸ {t}"),
            (Some(f), _) if !f.is_empty() => f.clone(),
            _ => String::new(),
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
    let mut body = vec![theme::spread(
        vec![
            Span::styled(
                e.from_agent.clone().unwrap_or_default(),
                Style::default().fg(theme::OK),
            ),
            Span::styled(" ──▶ ", Style::default().fg(theme::BORDER)),
            Span::styled(
                e.to_agent.clone().unwrap_or_else(|| "—".into()),
                theme::accent(),
            ),
            Span::raw("  "),
            Span::styled(e.task_key.clone().unwrap_or_default(), theme::faint()),
        ],
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
                tmux_target: None,
                pid: None,
            })
            .collect()
    }

    fn drawn(view: View, events: &[Event], selected: usize, w: u16, h: u16) -> String {
        let rows = rows();
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render(f, f.area(), view, events, &rows, selected, true))
            .unwrap();
        format!("{}", term.backend())
    }

    #[test]
    fn only_messages_count_as_traffic() {
        let events = [
            event("message", "chief", "billing-svc", "start -2"),
            event("message", "billing-svc", "chief", "blocked"),
            event("task", "billing-svc", "", "done"),
            event("bg", "billing-svc", "", "started: gradlew"),
        ];
        let edges = edges(&events);
        assert_eq!(edges.len(), 2, "a task moving is not one agent telling another: {edges:?}");
        assert!(edges.iter().all(|e| e.count == 1));
    }

    #[test]
    fn the_graph_separates_spokes_from_traffic_that_bypassed_the_chief() {
        let events = [
            event("message", "chief", "billing-svc", "start"),
            event("message", "chief", "billing-svc", "again"),
            event("message", "billing-svc", "chief", "blocked"),
            event("message", "accounts-svc", "billing-svc", "!412 is in"),
        ];
        let out = drawn(View::Graph, &events, 0, 60, 18);

        assert!(out.contains("◆ chief"), "{out}");
        assert!(out.contains("2▸ 1◂"), "sent and received are separate: {out}");
        assert!(out.contains("bypassing the chief"), "{out}");
        assert!(out.contains("accounts-svc ──▶"), "{out}");
    }

    #[test]
    fn an_empty_graph_says_how_messages_get_recorded() {
        let out = drawn(View::Graph, &[], 0, 60, 14);
        assert!(out.contains("no messages logged yet"), "{out}");
        assert!(out.contains("fleet msg"), "{out}");
    }

    #[test]
    fn the_bar_scales_to_the_busiest_and_never_hides_a_single_message() {
        assert_eq!(bar(0, 8), "    ");
        assert_eq!(bar(8, 8), "▇▇▇▇");
        assert_eq!(bar(4, 8), "▇▇  ");
        assert_eq!(
            bar(1, 100),
            "▇   ",
            "one message must still show, or a quiet edge looks like none"
        );
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
