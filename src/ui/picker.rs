//! Choosing a repository to start an agent in.
//!
//! A filter over the repositories under the workspace root, rather than a
//! path to type. The whole point of the key is not opening a terminal by
//! hand, and making someone type `~/Code/acme/service/billing-service`
//! gives most of that back.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::agent::Candidate;
use crate::ui::theme;

pub struct Picker {
    all: Vec<Candidate>,
    filter: String,
    selected: usize,
}

impl Picker {
    pub fn new(all: Vec<Candidate>) -> Picker {
        Picker {
            all,
            filter: String::new(),
            selected: 0,
        }
    }

    /// Matches, best first.
    ///
    /// A subsequence match, so "cosv" finds "billing-service" — the point of
    /// a filter is to stop typing early, not to type the name accurately.
    /// Ranked, because a loose match on its own is not enough: "r" is a
    /// subsequence of almost every name, and the one starting with it is
    /// nearly always the one meant.
    pub fn matches(&self) -> Vec<&Candidate> {
        if self.filter.is_empty() {
            return self.all.iter().collect();
        }
        let needle = self.filter.to_lowercase();
        let mut scored: Vec<(u8, &Candidate)> = self
            .all
            .iter()
            .filter_map(|c| {
                let name = c.name.to_lowercase();
                let rank = if name.starts_with(&needle) {
                    0
                } else if name.contains(&needle) {
                    1
                } else if subsequence(&needle, &name) {
                    2
                } else {
                    return None;
                };
                Some((rank, c))
            })
            .collect();
        // Stable, so equally ranked names keep the order they were found in.
        scored.sort_by_key(|(rank, _)| *rank);
        scored.into_iter().map(|(_, c)| c).collect()
    }

    pub fn chosen(&self) -> Option<&Candidate> {
        self.matches().get(self.selected).copied()
    }

    pub fn push(&mut self, c: char) {
        self.filter.push(c);
        // Narrowing the list can strand the cursor past the end of it.
        self.selected = 0;
    }

    pub fn backspace(&mut self) {
        self.filter.pop();
        self.selected = 0;
    }

    pub fn move_by(&mut self, delta: isize) {
        let len = self.matches().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let last = len as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let width = 52.min(area.width.saturating_sub(4));
        let height = 16.min(area.height.saturating_sub(4));
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

        let matches = self.matches();
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    " start an agent in",
                    theme::label(),
                )),
                Line::from(vec![
                    Span::styled(" › ", theme::accent()),
                    Span::styled(self.filter.clone(), Style::default().fg(theme::TEXT)),
                    Span::styled("█", theme::accent()),
                ]),
            ]),
            head,
        );

        let mut lines = Vec::new();
        if matches.is_empty() {
            lines.push(Line::from(Span::styled(
                "  no repository matches",
                theme::faint(),
            )));
        }
        let room = list.height as usize;
        let first = self.selected.saturating_sub(room.saturating_sub(1));
        for (i, c) in matches.iter().enumerate().skip(first).take(room) {
            let is_selected = i == self.selected;
            let mut spans = vec![
                Span::styled(if is_selected { " ▌" } else { "  " }, theme::accent()),
                Span::styled(
                    c.name.clone(),
                    if is_selected {
                        Style::default().fg(theme::TEXT)
                    } else {
                        theme::dim()
                    },
                ),
            ];
            if c.taken {
                // Not hidden: a second agent in one repo is a normal thing to
                // want, and saying so is better than silently allowing it.
                spans.push(Span::styled("  has an agent", theme::faint()));
            }
            let style = if is_selected {
                theme::selected()
            } else {
                Style::default()
            };
            lines.push(Line::from(spans).style(style));
        }
        frame.render_widget(Paragraph::new(lines), list);

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ↑↓", theme::dim()),
                Span::styled(" pick   ", theme::faint()),
                Span::styled("↵", theme::dim()),
                Span::styled(" start   ", theme::faint()),
                Span::styled("esc", theme::dim()),
                Span::styled(" cancel", theme::faint()),
            ])),
            foot,
        );
    }
}

fn centre(area: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// Is `needle` a subsequence of `haystack`?
fn subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|n| chars.any(|h| h == n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn picker() -> Picker {
        Picker::new(
            ["billing-service", "storefront", "workspace", "gateway"]
                .iter()
                .map(|n| Candidate {
                    path: PathBuf::from(format!("/repo/{n}")),
                    name: n.to_string(),
                    taken: *n == "storefront",
                })
                .collect(),
        )
    }

    #[test]
    fn a_name_starting_with_the_filter_outranks_a_loose_match() {
        let mut p = picker();
        p.push('r');
        let names: Vec<_> = p.matches().iter().map(|c| c.name.clone()).collect();
        assert_eq!(
            names.first().map(String::as_str),
            Some("storefront"),
            "every one of these contains an r somewhere: {names:?}"
        );
    }

    #[test]
    fn filtering_matches_a_subsequence_not_just_a_prefix() {
        let mut p = picker();
        for c in "cosv".chars() {
            p.push(c);
        }
        let names: Vec<_> = p.matches().iter().map(|c| c.name.clone()).collect();
        assert_eq!(names, vec!["billing-service"]);

        p.backspace();
        p.backspace();
        p.backspace();
        p.backspace();
        assert_eq!(p.matches().len(), 4, "an empty filter matches everything");
    }

    #[test]
    fn narrowing_the_list_does_not_strand_the_cursor_off_the_end() {
        let mut p = picker();
        p.move_by(3);
        assert_eq!(p.chosen().unwrap().name, "gateway");

        p.push('r');
        assert_eq!(
            p.chosen().map(|c| c.name.clone()),
            Some("storefront".into()),
            "the selection returns to the top of the narrowed list"
        );
    }

    #[test]
    fn a_filter_matching_nothing_chooses_nothing() {
        let mut p = picker();
        for c in "zzz".chars() {
            p.push(c);
        }
        assert!(p.matches().is_empty());
        assert!(p.chosen().is_none(), "Enter here must not start anything");

        p.move_by(1);
        assert!(p.chosen().is_none(), "and moving in an empty list is safe");
    }

    #[test]
    fn it_draws_the_repositories_and_marks_the_ones_already_taken() {
        let p = picker();
        let mut term = Terminal::new(TestBackend::new(70, 20)).unwrap();
        term.draw(|f| p.render(f, f.area())).unwrap();
        let out = format!("{}", term.backend());

        assert!(out.contains("start an agent in"), "{out}");
        assert!(out.contains("billing-service"), "{out}");
        assert!(out.contains("has an agent"), "storefront is marked: {out}");
        assert!(out.contains("cancel"), "the way out is shown: {out}");
    }

    #[test]
    fn it_draws_in_a_terminal_too_small_for_it_without_panicking() {
        let p = picker();
        for (w, h) in [(20, 6), (30, 8), (200, 60)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| p.render(f, f.area())).unwrap();
        }
    }
}
