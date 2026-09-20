//! The palette, in one place.
//!
//! A warm near-black ground with clay as the only brand accent; teal, amber
//! and red mean something rather than decorate. Kept together so the panes
//! cannot drift apart as they are written.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

/// Breathing room inside a pane. One column read as cramped against the
/// borders; two is what makes a column of text look placed rather than
/// wedged.
pub const GUTTER: u16 = 2;

/// Space between the app and the edges of the terminal it is running in.
/// Without it everything reads as pasted into the corner.
pub const MARGIN_X: u16 = 2;
pub const MARGIN_Y: u16 = 1;

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
pub fn selected() -> Style {
    Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
}
