//! Everything fleet sends into a pane, off the thread that draws.
//!
//! Each send is a tmux client process: about 11ms to start it and hear
//! back. Done on the UI thread, a trackpad flick — thirty to sixty wheel
//! events a second, each once four processes — queued work faster than it
//! could be drained, and the pane froze and then kept scrolling after the
//! hand had stopped. Typing slept on top of that, waiting for an echo.
//!
//! So sends are queued, in order, to one thread that owns them. It takes
//! everything waiting at once and merges what can be merged: a run of typed
//! characters is one `send-keys`, a flick of the wheel is one report. The
//! UI thread never waits on tmux for input at all.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::tmux::{Pane, PaneMode, Tmux};

/// What to deliver to a pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// Typed text, sent literally.
    Text(String),
    /// A tmux key name: Enter, M-Left, C-c.
    Key(String),
    /// A paste, delivered as one.
    Paste(String),
    /// Wheel notches: positive is up, towards history.
    Wheel(isize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued {
    pub pane: Pane,
    pub job: Job,
}

/// What the sender tells the UI thread back.
pub enum Reply {
    /// A send failed; worth saying, since the keystroke went nowhere.
    Failed(String),
    /// A wheel over a pane with no program asking for it: scroll fleet's own
    /// view of it, which only the UI thread holds.
    ScrollLocal(isize),
}

pub struct Sender {
    tx: mpsc::Sender<Queued>,
}

/// How long a pane's mode is trusted. A program changes it at startup and
/// on exit, not between two wheel events, and asking costs a process.
const MODE_TTL: Duration = Duration::from_secs(1);

impl Sender {
    pub fn start(tmux: Tmux, reply: impl Fn(Reply) + Send + 'static) -> Sender {
        let (tx, rx) = mpsc::channel::<Queued>();
        std::thread::spawn(move || {
            let mut modes: HashMap<String, (PaneMode, Instant)> = HashMap::new();
            while let Ok(first) = rx.recv() {
                // Everything already waiting, so bursts merge.
                let mut batch = vec![first];
                batch.extend(rx.try_iter());
                for q in coalesce(batch) {
                    if let Err(e) = deliver(&tmux, &q, &mut modes, &reply) {
                        reply(Reply::Failed(e.to_string()));
                    }
                }
            }
        });
        Sender { tx }
    }

    pub fn send(&self, pane: &Pane, job: Job) {
        let _ = self.tx.send(Queued {
            pane: pane.clone(),
            job,
        });
    }
}

/// Merge neighbours that mean the same thing sent once.
///
/// Only neighbours, and only to the same pane: order is the whole contract.
/// "ab", Enter, "c" must not become "abc", Enter.
pub fn coalesce(batch: Vec<Queued>) -> Vec<Queued> {
    let mut out: Vec<Queued> = Vec::with_capacity(batch.len());
    for q in batch {
        if let Some(last) = out.last_mut()
            && last.pane == q.pane
        {
            match (&mut last.job, &q.job) {
                (Job::Text(a), Job::Text(b)) => {
                    a.push_str(b);
                    continue;
                }
                (Job::Wheel(a), Job::Wheel(b)) => {
                    *a += b;
                    continue;
                }
                _ => {}
            }
        }
        out.push(q);
    }
    // A flick up and back down again is no scroll at all.
    out.retain(|q| q.job != Job::Wheel(0));
    out
}

fn deliver(
    tmux: &Tmux,
    q: &Queued,
    modes: &mut HashMap<String, (PaneMode, Instant)>,
    reply: &impl Fn(Reply),
) -> anyhow::Result<()> {
    match &q.job {
        Job::Text(text) => tmux.send_text(&q.pane, text),
        Job::Key(name) => tmux.send_key(&q.pane, name),
        Job::Paste(text) => tmux.paste(&q.pane, text),
        Job::Wheel(notches) => {
            let mode = match modes.get(&q.pane.id) {
                Some((mode, at)) if at.elapsed() < MODE_TTL => *mode,
                _ => {
                    let mode = tmux.mode(&q.pane)?;
                    modes.insert(q.pane.id.clone(), (mode, Instant::now()));
                    mode
                }
            };
            wheel(tmux, &q.pane, mode, *notches, reply)
        }
    }
}

/// Lines a notch moves, where the move is ours or is arrow keys. A report
/// is one notch, and the program decides how far that is.
const LINES_PER_NOTCH: usize = 3;

fn wheel(
    tmux: &Tmux,
    pane: &Pane,
    mode: PaneMode,
    notches: isize,
    reply: &impl Fn(Reply),
) -> anyhow::Result<()> {
    let up = notches > 0;
    let n = notches.unsigned_abs();
    if mode.mouse {
        // One report per notch, as a terminal sends them, all in one call.
        let mut bytes = Vec::new();
        for _ in 0..n {
            bytes.extend(report(up, mode.sgr, pane));
        }
        tmux.send_raw(pane, &bytes)
    } else if mode.alternate {
        // What tmux itself sends a program on the alternate screen that did
        // not ask for the mouse: it has no scrollback, so arrows it is.
        tmux.send_repeated(pane, if up { "Up" } else { "Down" }, n * LINES_PER_NOTCH)
    } else {
        reply(Reply::ScrollLocal(notches * LINES_PER_NOTCH as isize));
        Ok(())
    }
}

/// One wheel report, aimed at the middle of the pane: a report carries a
/// position, some programs route by it, and the centre is inside every pane.
fn report(up: bool, sgr: bool, pane: &Pane) -> Vec<u8> {
    let _ = pane;
    let (col, row) = (10u16, 5u16);
    let button: u16 = if up { 64 } else { 65 };
    if sgr {
        format!("\x1b[<{button};{col};{row}M").into_bytes()
    } else {
        // X10 biases everything by 32 and cannot express a coordinate past
        // 223. The cap is the protocol's, not ours.
        let at = |n: u16| (32 + n.min(223)) as u8;
        vec![0x1b, b'[', b'M', at(button), at(col), at(row)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn pane(id: &str) -> Pane {
        Pane {
            id: id.into(),
            window: "@1".into(),
            session: "s".into(),
            window_name: "w".into(),
            pid: 1,
            cwd: PathBuf::from("/"),
        }
    }

    fn q(p: &str, job: Job) -> Queued {
        Queued { pane: pane(p), job }
    }

    #[test]
    fn a_burst_of_typing_is_sent_once() {
        // Each send is a process; forty characters typed while one was in
        // flight were forty more.
        let out = coalesce(vec![
            q("%1", Job::Text("h".into())),
            q("%1", Job::Text("e".into())),
            q("%1", Job::Text("llo".into())),
        ]);
        assert_eq!(out, vec![q("%1", Job::Text("hello".into()))]);
    }

    #[test]
    fn order_survives_merging() {
        // "ab", Enter, "c" must not become "abc", Enter.
        let out = coalesce(vec![
            q("%1", Job::Text("a".into())),
            q("%1", Job::Text("b".into())),
            q("%1", Job::Key("Enter".into())),
            q("%1", Job::Text("c".into())),
        ]);
        assert_eq!(
            out,
            vec![
                q("%1", Job::Text("ab".into())),
                q("%1", Job::Key("Enter".into())),
                q("%1", Job::Text("c".into())),
            ]
        );
    }

    #[test]
    fn text_for_two_panes_is_not_merged_across_them() {
        // Typed into one agent, then clicked another: the second agent must
        // not receive the first one's keystrokes.
        let out = coalesce(vec![
            q("%1", Job::Text("a".into())),
            q("%2", Job::Text("b".into())),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_flick_of_the_wheel_is_one_scroll() {
        let out = coalesce(vec![
            q("%1", Job::Wheel(1)),
            q("%1", Job::Wheel(1)),
            q("%1", Job::Wheel(1)),
        ]);
        assert_eq!(out, vec![q("%1", Job::Wheel(3))]);
    }

    #[test]
    fn up_and_back_down_again_is_no_scroll() {
        let out = coalesce(vec![q("%1", Job::Wheel(2)), q("%1", Job::Wheel(-2))]);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn a_flick_of_fifty_notches_leaves_the_ui_thread_at_once_and_arrives_whole() {
        use std::sync::{Arc, Mutex};

        let name = format!("fleet-sender-{}", std::process::id());
        let Ok(tmux) = Tmux::detect(Some(&name)) else { return };
        let tmux = tmux.on_socket(&name);
        let dir = tempfile::tempdir().unwrap();
        let got = dir.path().join("input");
        // Claude Code's shape: alternate screen, mouse, SGR, raw tty.
        let Ok(pane) = tmux.spawn(
            "grabby",
            dir.path(),
            &format!(
                "sh -c 'stty raw -echo; printf \"\\033[?1049h\\033[?1000h\\033[?1006h\"; cat > {}'",
                got.display()
            ),
        ) else {
            let _ = tmux.kill_server();
            return;
        };
        std::thread::sleep(Duration::from_millis(500));

        let failures = Arc::new(Mutex::new(Vec::new()));
        let seen = failures.clone();
        let sender = Sender::start(tmux.clone(), move |r| {
            if let Reply::Failed(e) = r {
                seen.lock().unwrap().push(e);
            }
        });

        // What the UI thread pays: enqueueing, nothing else. It used to be
        // four tmux processes, about 47ms, for each one of these.
        let t = Instant::now();
        for _ in 0..50 {
            sender.send(&pane, Job::Wheel(1));
        }
        let enqueued = t.elapsed();

        let mut count = 0;
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) {
            let data = std::fs::read(&got).unwrap_or_default();
            count = String::from_utf8_lossy(&data).matches("\x1b[<64;").count();
            if count >= 50 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let delivered = t.elapsed();
        let _ = tmux.kill_server();

        eprintln!("50 notches: enqueued in {enqueued:?}, all delivered after {delivered:?}");
        assert!(failures.lock().unwrap().is_empty(), "{:?}", failures.lock().unwrap());
        assert_eq!(count, 50, "every notch is a report, none lost to merging");
        assert!(enqueued < Duration::from_millis(20), "the UI thread waited: {enqueued:?}");
    }

    #[test]
    fn a_burst_of_typing_arrives_in_order_without_waiting_on_the_ui_thread() {
        let name = format!("fleet-typing-{}", std::process::id());
        let Ok(tmux) = Tmux::detect(Some(&name)) else { return };
        let tmux = tmux.on_socket(&name);
        let dir = tempfile::tempdir().unwrap();
        let got = dir.path().join("input");
        let Ok(pane) = tmux.spawn(
            "typist",
            dir.path(),
            &format!("sh -c 'stty raw -echo; cat > {}'", got.display()),
        ) else {
            let _ = tmux.kill_server();
            return;
        };
        std::thread::sleep(Duration::from_millis(500));
        let sender = Sender::start(tmux.clone(), |_| {});

        let text = "the quick brown fox jumps over the lazy dog";
        let t = Instant::now();
        for c in text.chars() {
            sender.send(&pane, Job::Text(c.to_string()));
        }
        let enqueued = t.elapsed();

        let t = Instant::now();
        let mut seen = String::new();
        while t.elapsed() < Duration::from_secs(5) {
            seen = std::fs::read_to_string(&got).unwrap_or_default();
            if seen.len() >= text.len() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let delivered = t.elapsed();
        let _ = tmux.kill_server();

        eprintln!("{} keystrokes: enqueued in {enqueued:?}, delivered after {delivered:?}", text.len());
        assert_eq!(seen, text, "in order, nothing dropped or doubled");
    }

    #[test]
    fn keys_are_not_merged_with_each_other() {
        // Two Enters are two Enters.
        let out = coalesce(vec![
            q("%1", Job::Key("Enter".into())),
            q("%1", Job::Key("Enter".into())),
        ]);
        assert_eq!(out.len(), 2);
    }
}
