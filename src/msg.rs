//! Delivering a message to the agent it is addressed to.
//!
//! `fleet board msg` used to only write the event: the flow graph drew the
//! edge, and the recipient never learned anything. The board is the record
//! and the message is the interrupt, and there was no interrupt.
//!
//! Delivery is herdr's `agent prompt` into the recipient's pane, which is
//! what fleet opened and what it can always reach. Claude Code has a
//! messaging socket of its own, but it is undocumented and a protocol we
//! would be guessing at.
//!
//! Best-effort on purpose. A message to an agent that has died, or was never
//! fleet's to begin with, is still worth recording — so delivery failing
//! never fails the write.

/// Past this, a message is a document and belongs on the board. What is
/// delivered says where the rest of it is rather than pasting a page into
/// somebody's prompt.
const LIMIT: usize = 1500;

/// The one line a message arrives as.
///
/// One line because it is typed at the agent's prompt, and a newline in the
/// middle would submit half of it. The board keeps the original; this is the knock at the
/// door.
pub fn line(from: &str, task: Option<&str>, summary: &str, body: Option<&str>) -> String {
    let mut text = summary.to_string();
    if let Some(body) = body.map(str::trim).filter(|b| !b.is_empty()) {
        text.push_str(" — ");
        text.push_str(body);
    }
    let text = flatten(&text);

    // Marked as the fleet's, so the agent does not read a peer's message as
    // something the person at the keyboard said.
    let who = match task {
        Some(key) => format!("[fleet · {from} · {key}]"),
        None => format!("[fleet · {from}]"),
    };
    let room = LIMIT.saturating_sub(who.chars().count() + 1);
    if text.chars().count() <= room {
        return format!("{who} {text}");
    }
    // Measured rather than guessed: the first version subtracted a length
    // the suffix did not have, and went over the limit it was enforcing.
    const REST: &str = "… (the rest is on the board)";
    let cut: String = text
        .chars()
        .take(room.saturating_sub(REST.chars().count()))
        .collect();
    format!("{who} {cut}{REST}")
}

/// Everything onto one line, with the runs of space that produces collapsed.
fn flatten(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_says_who_it_is_from_and_what_it_is_about() {
        let out = line("accounts-svc", Some("ENG-2553-2"), "column is in", None);
        assert_eq!(out, "[fleet · accounts-svc · ENG-2553-2] column is in");
    }

    #[test]
    fn a_message_about_nothing_in_particular_still_says_who_sent_it() {
        // Without the marker an agent reads a peer's message as something
        // the person at the keyboard typed.
        let out = line("chief", None, "stand down for now", None);
        assert_eq!(out, "[fleet · chief] stand down for now");
    }

    #[test]
    fn the_body_comes_with_it_on_the_same_line() {
        // It is typed at a prompt, so a newline in the middle would submit
        // half a message.
        let out = line(
            "chief",
            None,
            "take the column",
            Some("the shared flag\nlands first\n\nthen the parameter"),
        );
        assert!(!out.contains('\n'), "{out}");
        assert!(out.contains("the shared flag lands first then the parameter"), "{out}");
    }

    #[test]
    fn a_message_long_enough_to_be_a_document_is_left_on_the_board() {
        let out = line("chief", None, &"word ".repeat(2000), None);
        assert!(out.chars().count() <= LIMIT, "{}", out.chars().count());
        assert!(out.ends_with("(the rest is on the board)"), "{out}");
    }
}
