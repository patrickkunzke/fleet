//! Agents as sessions of the Claude desktop app, in its sidebar under the
//! chief: the host a user turns on with `FLEET_HOST=desktop` or
//! `fleet spawn --host desktop`.
//!
//! fleet cannot start such a session. Only the chief's model can, with the
//! app's `start_session` tool, and the app allows it only on a turn that the
//! user's own message started (docs/spikes/desktop-sidebar.md). So
//! `fleet spawn --host desktop` does the board's half: it puts the agent on
//! the board and prints what the chief passes to `start_session`. The
//! chief then records the `local_…` id the app gives back as the agent's
//! target, `desktop:local_<id>`, and the worker joins the board itself with
//! fleet's `mcp__fleet__join` tool, which records its CLI session id.
//!
//! The two ids are different. The desktop id is for the app's own tools:
//! `SendMessage`, `stop_session`, `archive_session`. The CLI id is what the
//! worker's mod checks in with, as every fleet session does.
//!
//! Messages to a desktop worker go through the chief's model: the board
//! records them, and fleet tells the sender to deliver them with
//! `SendMessage` to the `local_…` id. A mod's own send is refused in auto
//! mode, and the worker's mailbox stays for background workers.

use std::path::Path;

use serde::Serialize;

use crate::brief::Brief;

pub const PREFIX: &str = "desktop:";

/// Whether `target` is a desktop session's.
pub fn is_desktop(target: &str) -> bool {
    target.starts_with(PREFIX)
}

/// The app's id for the session at `target`, once the chief has recorded
/// it: `local_…`.
pub fn local_id(target: &str) -> Option<&str> {
    target.strip_prefix(PREFIX).filter(|id| !id.is_empty())
}

/// Whether the user asked for desktop sessions: `--host desktop`, or else
/// `FLEET_HOST=desktop`.
pub fn wanted(flag: Option<&str>) -> anyhow::Result<bool> {
    let env = std::env::var("FLEET_HOST").unwrap_or_default();
    match flag.unwrap_or(env.as_str()) {
        "desktop" => Ok(true),
        "" | "herdr" | "background" => Ok(false),
        other => anyhow::bail!("host '{other}': herdr, background or desktop"),
    }
}

/// What the chief passes to the app's `start_session` for one worker.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct StartSession {
    pub initiation: &'static str,
    pub cwd: String,
    pub title: String,
    pub name: String,
    pub prompt: String,
    pub background: String,
}

/// The `start_session` arguments for agent `name` in `repo`, briefed with
/// `brief`. `board` and `run` are what it joins.
pub fn start_session(
    name: &str,
    repo: &Path,
    brief: &Brief,
    board: &Path,
    run: i64,
    waits_on: &[String],
) -> StartSession {
    let title = if waits_on.is_empty() {
        format!("fleet: {name}")
    } else {
        format!("fleet: {name} (waiting on {})", waits_on.join(", "))
    };
    StartSession {
        initiation: "user_asked",
        cwd: repo.to_string_lossy().into_owned(),
        title: title.chars().take(59).collect(),
        // The app takes [a-z0-9-], at most 24.
        name: session_name(name),
        prompt: prompt(name, brief, board, run),
        background: format!(
            "fleet (github.com/patrickkunzke/fleet) runs a chief of staff session that plans \
             work across several repositories and delegates one task per repository to a \
             worker session. This session is the worker `{name}`, in {}. The chief \
             coordinates through a shared board; the brief says how to join it.",
            repo.display()
        ),
    }
}

/// The worker's first prompt. The app wraps it, so it cannot start with a
/// slash command: the join is a tool the brief asks for first. The role
/// comes with it, since a desktop session has no system prompt of fleet's.
fn prompt(name: &str, brief: &Brief, board: &Path, run: i64) -> String {
    format!(
        "Before anything else, call the tool `mcp__fleet__join` with board \"{}\", run {run} \
         and name \"{name}\" (load it with ToolSearch if it is deferred). It puts this session \
         on the fleet's board and gives it the board's tools. If the tool is missing, the \
         fleet plugin is not installed here: say so and stop. In this desktop app, \
         the chief's messages, its go-ahead included, reach you as a message from the chief's \
         session, with the same `[fleet · chief · …]` marker.\n\n{}\n\n{}",
        board.display(),
        brief.role,
        brief.opening
    )
}

/// A name the app takes for a session: `[a-z0-9-]`, at most 24.
pub fn session_name(name: &str) -> String {
    let mut out: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(24);
    out
}

/// What the sender of a message to a desktop worker is told: fleet cannot
/// reach it, the sender's model can.
pub fn deliver_yourself(to: &str, target: &str, text: &str) -> String {
    match local_id(target) {
        Some(id) => format!(
            "on the board; {to} is a desktop session, so deliver it yourself: \
             SendMessage with to \"{id}\" and this message:\n{text}"
        ),
        None => format!(
            "on the board; {to} is a desktop session that has not been recorded yet. \
             Once `fleet board agent {to} --target desktop:<local_id>` has its id, send it \
             with SendMessage"
        ),
    }
}

/// What retiring a desktop worker leaves for the chief to do.
pub fn close_hint(target: &str) -> String {
    match local_id(target) {
        Some(id) => format!(
            "it is a desktop session: stop it with stop_session and archive it with \
             archive_session, session_id \"{id}\""
        ),
        None => "it is a desktop session whose id was never recorded: close it in the app".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brief() -> Brief {
        Brief {
            role: "You are `api`.".into(),
            opening: "Your task is T-1.".into(),
            deny: &[],
            allow: &[],
        }
    }

    #[test]
    fn a_target_carries_the_apps_id() {
        assert!(is_desktop("desktop:local_ab12"));
        assert_eq!(local_id("desktop:local_ab12"), Some("local_ab12"));
        assert_eq!(
            local_id("desktop:"),
            None,
            "recorded before the app gave an id"
        );
        assert!(!is_desktop("bg:ab12"));
    }

    #[test]
    fn the_start_is_the_users_in_the_workers_repo() {
        let s = start_session(
            "api",
            Path::new("/r/api"),
            &brief(),
            Path::new("/b/fleet.db"),
            7,
            &[],
        );
        assert_eq!(s.initiation, "user_asked");
        assert_eq!(s.cwd, "/r/api");
        assert_eq!(s.title, "fleet: api");
        assert_eq!(s.name, "api");
    }

    #[test]
    fn the_brief_asks_for_the_join_first_and_carries_the_role() {
        // The app wraps the prompt, so a slash command never runs: a tool does.
        let s = start_session(
            "api",
            Path::new("/r/api"),
            &brief(),
            Path::new("/b/fleet.db"),
            7,
            &[],
        );
        assert!(
            s.prompt
                .starts_with("Before anything else, call the tool `mcp__fleet__join`"),
            "{}",
            s.prompt
        );
        assert!(
            s.prompt
                .contains("board \"/b/fleet.db\", run 7 and name \"api\""),
            "{}",
            s.prompt
        );
        assert!(s.prompt.contains("You are `api`."));
        assert!(s.prompt.ends_with("Your task is T-1."));
    }

    #[test]
    fn a_worker_that_waits_says_so_in_the_sidebar() {
        let s = start_session(
            "web",
            Path::new("/r/web"),
            &brief(),
            Path::new("/b"),
            1,
            &["T-1".into()],
        );
        assert_eq!(s.title, "fleet: web (waiting on T-1)");
    }

    #[test]
    fn a_name_is_one_the_app_takes() {
        assert_eq!(session_name("Billing_Service.v2"), "billing-service-v2");
        assert_eq!(session_name(&"a".repeat(40)).len(), 24);
    }

    #[test]
    fn a_message_is_left_for_the_senders_model() {
        let said = deliver_yourself("api", "desktop:local_ab12", "[fleet · chief] go");
        assert!(
            said.contains("SendMessage with to \"local_ab12\""),
            "{said}"
        );
        assert!(said.ends_with("[fleet · chief] go"), "{said}");
    }

    #[test]
    fn the_host_is_asked_for() {
        assert!(wanted(Some("desktop")).unwrap());
        assert!(!wanted(Some("background")).unwrap());
        assert!(wanted(Some("tmux")).is_err());
    }
}
