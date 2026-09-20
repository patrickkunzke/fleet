//! Getting text out of fleet and into the system clipboard.
//!
//! Two routes, because neither covers everything. OSC 52 is an escape
//! sequence the terminal itself acts on, so it works over SSH and needs no
//! program on this machine — but Terminal.app ignores it, and tmux swallows
//! it unless it has been told not to. `pbcopy` is the opposite: local only,
//! and always right when it is there.
//!
//! So both are tried, and the same text lands on the same clipboard twice
//! rather than sometimes not at all.

use std::io::Write;
use std::process::{Command, Stdio};

/// Put `text` on the clipboard. Reports which route worked, for the status
/// line — "copied" with nothing behind it is the failure people notice only
/// when they paste.
pub fn copy(text: &str) -> bool {
    let local = pbcopy(text);
    let escape = osc52(text);
    local || escape
}

fn pbcopy(text: &str) -> bool {
    let Ok(mut child) = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take()
        && stdin.write_all(text.as_bytes()).is_err()
    {
        return false;
    }
    matches!(child.wait(), Ok(status) if status.success())
}

/// Ask the terminal to set the clipboard itself.
fn osc52(text: &str) -> bool {
    // Terminals cap what they will accept and drop the whole sequence when
    // it is too long, so a selection past the limit is not sent at all —
    // half a clipboard is worse than none.
    const LIMIT: usize = 100_000;
    if text.len() > LIMIT {
        return false;
    }
    let mut out = std::io::stdout();
    let sent = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes())).is_ok();
    sent && out.flush().is_ok()
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let at = |shift: u32| ALPHABET[(n >> shift & 63) as usize] as char;
        out.push(at(18));
        out.push(at(12));
        out.push(if chunk.len() > 1 { at(6) } else { '=' });
        out.push(if chunk.len() > 2 { at(0) } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_examples_every_implementation_is_checked_against() {
        // RFC 4648's own vectors, padding included: a terminal given a
        // sequence it cannot decode sets the clipboard to nothing and says
        // nothing about it.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn it_encodes_what_an_agent_actually_prints() {
        // Not ASCII: box drawing, an arrow, an accent.
        let text = "⏺ Edit V12__shared.sql  +7 −0  ✔ café";
        let encoded = base64(text.as_bytes());
        assert!(encoded.chars().all(|c| c.is_ascii_alphanumeric()
            || c == '+'
            || c == '/'
            || c == '='));
        assert_eq!(encoded.len() % 4, 0, "a decoder reads it four at a time");
    }

    #[test]
    fn a_selection_too_large_for_the_escape_route_is_not_half_sent() {
        assert!(!osc52(&"x".repeat(200_000)));
    }
}
