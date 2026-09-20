//! Dragging over an agent's terminal to copy what it said.
//!
//! Capturing the mouse takes the terminal's own selection away — there is no
//! way to have both, and no way to hand a selection to the clipboard on the
//! terminal's behalf. So a program that captures the mouse either gives it
//! back or does the selecting itself. This is the second.
//!
//! It holds two points in the mirrored screen's own coordinates, not the
//! screen's: the pane scrolls and is redrawn constantly, and a selection
//! anchored to a cell on the display would slide off whatever it was on.

use ratatui::layout::Rect;
use ratatui::style::Modifier;

/// A cell in the mirrored terminal: row then column, both from its top left.
pub type At = (u16, u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Where the drag began. Stays put while the other end moves.
    anchor: At,
    cursor: At,
}

impl Selection {
    pub fn start(at: At) -> Selection {
        Selection {
            anchor: at,
            cursor: at,
        }
    }

    pub fn drag_to(&mut self, at: At) {
        self.cursor = at;
    }

    /// The two ends in reading order, whichever way the drag went.
    pub fn range(&self) -> (At, At) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// Whether anything is actually selected. A click is a drag of nothing,
    /// and must clear the last selection rather than copy one character.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }

    /// What the selection covers, read out of the mirrored screen.
    ///
    /// The end column is inclusive on screen — the cell under the pointer is
    /// selected, as everywhere else — and exclusive to vt100, hence the + 1.
    pub fn text(&self, screen: &vt100::Screen) -> String {
        let ((r1, c1), (r2, c2)) = self.range();
        screen.contents_between(r1, c1, r2, c2.saturating_add(1))
    }

    /// Mark the selected cells in whatever has already been drawn there.
    ///
    /// Reversed rather than a colour of ours: the pane underneath is an
    /// agent's own output in its own palette, and a selection has to read as
    /// selected over all of it.
    pub fn highlight(&self, buf: &mut ratatui::buffer::Buffer, area: Rect) {
        let ((r1, c1), (r2, c2)) = self.range();
        for row in r1..=r2 {
            if row >= area.height {
                break;
            }
            let from = if row == r1 { c1 } else { 0 };
            let to = if row == r2 { c2 } else { area.width - 1 };
            for col in from..=to.min(area.width.saturating_sub(1)) {
                if let Some(cell) = buf.cell_mut((area.x + col, area.y + row)) {
                    cell.modifier |= Modifier::REVERSED;
                }
            }
        }
    }
}

/// Which cell of `area` a screen position landed on, if it is inside it.
pub fn cell_at(area: Rect, column: u16, row: u16) -> Option<At> {
    let inside = column >= area.x
        && column < area.x + area.width
        && row >= area.y
        && row < area.y + area.height;
    inside.then(|| (row - area.y, column - area.x))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(lines: &[&str]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(6, 20, 0);
        parser.process(lines.join("\r\n").as_bytes());
        parser
    }

    #[test]
    fn a_drag_reads_the_same_either_way_round() {
        let parser = screen(&["hello world", "second line"]);
        let mut down = Selection::start((0, 0));
        down.drag_to((1, 5));
        let mut up = Selection::start((1, 5));
        up.drag_to((0, 0));
        assert_eq!(down.range(), up.range());
        assert_eq!(down.text(parser.screen()), up.text(parser.screen()));
    }

    #[test]
    fn it_takes_the_cell_under_the_pointer_with_it() {
        let parser = screen(&["hello world"]);
        let mut sel = Selection::start((0, 0));
        // Through to the second 'l': six cells, not five.
        sel.drag_to((0, 5));
        assert_eq!(sel.text(parser.screen()), "hello ");
    }

    #[test]
    fn a_selection_over_several_lines_keeps_the_line_breaks() {
        let parser = screen(&["first", "second", "third"]);
        let mut sel = Selection::start((0, 0));
        sel.drag_to((2, 4));
        assert_eq!(sel.text(parser.screen()), "first\nsecond\nthird");
    }

    #[test]
    fn a_click_selects_nothing_at_all() {
        // Otherwise every click on the pane would put one character on the
        // clipboard, silently replacing whatever was there.
        let sel = Selection::start((3, 7));
        assert!(sel.is_empty());
    }

    #[test]
    fn the_highlight_covers_the_cells_and_stops_at_the_pane_edge() {
        let area = Rect::new(2, 1, 8, 3);
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 14, 6));
        let mut sel = Selection::start((0, 3));
        sel.drag_to((1, 2));
        sel.highlight(&mut buf, area);

        // Columns relative to the pane, the way the selection counts them.
        let on = |col: u16, row: u16| {
            buf.cell((area.x + col, area.y + row))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        };
        assert!(on(3, 0), "the first cell of the drag");
        assert!(on(7, 0), "to the end of its row");
        assert!(on(0, 1), "and on from the start of the next");
        assert!(on(2, 1), "up to where the pointer is");
        assert!(!on(3, 1), "but no further");
        assert!(!on(2, 0), "nor before where it began");
        assert!(
            !buf.cell((area.x - 1, area.y))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED),
            "and never outside the pane"
        );
    }

    #[test]
    fn a_position_outside_the_pane_is_not_a_cell_in_it() {
        let area = Rect::new(4, 2, 10, 5);
        assert_eq!(cell_at(area, 4, 2), Some((0, 0)));
        assert_eq!(cell_at(area, 13, 6), Some((4, 9)));
        assert_eq!(cell_at(area, 3, 2), None);
        assert_eq!(cell_at(area, 14, 2), None);
        assert_eq!(cell_at(area, 4, 7), None);
    }
}
