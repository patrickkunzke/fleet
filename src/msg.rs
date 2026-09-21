//! Delivering a message to the agent it is addressed to.
//!
//! `fleet board msg` used to only write the event: the flow graph drew the
//! edge, and the recipient never learned anything. The board is the record
//! and the message is the interrupt, and there was no interrupt.
//!
//! Delivery is into the recipient's tmux pane, because that is the one
//! channel fleet already owns end to end. Claude Code has a messaging socket
//! of its own, but it is undocumented and a protocol we would be guessing
//! at; a pane is what fleet opened and what it can always reach.
//!
//! Best-effort on purpose. A message to an agent that has died, or was never
//! fleet's to begin with, is still worth recording — so delivery failing
//! never fails the write.

use anyhow::Result;

use crate::tmux::Tmux;

/// Past this, a message is a document and belongs on the board. What is
/// delivered says where the rest of it is rather than pasting a page into
/// somebody's prompt.
const LIMIT: usize = 1500;

/// The one line a message arrives as.
///
/// One line because `send-keys` types it and a newline in the middle would
/// submit half of it. The board keeps the original; this is the knock at the
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

/// Type the message into the recipient's pane. False when there is no such
/// pane — the agent has gone, or was never one of ours.
pub fn deliver(tmux: &Tmux, target: &str, text: &str) -> Result<bool> {
    // A tmux that cannot be asked — no server running, because every agent
    // has exited — is the same answer as a pane that is not there: nobody
    // was knocked at. Only a send that fails against a pane we did find is
    // worth reporting as an error.
    let Ok(Some(pane)) = tmux.find(target) else {
        return Ok(false);
    };
    tmux.send_line(&pane, text)?;
    Ok(true)
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
        // send-keys types what it is given, so a newline in the middle would
        // submit half a message.
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

    #[test]
    fn it_arrives_in_the_pane_as_one_prompt() {
        use crate::tmux::Tmux;
        use std::time::Duration;

        let name = format!("fleet-msg-{}", std::process::id());
        let Ok(tmux) = Tmux::detect(Some(&name)) else { return };
        let tmux = tmux.on_socket(&name);

        let dir = tempfile::tempdir().unwrap();
        let landed = dir.path().join("got");
        // Stands in for an agent waiting at its prompt.
        let pane = tmux.spawn(
            "listener",
            dir.path(),
            &format!("sh -c 'head -n 1 > {}'", landed.display()),
        );
        let Ok(pane) = pane else {
            let _ = tmux.kill_server();
            return;
        };
        std::thread::sleep(Duration::from_millis(400));

        let text = line("accounts-svc", Some("ENG-2553-2"), "the parameter is yours", None);
        let target = format!("{}:{}", pane.session, pane.window_name);
        let sent = deliver(&tmux, &target, &text);

        for _ in 0..40 {
            if std::fs::read_to_string(&landed).is_ok_and(|s| !s.is_empty()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let got = std::fs::read_to_string(&landed).unwrap_or_default();
        let _ = tmux.kill_server();

        assert!(matches!(sent, Ok(true)), "{sent:?}");
        assert_eq!(got.trim_end(), text, "the message did not arrive intact");
    }

    #[test]
    fn a_pane_that_is_not_there_is_not_an_error() {
        // The agent died, or was never one of ours. The event is still
        // written; only the knock is lost.
        let name = format!("fleet-msg-gone-{}", std::process::id());
        let Ok(tmux) = Tmux::detect(Some(&name)) else { return };
        let tmux = tmux.on_socket(&name);
        assert!(matches!(deliver(&tmux, "nowhere:nothing", "hello"), Ok(false)));
        let _ = tmux.kill_server();
    }
}
