//! The palette, in one place.
//!
//! A warm near-black ground with clay as the only brand accent; teal, amber
//! and red mean something rather than decorate. Kept together so the panes
//! cannot drift apart as they are written.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// Breathing room inside a pane. Every pane carries its own, which is why
/// the app itself needs no outer margin: the rules then run wall to wall,
/// and a rule that stops short of the edge looks like a mistake.
pub const GUTTER: u16 = 2;


pub fn pad(area: Rect) -> Rect {
    Rect {
        x: area.x + GUTTER,
        y: area.y,
        width: area.width.saturating_sub(GUTTER * 2),
        height: area.height,
    }
}

/// Deliberately not a colour. The centre pane is a real terminal drawing on
/// the terminal's own background, and any ground we picked for the rails
/// would differ from it — so the app takes whatever the terminal is, and
/// only the foreground is ours.
pub const BG: Color = Color::Reset;
pub const BORDER: Color = Color::Rgb(0x2B, 0x27, 0x22);
pub const TEXT: Color = Color::Rgb(0xE8, 0xE3, 0xDA);
pub const DIM: Color = Color::Rgb(0x8B, 0x82, 0x78);
pub const FAINT: Color = Color::Rgb(0x5C, 0x55, 0x4D);
pub const ACCENT: Color = Color::Rgb(0xD9, 0x77, 0x57);
pub const OK: Color = Color::Rgb(0x7F, 0xB3, 0xA3);
pub const BUSY: Color = Color::Rgb(0xD9, 0xA7, 0x5B);

pub fn base() -> Style {
    Style::default().fg(TEXT).bg(BG)
}

pub fn dim() -> Style {
    Style::default().fg(DIM)
}

pub fn faint() -> Style {
    Style::default().fg(FAINT)
}

pub fn accent() -> Style {
    Style::default().fg(ACCENT)
}

/// Section labels: FLEET, TASKS, BACKGROUND.
pub fn label() -> Style {
    Style::default().fg(FAINT).add_modifier(Modifier::BOLD)
}

/// Selection is a bar and brighter text, never a filled background: a panel
/// colour that suits one terminal theme is wrong in the next.
/// One line with something pushed to each edge.
///
/// The design puts every count and every state against the right-hand edge
/// so the eye can run down a column of them. Done by measuring rather than
/// by a second right-aligned widget, which would blank the left half.
pub fn spread<'a>(
    left: Vec<Span<'a>>,
    right: Vec<Span<'a>>,
    width: u16,
) -> ratatui::text::Line<'a> {
    let width = width as usize;
    let right_width: usize = right.iter().map(|s| s.width()).sum();

    // No room for both: the right-hand side is the state — scrolled back,
    // typing here — and losing it silently is worse than losing the tail of
    // a name, so the left gives way.
    if right_width + 1 >= width {
        return ratatui::text::Line::from(right);
    }

    let budget = width - right_width - 1;
    let mut spans = Vec::new();
    let mut used = 0usize;
    for span in left {
        let w = span.width();
        if used + w <= budget {
            used += w;
            spans.push(span);
            continue;
        }
        let room = budget.saturating_sub(used);
        if room > 1 {
            let cut: String = span.content.chars().take(room - 1).collect();
            used += room;
            spans.push(Span::styled(format!("{cut}…"), span.style));
        }
        break;
    }

    spans.push(Span::raw(" ".repeat(width - used - right_width)));
    spans.extend(right);
    ratatui::text::Line::from(spans)
}

/// Join the rules to the dividers they cross.
///
/// Drawn separately, a rule and a divider land next to each other as `─│─`:
/// adjacent, but visibly not connected. This walks the divider columns and
/// swaps in the junction the neighbours ask for.
///
/// Only the divider columns are touched, and `theirs` marks the area an
/// agent draws into. Without that exclusion, a `───` an agent printed sits
/// next to a divider and drags it into a junction with something that is
/// not one of our rules at all.
pub fn join(
    buf: &mut ratatui::buffer::Buffer,
    columns: &[u16],
    top: u16,
    bottom: u16,
    // Cells belonging to an agent rather than to us. A rule an agent printed
    // must not pull a divider into a junction with it.
    theirs: Rect,
) {
    const VERTICAL: [char; 6] = ['│', '├', '┤', '┬', '┴', '┼'];
    const HORIZONTAL: [char; 6] = ['─', '├', '┤', '┬', '┴', '┼'];

    let at = |buf: &ratatui::buffer::Buffer, x: u16, y: u16| -> char {
        buf.cell((x, y))
            .and_then(|c| c.symbol().chars().next())
            .unwrap_or(' ')
    };

    for &x in columns {
        for y in top..bottom {
            let here = at(buf, x, y);
            if !VERTICAL.contains(&here) && here != '─' {
                continue;
            }
            let up = y > top && VERTICAL.contains(&at(buf, x, y - 1));
            let down = y + 1 < bottom && VERTICAL.contains(&at(buf, x, y + 1));
            let ours = |x: u16, y: u16| {
                !(x >= theirs.x
                    && x < theirs.x + theirs.width
                    && y >= theirs.y
                    && y < theirs.y + theirs.height)
            };
            let left = x > 0 && ours(x - 1, y) && HORIZONTAL.contains(&at(buf, x - 1, y));
            let right = ours(x + 1, y) && HORIZONTAL.contains(&at(buf, x + 1, y));

            let glyph = match (up, down, left, right) {
                (true, true, true, true) => '┼',
                (true, true, true, false) => '┤',
                (true, true, false, true) => '├',
                (false, true, true, true) => '┬',
                (true, false, true, true) => '┴',
                (true, true, false, false) => '│',
                (false, true, false, true) => '┌',
                (false, true, true, false) => '┐',
                (true, false, false, true) => '└',
                (true, false, true, false) => '┘',
                _ => continue,
            };
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(glyph);
            }
        }
    }
}

/// A horizontal rule across an area one row high.
pub fn rule(frame: &mut ratatui::Frame, area: Rect) {
    frame.render_widget(
        ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::TOP)
            .border_style(Style::default().fg(BORDER)),
        area,
    );
}

pub fn selected() -> Style {
    Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(left: &str, right: &str, width: u16) -> String {
        spread(
            vec![Span::raw(left.to_string())],
            vec![Span::raw(right.to_string())],
            width,
        )
        .to_string()
    }

    fn buffer(rows: &[&str]) -> ratatui::buffer::Buffer {
        ratatui::buffer::Buffer::with_lines(rows.to_vec())
    }

    fn row(buf: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect()
    }

    #[test]
    fn a_rule_crossing_a_divider_becomes_a_junction() {
        let mut buf = buffer(&["──────", "  │   ", "  │   ", "──────"]);
        join(&mut buf, &[2], 0, 4, Rect::ZERO);

        assert_eq!(row(&buf, 0, 6), "──┬───", "the divider starts below the rule");
        assert_eq!(row(&buf, 3, 6), "──┴───", "and ends above this one");
    }

    #[test]
    fn a_rule_an_agent_printed_does_not_pull_the_divider_into_it() {
        // The right half is the agent's pane; the ─ in it is its own.
        let mut buf = buffer(&["  │───", "  │───", "  │───"]);
        join(&mut buf, &[2], 0, 3, Rect::new(3, 0, 3, 3));

        assert_eq!(
            row(&buf, 1, 6),
            "  │───",
            "the divider stays a divider"
        );
    }

    #[test]
    fn it_pushes_the_two_sides_to_the_edges() {
        assert_eq!(line("fleet", "4 agents", 20), "fleet       4 agents");
        // Exactly full: a space is kept even at the cost of a character,
        // because two values butted together read as one.
        assert_eq!(line("fleet", "4 agents", 13), "fle… 4 agents");
    }

    #[test]
    fn a_line_too_narrow_for_both_gives_up_the_left_not_the_right() {
        // The right side carries the state; truncating the name is the
        // cheaper loss, and it says that it was truncated.
        let out = line("billing-service", "typing here", 20);
        assert!(out.ends_with("typing here"), "{out}");
        assert!(out.contains('…'), "{out}");
        assert_eq!(out.chars().count(), 20, "{out}");
    }

    #[test]
    fn a_line_with_no_room_at_all_keeps_the_state() {
        assert_eq!(line("billing-service", "typing here", 8), "typing here");
    }
}
