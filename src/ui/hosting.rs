//! The fleet tab's line to herdr, kept off the thread that draws.
//!
//! Three things, all only when fleet runs in herdr:
//!
//! - **Following herdr's agents.** herdr knows each agent's state the moment
//!   it changes — working, at a dialog, waiting — and says so on its event
//!   stream. [`follow`] listens and hands the view the current list after
//!   every change, so the rail and herdr's sidebar never disagree, and a
//!   session id is picked up as soon as herdr has one.
//! - **What herdr's sidebar says about each agent.** In place of the agent
//!   kind (`claude`), the task it holds and where that stands, so the
//!   sidebar answers "who is on what" without the fleet tab.
//! - **Notifications** for what only the board knows: a task blocked, ready
//!   for review, done. An agent stopped at a dialog herdr signals itself.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use crate::db::{Event, State, Task};
use crate::herdr::{Agent, Herdr};

/// Changes arriving closer together than this are one re-read. A working
/// agent retitles its pane with every spinner frame.
const SETTLE: Duration = Duration::from_millis(250);

/// How long a sidebar label lives unless reported again, and how often it
/// is. When fleet stops, the labels go within this long.
const LABEL_TTL: Duration = Duration::from_secs(600);
const LABEL_REFRESH: Duration = Duration::from_secs(240);

/// Follow herdr's agents for as long as `send` accepts them.
///
/// The list once at the start, and again after every change. The stream is
/// reopened whenever an agent appears or a pane goes, because herdr reports
/// an agent's state changes only to a subscription that named its pane; and
/// after anything that breaks it — a restarting server comes back as a new
/// socket at the same path.
pub fn follow(herdr: Herdr, send: impl Fn(Vec<Agent>) -> bool + Send + 'static) {
    std::thread::spawn(move || {
        loop {
            let Ok(agents) = herdr.agents() else {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            };
            let panes: Vec<String> = agents.iter().map(|a| a.pane_id.clone()).collect();
            if !send(agents) {
                return;
            }
            let fetch = || herdr.agents().ok();
            match listen(&herdr, &panes, &fetch, &send) {
                Ok(Listen::Again) => {}
                Ok(Listen::Stop) => return,
                Err(_) => std::thread::sleep(Duration::from_secs(2)),
            }
        }
    });
}

enum Listen {
    /// The set of panes changed: subscribe again, naming the new ones.
    Again,
    /// Nobody is listening any more.
    Stop,
}

fn subscription(panes: &[String]) -> String {
    let mut subs: Vec<serde_json::Value> = ["pane.agent_detected", "pane.updated", "pane.closed", "pane.exited"]
        .iter()
        .map(|t| serde_json::json!({ "type": t }))
        .collect();
    subs.extend(
        panes
            .iter()
            .map(|p| serde_json::json!({ "type": "pane.agent_status_changed", "pane_id": p })),
    );
    serde_json::json!({
        "id": "fleet",
        "method": "events.subscribe",
        "params": { "subscriptions": subs },
    })
    .to_string()
}

/// What an event on the stream means for the list of agents.
#[derive(Debug, PartialEq)]
enum Change {
    /// An agent came or went, or a pane closed: resubscribe.
    Panes,
    /// Something about an agent changed: read the list again.
    State,
    /// The subscription's own acknowledgement, and anything unknown.
    Nothing,
}

fn classify(line: &str) -> Change {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Change::Nothing;
    };
    match v["event"].as_str().unwrap_or("") {
        "pane_agent_detected" | "pane.agent_detected" | "pane.closed" | "pane.exited" => Change::Panes,
        "pane.agent_status_changed" | "pane.updated" => Change::State,
        _ => Change::Nothing,
    }
}

fn listen(
    herdr: &Herdr,
    panes: &[String],
    fetch: &dyn Fn() -> Option<Vec<Agent>>,
    send: &dyn Fn(Vec<Agent>) -> bool,
) -> std::io::Result<Listen> {
    let mut stream = UnixStream::connect(herdr.socket())?;
    stream.write_all(format!("{}\n", subscription(panes)).as_bytes())?;
    stream.set_read_timeout(Some(SETTLE))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut pending = false;
    loop {
        match reader.read_line(&mut line) {
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(_) => {
                let change = classify(line.trim());
                line.clear();
                match change {
                    Change::Panes => {
                        return Ok(Listen::Again);
                    }
                    Change::State => pending = true,
                    Change::Nothing => {}
                }
            }
            // Quiet for a moment: whatever arrived before it is one re-read.
            // A line cut off by the timeout stays in `line` and is finished
            // by the next read.
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if pending {
                    pending = false;
                    if let Some(agents) = fetch()
                        && !send(agents)
                    {
                        return Ok(Listen::Stop);
                    }
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// Work for herdr that the view hands off.
pub enum Job {
    /// Every agent's sidebar label, by pane. Only changed ones are sent on,
    /// and the rest again before they expire.
    Labels(Vec<(String, String)>),
    Notify { title: String, body: String },
}

/// Start the thread that does [`Job`]s.
pub fn worker(herdr: Herdr) -> Sender<Job> {
    let (tx, rx) = channel();
    std::thread::spawn(move || work(herdr, rx));
    tx
}

fn work(herdr: Herdr, rx: Receiver<Job>) {
    let mut sent: HashMap<String, (String, Instant)> = HashMap::new();
    let mut wanted: Vec<(String, String)> = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Job::Labels(labels)) => wanted = labels,
            Ok(Job::Notify { title, body }) => {
                let _ = herdr.notify(&title, &body);
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let now = Instant::now();
        for (pane, text) in &wanted {
            let fresh = sent
                .get(pane)
                .is_some_and(|(was, at)| was == text && now.duration_since(*at) < LABEL_REFRESH);
            if !fresh && herdr.report_display(pane, text, LABEL_TTL).is_ok() {
                sent.insert(pane.clone(), (text.clone(), now));
            }
        }
        sent.retain(|pane, _| wanted.iter().any(|(p, _)| p == pane));
    }
}

/// The chief's label: the board at a glance.
pub fn chief_label(tasks: &[Task]) -> String {
    let count = |s: State| tasks.iter().filter(|t| t.state == s).count();
    let open = tasks
        .iter()
        .filter(|t| !matches!(t.state, State::Done | State::Dropped))
        .count();
    let mut parts = vec![if open == 0 {
        "nothing open".to_string()
    } else {
        format!("{open} open")
    }];
    for (state, word) in [(State::Blocked, "blocked"), (State::Review, "in review")] {
        let n = count(state);
        if n > 0 {
            parts.push(format!("{n} {word}"));
        }
    }
    parts.join(" · ")
}

/// A worker's label: its task, and where that stands.
pub fn worker_label(name: &str, tasks: &[Task], awaiting_go: Option<&str>) -> String {
    // Said before anything else: nothing moves until someone answers it.
    if let Some(key) = awaiting_go {
        return clip(&format!("{key} · needs a go"), 72);
    }
    // The one it is on, before one it has finished.
    let mine = tasks
        .iter()
        .filter(|t| t.agent.as_deref() == Some(name))
        .min_by_key(|t| match t.state {
            State::Running | State::Blocked => 0,
            State::Review => 1,
            State::Queued => 2,
            State::Done | State::Dropped => 3,
        });
    let Some(t) = mine.filter(|t| !matches!(t.state, State::Done | State::Dropped)) else {
        return "no task".into();
    };
    let tail = match t.state {
        State::Blocked => format!("blocked: {}", t.blocked_on.as_deref().unwrap_or("?")),
        State::Review => "review".into(),
        State::Queued if !t.waiting_on.is_empty() => format!("waits on {}", t.waiting_on.join(", ")),
        State::Queued => "queued".into(),
        _ => t.title.clone(),
    };
    clip(&format!("{} · {tail}", t.key), 72)
}

/// The notification for a worker that has stopped to wait for its go.
pub fn go_notice(agent: &str, task: &str) -> (String, String) {
    (format!("fleet · {agent} needs a go"), format!("{task}: its plan is in, and it cannot change files until the chief or you say go"))
}

/// A notification for a board event worth interrupting someone for.
pub fn notice(e: &Event) -> Option<(String, String)> {
    if e.kind != "task" {
        return None;
    }
    let key = e.task_key.as_deref()?;
    let who = e.from_agent.as_deref().unwrap_or("fleet");
    let s = e.summary.as_str();
    let (what, rest) = if let Some(why) = s.strip_prefix("blocked: ") {
        ("is blocked", why.to_string())
    } else if s.starts_with("ready for review") {
        ("is ready for review", s.trim_start_matches("ready for review").trim_start_matches([' ', '-']).to_string())
    } else if s.starts_with("done") {
        ("is done", s.trim_start_matches("done").trim_start_matches([' ', '—']).to_string())
    } else {
        return None;
    };
    let body = if rest.is_empty() { who.to_string() } else { format!("{who}: {rest}") };
    Some((format!("fleet · {key} {what}"), body))
}

fn clip(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let cut: String = s.chars().take(width.saturating_sub(1)).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(key: &str, state: State, agent: &str) -> Task {
        Task {
            key: key.into(),
            title: "consume the parameter".into(),
            state,
            repo: "/w/billing-service".into(),
            epic_key: None,
            agent: Some(agent.into()),
            mr_url: None,
            blocked_on: None,
            waiting_on: vec![],
        }
    }

    fn event(summary: &str) -> Event {
        Event {
            ts: "2026-09-28T10:00:00Z".into(),
            kind: "task".into(),
            from_agent: Some("billing-svc".into()),
            to_agent: None,
            task_key: Some("ENG-2553-2".into()),
            summary: summary.into(),
            body: None,
        }
    }

    #[test]
    fn a_worker_waiting_for_its_go_says_so_first() {
        let t = task("ENG-2553-2", State::Running, "billing-svc");
        assert_eq!(worker_label("billing-svc", &[t], Some("ENG-2553-2")), "ENG-2553-2 · needs a go");
    }

    #[test]
    fn a_workers_label_says_what_it_is_on_and_where_that_stands() {
        let mut t = task("ENG-2553-2", State::Running, "billing-svc");
        assert_eq!(worker_label("billing-svc", &[t.clone()], None), "ENG-2553-2 · consume the parameter");
        t.state = State::Blocked;
        t.blocked_on = Some("needs the flag".into());
        assert_eq!(worker_label("billing-svc", &[t.clone()], None), "ENG-2553-2 · blocked: needs the flag");
        t.state = State::Queued;
        t.waiting_on = vec!["ENG-2553-1".into()];
        assert_eq!(worker_label("billing-svc", &[t], None), "ENG-2553-2 · waits on ENG-2553-1");
    }

    #[test]
    fn a_finished_task_is_not_what_an_agent_is_on() {
        let done = task("ENG-1-1", State::Done, "x");
        assert_eq!(worker_label("x", &[done.clone()], None), "no task");
        let next = task("ENG-1-2", State::Queued, "x");
        assert_eq!(worker_label("x", &[done, next], None), "ENG-1-2 · queued");
        assert_eq!(worker_label("y", &[task("ENG-1-3", State::Running, "x")], None), "no task", "only its own");
    }

    #[test]
    fn the_chiefs_label_is_the_board_at_a_glance() {
        assert_eq!(chief_label(&[]), "nothing open");
        let tasks = [
            task("a", State::Running, "x"),
            task("b", State::Blocked, "y"),
            task("c", State::Review, "z"),
            task("d", State::Done, "z"),
        ];
        assert_eq!(chief_label(&tasks), "3 open · 1 blocked · 1 in review");
    }

    #[test]
    fn only_what_needs_someone_becomes_a_notification() {
        let (title, body) = notice(&event("blocked: needs the flag")).unwrap();
        assert_eq!(title, "fleet · ENG-2553-2 is blocked");
        assert_eq!(body, "billing-svc: needs the flag");
        let (title, body) = notice(&event("ready for review - !412")).unwrap();
        assert_eq!(title, "fleet · ENG-2553-2 is ready for review");
        assert_eq!(body, "billing-svc: !412");
        assert_eq!(notice(&event("done")).unwrap().1, "billing-svc");
        assert!(notice(&event("started")).is_none());
        assert!(notice(&event("queued: x")).is_none());
        let mut msg = event("blocked: x");
        msg.kind = "message".into();
        assert!(notice(&msg).is_none(), "a message saying 'blocked' is not a blocked task");
    }

    #[test]
    fn every_agent_is_subscribed_to_by_its_pane() {
        let sub: serde_json::Value = serde_json::from_str(&subscription(&["w1:p1".into(), "w1:p4".into()])).unwrap();
        assert_eq!(sub["method"], "events.subscribe");
        let subs = sub["params"]["subscriptions"].as_array().unwrap();
        let status: Vec<&str> = subs
            .iter()
            .filter(|s| s["type"] == "pane.agent_status_changed")
            .map(|s| s["pane_id"].as_str().unwrap())
            .collect();
        assert_eq!(status, ["w1:p1", "w1:p4"]);
        assert!(subs.iter().any(|s| s["type"] == "pane.agent_detected"));
    }

    #[test]
    fn events_are_read_the_way_herdr_sends_them() {
        // Lines as herdr 0.9.0 sent them, from a recorded session.
        assert_eq!(
            classify(r#"{"data":{"agent":"claude","pane_id":"w1:p1","type":"pane_agent_detected","workspace_id":"w1"},"event":"pane_agent_detected"}"#),
            Change::Panes
        );
        assert_eq!(
            classify(r#"{"data":{"agent":"claude","agent_status":"blocked","pane_id":"w1:p1","workspace_id":"w1"},"event":"pane.agent_status_changed"}"#),
            Change::State
        );
        assert_eq!(classify(r#"{"id":"fleet","result":{"type":"subscription_started"}}"#), Change::Nothing);
        assert_eq!(classify("not json"), Change::Nothing);
    }
}
