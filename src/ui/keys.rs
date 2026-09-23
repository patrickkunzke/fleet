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
///
/// Every modifier survives the trip. The first version kept Ctrl and Alt on
/// letters and dropped them from everything else, so Option+Left — word
/// left, in every editor an agent has — arrived as a plain Left. The round
/// trip through tmux is only faithful if nothing is thrown away on the way
/// in.
pub fn translate(key: KeyEvent) -> Option<Key> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // tmux spells a modified key as its modifiers, then its name: C-M-Left.
    let named = |name: &str, with_shift: bool| {
        let mut out = String::new();
        if ctrl {
            out.push_str("C-");
        }
        if alt {
            out.push_str("M-");
        }
        if shift && with_shift {
            out.push_str("S-");
        }
        out.push_str(name);
        Some(Key::Named(out))
    };

    match key.code {
        KeyCode::Char(c) => {
            if !ctrl && !alt {
                return Some(Key::Literal(c.to_string()));
            }
            // Shift is already in a letter's case, so it is not said again.
            // tmux spells control keys lower case, and a space by name.
            let c = if ctrl { c.to_ascii_lowercase() } else { c };
            let name = if c == ' ' { "Space".to_string() } else { c.to_string() };
            named(&name, false)
        }
        // Shift+Enter is how an agent takes a newline without sending the
        // message. tmux cannot hand a shifted Enter to a program that has not
        // asked it for extended keys: it would arrive as Enter, and send. ESC
        // then CR is what Claude Code's own /terminal-setup makes a terminal
        // send for Shift+Enter, and what it reads as a newline.
        KeyCode::Enter if shift || alt => Some(Key::Named("M-Enter".into())),
        KeyCode::Enter => named("Enter", false),
        KeyCode::Esc => named("Escape", true),
        // Not "Backspace": tmux does not know that name.
        KeyCode::Backspace => named("BSpace", true),
        KeyCode::Tab => named("Tab", true),
        // Shift is what makes it a back-tab; saying it twice is S-BTab,
        // which tmux does not know.
        KeyCode::BackTab => {
            let mut out = String::new();
            if ctrl {
                out.push_str("C-");
            }
            if alt {
                out.push_str("M-");
            }
            out.push_str("BTab");
            Some(Key::Named(out))
        }
        KeyCode::Up => named("Up", true),
        KeyCode::Down => named("Down", true),
        KeyCode::Left => named("Left", true),
        KeyCode::Right => named("Right", true),
        KeyCode::Home => named("Home", true),
        KeyCode::End => named("End", true),
        KeyCode::PageUp => named("PPage", true),
        KeyCode::PageDown => named("NPage", true),
        KeyCode::Delete => named("DC", true),
        KeyCode::Insert => named("IC", true),
        KeyCode::F(n) => named(&format!("F{n}"), true),
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

    fn with(code: KeyCode, mods: KeyModifiers) -> Option<Key> {
        translate(KeyEvent::new(code, mods))
    }

    fn name(n: &str) -> Option<Key> {
        Some(Key::Named(n.into()))
    }

    #[test]
    fn word_movement_keeps_its_modifier() {
        // Option+Left arrived as a plain Left, and the cursor moved one
        // character instead of one word.
        assert_eq!(with(KeyCode::Left, KeyModifiers::ALT), name("M-Left"));
        assert_eq!(with(KeyCode::Right, KeyModifiers::ALT), name("M-Right"));
        assert_eq!(with(KeyCode::Left, KeyModifiers::CONTROL), name("C-Left"));
        assert_eq!(with(KeyCode::Left, KeyModifiers::SHIFT), name("S-Left"));
        assert_eq!(
            with(KeyCode::Right, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            name("C-S-Right")
        );
    }

    #[test]
    fn deleting_a_word_backwards_keeps_its_modifier() {
        assert_eq!(with(KeyCode::Backspace, KeyModifiers::ALT), name("M-BSpace"));
        assert_eq!(with(KeyCode::Backspace, KeyModifiers::CONTROL), name("C-BSpace"));
    }

    #[test]
    fn shift_enter_is_a_newline_not_a_send() {
        // tmux cannot pass a shifted Enter to a program that did not ask for
        // extended keys; plain Enter sends the message half-written.
        assert_eq!(with(KeyCode::Enter, KeyModifiers::SHIFT), name("M-Enter"));
        assert_eq!(with(KeyCode::Enter, KeyModifiers::ALT), name("M-Enter"));
        assert_eq!(translate(press(KeyCode::Enter)), name("Enter"));
    }

    #[test]
    fn control_and_alt_together_are_both_said() {
        // It returned on the first modifier it found, so Ctrl+Alt+b went out
        // as C-b.
        assert_eq!(
            with(KeyCode::Char('b'), KeyModifiers::CONTROL | KeyModifiers::ALT),
            name("C-M-b")
        );
    }

    #[test]
    fn a_modified_space_is_spelled_by_name() {
        assert_eq!(with(KeyCode::Char(' '), KeyModifiers::CONTROL), name("C-Space"));
        assert_eq!(with(KeyCode::Char(' '), KeyModifiers::ALT), name("M-Space"));
    }

    #[test]
    fn a_back_tab_is_not_shifted_twice() {
        assert_eq!(with(KeyCode::BackTab, KeyModifiers::SHIFT), name("BTab"));
    }

    #[test]
    fn a_keystroke_tmux_cannot_express_sends_nothing() {
        // Better than guessing and typing rubbish at a running agent.
        assert_eq!(translate(press(KeyCode::CapsLock)), None);
        assert_eq!(translate(press(KeyCode::Menu)), None);
    }
}
