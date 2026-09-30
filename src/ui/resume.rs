//! Picking an earlier run to bring back.
//!
//! Shown when fleet starts in a workspace with nothing running in it and
//! something to come back to, and on `r` at any time. A run is the crew
//! that worked together — the chief and the agents it started, each with the
//! conversation it had — so choosing one brings all of them back at once,
//! each into its own session.

use std::path::Path;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::agent;
use crate::db::Run;
use crate::ui::{graph, theme};

/// What a run offers: who would come back, and who could not.
pub struct Offer {
    pub run: Run,
    /// Would come back: not retired, with a conversation still on disk.
    pub back: Vec<String>,
    /// Would not: their conversation is gone from disk.
    pub lost: Vec<String>,
}

impl Offer {
    /// What can be brought back of `run`, or nothing if none of it can.
    pub fn of(run: Run) -> Option<Offer> {
        let mut back = Vec::new();
        let mut lost = Vec::new();
        for a in run.resumable() {
            let session = a.session_id.as_deref().unwrap_or_default();
            if agent::conversation_exists(Path::new(&a.repo), session) {
                back.push(a.name.clone());
            } else {
                lost.push(a.name.clone());
            }
        }
        (!back.is_empty()).then_some(Offer { run, back, lost })
    }
}

pub enum Choice {
    Run(usize),
    Fresh,
}

pub struct ResumePicker {
    pub offers: Vec<Offer>,
    selected: usize,
    /// The run fleet is already in, if it is in one: resuming it brings back
    /// whichever of its agents have died, and it is labelled so.
    current: Option<i64>,
}

impl ResumePicker {
    pub fn new(offers: Vec<Offer>, current: Option<i64>) -> ResumePicker {
        ResumePicker {
            offers,
            selected: 0,
            current,
        }
    }

    /// Runs, then "start fresh" as the last entry.
    fn len(&self) -> usize {
        self.offers.len() + 1
    }

    pub fn move_by(&mut self, delta: isize) {
        let last = self.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    pub fn choice(&self) -> Choice {
        if self.selected < self.offers.len() {
            Choice::Run(self.selected)
        } else {
            Choice::Fresh
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, now: f64) {
        let width = 64.min(area.width.saturating_sub(4));
        let wanted = 4 + self.offers.len() as u16 * 3 + 2;
        let height = wanted.min(area.height.saturating_sub(2)).max(8);
        let box_area = centre(area, width, height);

        // Blank what is underneath: an overlay drawn over live text is
        // unreadable the moment either of them has colour.
        frame.render_widget(Clear, box_area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::ACCENT))
            .style(Style::default().bg(theme::BG));
        let inner = block.inner(box_area);
        frame.render_widget(block, box_area);

        let [head, list, foot] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(inner);

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " pick up where you left off",
                theme::label(),
            ))),
            head,
        );

        let text_w = inner.width.saturating_sub(4) as usize;
        let mut lines = Vec::new();
        for (i, offer) in self.offers.iter().enumerate() {
            let is_selected = i == self.selected;
            let bar = if is_selected { " ▌ " } else { "   " };
            let when = ago(now, &offer.run.last_active);
            let others = offer.back.len() - usize::from(offer.back.iter().any(|n| n == "chief"));
            let crew = match (offer.back.iter().any(|n| n == "chief"), others) {
                (true, 0) => "the chief".to_string(),
                (true, n) => format!("the chief + {n} agent{}", if n == 1 { "" } else { "s" }),
                (false, n) => format!("{n} agent{}", if n == 1 { "" } else { "s" }),
            };
            let mut head = vec![
                Span::styled(bar, theme::accent()),
                Span::styled(
                    format!("{when} · {crew}"),
                    if is_selected {
                        theme::selected()
                    } else {
                        Style::default().fg(theme::TEXT)
                    },
                ),
            ];
            if self.current == Some(offer.run.id) {
                head.push(Span::styled("  this run", theme::faint()));
            }
            lines.push(Line::from(head));

            // What it was about, then who is in it: the tasks tell two runs
            // apart better than the names do, which repeat.
            let mut about = offer.run.tasks.join(", ");
            if about.is_empty() {
                about = "no tasks on the board".into();
            }
            lines.push(Line::from(vec![
                Span::styled(if is_selected { " ▌   " } else { "     " }, theme::accent()),
                Span::styled(clip(&about, text_w), theme::dim()),
            ]));
            let mut who = offer.back.join(", ");
            if !offer.lost.is_empty() {
                who.push_str(&format!(" — {} gone from disk", offer.lost.join(", ")));
            }
            lines.push(Line::from(vec![
                Span::styled(if is_selected { " ▌   " } else { "     " }, theme::accent()),
                Span::styled(clip(&who, text_w), theme::faint()),
            ]));
        }
        let fresh_selected = self.selected == self.offers.len();
        lines.push(Line::from(vec![
            Span::styled(if fresh_selected { " ▌ " } else { "   " }, theme::accent()),
            Span::styled(
                "start fresh — a new chief, nothing brought back",
                if fresh_selected {
                    theme::selected()
                } else {
                    theme::dim()
                },
            ),
        ]));

        // Keep the selection on screen: three lines a run.
        let room = list.height as usize;
        let at = (self.selected * 3).min(lines.len().saturating_sub(1));
        let first = (at + 3).saturating_sub(room);
        frame.render_widget(
            Paragraph::new(lines.into_iter().skip(first).collect::<Vec<_>>()),
            list,
        );

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ↑↓", theme::dim()),
                Span::styled(" pick   ", theme::faint()),
                Span::styled("↵", theme::dim()),
                Span::styled(" bring it back   ", theme::faint()),
                Span::styled("esc", theme::dim()),
                Span::styled(" decide later", theme::faint()),
            ])),
            foot,
        );
    }
}

/// "12m ago", "3h ago", "yesterday", "5d ago" — how long since a board
/// timestamp.
pub fn ago(now: f64, ts: &str) -> String {
    let Some(then) = graph::epoch(ts) else {
        return ts.to_string();
    };
    let secs = (now - then).max(0.0) as u64;
    match secs {
        s if s < 90 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 2 * 86_400 => "yesterday".into(),
        s => format!("{}d ago", s / 86_400),
    }
}

fn clip(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let cut: String = s.chars().take(width.saturating_sub(1)).collect();
    format!("{cut}…")
}

fn centre(area: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Run, RunAgent};
    use ratatui::backend::TestBackend;

    fn run(id: i64, names: &[&str], tasks: &[&str]) -> Run {
        Run {
            id,
            root: "/w".into(),
            started_at: "2026-09-24T09:00:00Z".into(),
            last_active: "2026-09-24T10:00:00.000Z".into(),
            agents: names
                .iter()
                .map(|n| RunAgent {
                    name: n.to_string(),
                    role: if *n == "chief" { "chief" } else { "worker" }.into(),
                    repo: "/w".into(),
                    session_id: Some(format!("s-{n}")),
                    retired: false,
                })
                .collect(),
            tasks: tasks.iter().map(|t| t.to_string()).collect(),
        }
    }

    fn offer(r: Run) -> Offer {
        let back = r.agents.iter().map(|a| a.name.clone()).collect();
        Offer { run: r, back, lost: vec![] }
    }

    fn drawn(p: &ResumePicker, now: f64) -> String {
        let mut term = ratatui::Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| p.render(f, f.area(), now)).unwrap();
        format!("{}", term.backend())
    }

    #[test]
    fn each_run_says_when_who_and_what_it_was_about() {
        let now = graph::epoch("2026-09-24T13:00:00Z").unwrap();
        let p = ResumePicker::new(
            vec![offer(run(2, &["chief", "eng-2155", "eng-2155-review"], &["ENG-2155-fixes", "ENG-1682"]))],
            None,
        );
        let out = drawn(&p, now);
        assert!(out.contains("3h ago · the chief + 2 agents"), "{out}");
        assert!(out.contains("ENG-2155-fixes, ENG-1682"), "what it was about:\n{out}");
        assert!(out.contains("eng-2155-review"), "who comes back:\n{out}");
        assert!(out.contains("start fresh"), "and the way out:\n{out}");
    }

    #[test]
    fn an_agent_whose_conversation_is_gone_is_named_not_quietly_dropped() {
        let mut o = offer(run(1, &["chief"], &[]));
        o.lost = vec!["storefront".into()];
        let p = ResumePicker::new(vec![o], None);
        let out = drawn(&p, graph::epoch("2026-09-24T10:05:00Z").unwrap());
        assert!(out.contains("storefront gone from disk"), "{out}");
    }

    #[test]
    fn the_last_entry_is_starting_fresh() {
        let mut p = ResumePicker::new(vec![offer(run(1, &["chief"], &[]))], None);
        assert!(matches!(p.choice(), Choice::Run(0)));
        p.move_by(1);
        assert!(matches!(p.choice(), Choice::Fresh));
        p.move_by(5);
        assert!(matches!(p.choice(), Choice::Fresh), "and nothing past it");
    }

    #[test]
    fn the_run_fleet_is_already_in_says_so() {
        let p = ResumePicker::new(vec![offer(run(7, &["chief"], &[]))], Some(7));
        let out = drawn(&p, graph::epoch("2026-09-24T10:00:30Z").unwrap());
        assert!(out.contains("this run"), "{out}");
    }

    #[test]
    fn how_long_ago_reads_the_way_people_say_it() {
        let at = |t: &str| graph::epoch(t).unwrap();
        let now = at("2026-09-24T12:00:00Z");
        assert_eq!(ago(now, "2026-09-24T11:59:30Z"), "just now");
        assert_eq!(ago(now, "2026-09-24T11:48:00Z"), "12m ago");
        assert_eq!(ago(now, "2026-09-24T09:00:00Z"), "3h ago");
        assert_eq!(ago(now, "2026-09-23T09:00:00Z"), "yesterday");
        assert_eq!(ago(now, "2026-09-19T09:00:00Z"), "5d ago");
    }
}
