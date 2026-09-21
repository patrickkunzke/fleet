//! What an agent is told when it starts.
//!
//! Fleet could start sessions and watch them, and that was the whole of it:
//! every pane opened at an empty prompt, and whoever wanted work done typed
//! the brief in themselves. The board already knew — the role, the repo, the
//! task, what it waits on — and none of it reached the agent.
//!
//! The brief goes in as Claude Code's own first prompt, `claude '<brief>'`,
//! rather than being typed into the pane once it is up. Typing means waiting
//! for a REPL that has not said it is ready, and half a prompt delivered to
//! an agent mid-start is worse than none.

use std::path::Path;

use crate::db::Task;

/// The session you talk to. It plans, it dispatches, and it is the only one
/// that writes tasks.
pub fn chief(root: &Path) -> String {
    let where_ = root.display();
    format!(
        "You are the chief of staff of a fleet of Claude Code sessions working \
         across the repositories under {where_}.\n\n\
         Use the board skill for everything shared: `fleet board --help`. The \
         board is what the other agents read, so anything they must act on \
         goes there rather than staying in this conversation.\n\n\
         Your job is to plan and delegate, not to do the work. Break an ask \
         into one task per repository with `fleet board add`, record what \
         blocks what with `--dep`, and start an agent for each ready task with \
         `fleet spawn <name> --repo <path> --task <key>` — that agent opens \
         already briefed on its task, so you do not have to repeat it.\n\n\
         Begin by running `fleet board ls` and telling me where things stand, \
         then ask what I want to work on. Be brief."
    )
}

/// A worker, and what it is for.
pub fn worker(name: &str, repo: &Path, task: Option<&Task>, body: Option<&str>) -> String {
    let here = repo.display();
    let mut out = format!(
        "You are `{name}`, one agent of a fleet working across several \
         repositories. Yours is {here}; stay in it — another agent has each of \
         the others, and two agents editing one repo is how the fleet breaks.\n\n\
         Use the board skill to report: `fleet board --help`. The board is how \
         the chief of staff and the other agents see what you are doing, so \
         state changes and blockers go there as they happen, not at the end.\n\n"
    );

    let Some(task) = task else {
        out.push_str(&format!(
            "You have no task yet. Run `fleet board agent {name}` to see \
             whether one has been assigned, and `fleet board ready` for what \
             is unblocked. If neither has anything for you, say so and wait."
        ));
        return out;
    };

    out.push_str(&format!("Your task is {} — {}.\n", task.key, task.title));
    if let Some(body) = body.map(str::trim).filter(|b| !b.is_empty()) {
        out.push_str(&format!("\n{body}\n"));
    }
    if !task.waiting_on.is_empty() {
        // Said outright, because an agent that starts work on a blocked task
        // writes code against an interface that does not exist yet.
        out.push_str(&format!(
            "\nIt waits on {}. Do not start until those are done — check with \
             `fleet board ls`. You can read the code and plan in the meantime.\n",
            task.waiting_on.join(", ")
        ));
    }
    out.push_str(&format!(
        "\nMark it running with `fleet board start {}` when you begin, and \
         `fleet board done {}` when it is finished. If you are blocked, \
         `fleet board block {} --reason ...` and tell the chief with \
         `fleet msg {} chief '...'`, because the board is the record and the \
         message is what interrupts them.\n\n\
         Start by reading enough of the repository to say back what you intend \
         to do, then wait for me. Be brief.",
        task.key, task.key, task.key, name
    ));
    out
}

/// `claude '<brief>'` — the agent starts with this as its first prompt.
///
/// Single quotes, because the brief is prose with apostrophes, backticks and
/// newlines in it, and a shell would otherwise read some of it as commands.
pub fn command(brief: &str) -> String {
    format!("claude '{}'", brief.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::State;

    fn task(key: &str, waiting_on: &[&str]) -> Task {
        Task {
            key: key.into(),
            title: "consume the parameter".into(),
            state: State::Queued,
            repo: "/w/billing-service".into(),
            epic_key: Some("ENG-2553".into()),
            agent: None,
            mr_url: None,
            blocked_on: None,
            waiting_on: waiting_on.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn the_chief_is_told_it_dispatches_rather_than_does() {
        let brief = chief(Path::new("/w"));
        assert!(brief.contains("chief of staff"));
        assert!(brief.contains("/w"), "the workspace it covers");
        assert!(brief.contains("fleet spawn"), "how to delegate");
        assert!(brief.contains("plan and delegate, not to do the work"));
    }

    #[test]
    fn a_worker_is_told_its_task_and_where_it_may_work() {
        let t = task("ENG-2553-2", &[]);
        let brief = worker("billing-svc", Path::new("/w/billing-service"), Some(&t), None);
        assert!(brief.contains("`billing-svc`"));
        assert!(brief.contains("/w/billing-service"));
        assert!(brief.contains("ENG-2553-2"));
        assert!(brief.contains("consume the parameter"));
        assert!(brief.contains("fleet board done ENG-2553-2"));
    }

    #[test]
    fn a_blocked_task_says_so_before_the_agent_starts_on_it() {
        // An agent that begins a blocked task writes code against an
        // interface that does not exist yet.
        let t = task("ENG-2553-2", &["ENG-2553-1", "ENG-2553-3"]);
        let brief = worker("billing-svc", Path::new("/w"), Some(&t), None);
        assert!(brief.contains("waits on ENG-2553-1, ENG-2553-3"), "{brief}");
        assert!(brief.contains("Do not start"));
    }

    #[test]
    fn an_agent_with_no_task_is_told_how_to_find_one() {
        let brief = worker("storefront", Path::new("/w/storefront"), None, None);
        assert!(brief.contains("no task yet"));
        assert!(brief.contains("fleet board agent storefront"));
        assert!(!brief.contains("Mark it running"), "there is nothing to mark");
    }

    #[test]
    fn the_body_of_a_task_reaches_the_agent() {
        let t = task("ENG-2553-2", &[]);
        let brief = worker("c", Path::new("/w"), Some(&t), Some("  the old header is the fallback  "));
        assert!(brief.contains("the old header is the fallback"));
        // Not the surrounding whitespace, which is an artefact of however it
        // was written to the board.
        assert!(!brief.contains("  the old header"));
    }

    #[test]
    fn a_brief_with_quotes_in_it_survives_the_shell() {
        // Apostrophes are ordinary in prose and fatal in a single-quoted
        // shell word; the whole rest of the brief would be read as commands.
        let out = command("don't stop; rm -rf / # 'nested'");
        assert!(out.starts_with("claude '"));
        assert!(out.ends_with('\''));
        assert_eq!(
            out,
            r"claude 'don'\''t stop; rm -rf / # '\''nested'\'''"
        );
    }

    #[test]
    fn the_brief_arrives_at_the_agent_through_tmux_intact() {
        use crate::tmux::Tmux;
        use std::time::Duration;

        // The whole path this feature rests on: our quoting, tmux's own
        // parsing of the command it is handed, and the shell in the pane.
        // Three layers, each of which would happily eat an apostrophe.
        let name = format!("fleet-brief-{}", std::process::id());
        let Ok(tmux) = Tmux::detect(Some(&name)) else { return };
        let tmux = tmux.on_socket(&name);

        let dir = tempfile::tempdir().unwrap();
        let landed = dir.path().join("argv");
        // A stand-in for Claude Code that records the prompt it was given.
        let fake = dir.path().join("claude");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\nprintf %s \"$1\" > {}\n", landed.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let t = task("ENG-2553-2", &["ENG-2553-1"]);
        let expected = worker("billing-svc", dir.path(), Some(&t), Some("the old header is the fallback"));
        let command = format!(
            "PATH={}:$PATH {}",
            dir.path().display(),
            command(&expected)
        );

        let spawned = tmux.spawn("briefed", dir.path(), &command);
        assert!(spawned.is_ok(), "{spawned:?}");
        for _ in 0..40 {
            if landed.is_file() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let got = std::fs::read_to_string(&landed).unwrap_or_default();
        let _ = tmux.kill_server();

        assert_eq!(got, expected, "the brief did not survive the trip");
        assert!(got.contains("ENG-2553-2"), "and it is the right brief");
    }

    #[test]
    fn the_shell_reads_a_quoted_brief_back_as_one_word() {
        // The real check: hand it to a shell and see what the program gets.
        let brief = chief(Path::new("/w/it's here"));
        let quoted = command(&brief);
        let printed = quoted.replacen("claude ", "printf %s ", 1);
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&printed)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), brief);
    }
}
