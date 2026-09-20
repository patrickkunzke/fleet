//! Turning a keystroke into something tmux will deliver.
//!
//! Typing goes straight to the agent rather than into a box of ours. Claude
//! Code has its own line editor — history, completion, paste, its own idea of
//! what an arrow key means — and a local input line would replace all of it
//! with a worse one.
//!
//! Two shapes come out of this. Printable text is sent literally, with
//! `send-keys -l`, so that a message containing "Enter" or "C-c" arrives as
//! those words. Everything else is sent as a tmux key name.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    /// Text, to be delivered exactly as written.
    Literal(String),
    /// A tmux key name: Enter, Escape, C-c, M-b, Up.
    Named(String),
}

/// What to send for a keystroke, or nothing when tmux has no way to say it.
pub fn translate(key: KeyEvent) -> Option<Key> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    let named = |n: &str| Some(Key::Named(n.to_string()));

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // tmux spells control keys C-x, always lower case.
                return named(&format!("C-{}", c.to_ascii_lowercase()));
            }
            if alt {
                return named(&format!("M-{c}"));
            }
            Some(Key::Literal(c.to_string()))
        }
        KeyCode::Enter => named("Enter"),
        KeyCode::Esc => named("Escape"),
        // Not "Backspace": tmux does not know that name.
        KeyCode::Backspace => named("BSpace"),
        KeyCode::Tab => named("Tab"),
        KeyCode::BackTab => named("BTab"),
        KeyCode::Up => named("Up"),
        KeyCode::Down => named("Down"),
        KeyCode::Left => named("Left"),
        KeyCode::Right => named("Right"),
        KeyCode::Home => named("Home"),
        KeyCode::End => named("End"),
        KeyCode::PageUp => named("PPage"),
        KeyCode::PageDown => named("NPage"),
        KeyCode::Delete => named("DC"),
        KeyCode::Insert => named("IC"),
        KeyCode::F(n) => named(&format!("F{n}")),
        // Modifier presses on their own, media keys, and the rest: nothing
        // useful to send, and guessing would type rubbish at an agent.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    #[test]
    fn printable_text_goes_literally() {
        assert_eq!(
            translate(press(KeyCode::Char('a'))),
            Some(Key::Literal("a".into()))
        );
        // A space is text, not the tmux key name "Space".
        assert_eq!(
            translate(press(KeyCode::Char(' '))),
            Some(Key::Literal(" ".into()))
        );
        assert_eq!(
            translate(press(KeyCode::Char('ü'))),
            Some(Key::Literal("ü".into()))
        );
    }

    #[test]
    fn control_and_alt_use_tmux_spelling() {
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Key::Named("C-c".into()))
        );
        // Shift-Ctrl-C arrives as an upper case char; tmux wants C-c.
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::CONTROL)),
            Some(Key::Named("C-c".into()))
        );
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT)),
            Some(Key::Named("M-b".into()))
        );
    }

    #[test]
    fn the_keys_an_agent_actually_needs_have_tmux_names() {
        for (code, name) in [
            (KeyCode::Enter, "Enter"),
            (KeyCode::Esc, "Escape"),
            (KeyCode::Backspace, "BSpace"),
            (KeyCode::Up, "Up"),
            (KeyCode::PageUp, "PPage"),
            (KeyCode::Delete, "DC"),
        ] {
            assert_eq!(translate(press(code)), Some(Key::Named(name.into())), "{name}");
        }
    }

    #[test]
    fn a_keystroke_tmux_cannot_express_sends_nothing() {
        // Better than guessing and typing rubbish at a running agent.
        assert_eq!(translate(press(KeyCode::CapsLock)), None);
        assert_eq!(translate(press(KeyCode::Menu)), None);
    }
}
