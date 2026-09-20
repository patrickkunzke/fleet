//! A placeholder driver that exercises the pieces the TUI will be built from.
//! `fleet` lists live sessions; `fleet <name>` renders one session's transcript
//! the way the centre pane will; `--watch` follows.

mod registry;
mod transcript;

use std::time::Duration;

use anyhow::{Result, bail};

use crate::registry::{Change, Registry, Watcher};
use crate::transcript::{BACKFILL_BYTES, Entry, Outcome, Transcript};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let follow = args.iter().any(|a| a == "--watch" || a == "-w");
    let target = args.iter().find(|a| !a.starts_with('-')).cloned();

    let dir = registry::default_dir();
    let mut reg = Registry::new(&dir);
    reg.refresh();

    match target {
        Some(name) => show_session(&reg, &name, follow),
        None => list_sessions(&mut reg, &dir, follow),
    }
}

fn list_sessions(reg: &mut Registry, dir: &std::path::Path, follow: bool) -> Result<()> {
    let projects = registry::default_projects_dir();
    let mut sessions: Vec<_> = reg.interactive().collect();
    sessions.sort_by_key(|s| s.started_at);

    println!("{} live in {}", sessions.len(), dir.display());
    for s in sessions {
        let transcript = match s.transcript_path(&projects) {
            Some(p) => std::fs::metadata(&p)
                .map(|m| format!("{} KB", m.len() / 1024))
                .unwrap_or_default(),
            None => "no transcript".into(),
        };
        println!(
            "  {:<6} {:<28} {:<5} {:<24} {}",
            s.pid,
            s.name,
            s.status.as_str(),
            s.repo(),
            transcript
        );
    }

    if !follow {
        return Ok(());
    }
    println!("\nwatching — ctrl-c to stop");
    let watcher = Watcher::new(dir)?;
    loop {
        watcher.wait(Duration::from_secs(5));
        for change in reg.refresh() {
            match change {
                Change::Upserted(s) => {
                    println!("  + {:<28} {:<5} {}", s.name, s.status.as_str(), s.repo())
                }
                Change::Removed { name, pid } => println!("  - {name} ({pid})"),
            }
        }
    }
}

fn show_session(reg: &Registry, name: &str, follow: bool) -> Result<()> {
    let Some(session) = reg.by_name(name) else {
        bail!("no live session named '{name}'");
    };
    let projects = registry::default_projects_dir();
    let Some(path) = session.transcript_path(&projects) else {
        bail!("'{name}' has no transcript yet");
    };

    let mut t = Transcript::open(&path, BACKFILL_BYTES)?;
    println!(
        "{} · {} · {} entries\n",
        session.name,
        session.repo(),
        t.len()
    );
    for entry in t.tail(40) {
        println!("{}", render(entry));
    }

    if !follow {
        return Ok(());
    }
    loop {
        // Polling, not watching: a transcript that is being appended to
        // changes constantly, and the pane redraws on a tick anyway.
        std::thread::sleep(Duration::from_millis(400));
        let added = t.poll()?;
        for entry in t.tail(added) {
            println!("{}", render(entry));
        }
    }
}

fn render(e: &Entry) -> String {
    let at = e.hhmm();
    match e {
        Entry::Prompt { text, .. } => format!("{at}  > {}", first_line(text)),
        Entry::CrossSessionMessage { from, text, .. } => {
            format!("{at}  <- {from}: {}", first_line(text))
        }
        Entry::Say { text, .. } => format!("{at}     {}", first_line(text)),
        Entry::Thought { chars, .. } => format!("{at}     ... thought {chars} chars"),
        Entry::Tool {
            name,
            target,
            outcome,
            sidechain,
            ..
        } => {
            let mark = if *sidechain { "  ~" } else { "  *" };
            let result = match outcome {
                Outcome::Pending => "running".into(),
                Outcome::Ok(s) => s.clone(),
                Outcome::Failed(s) => format!("failed: {s}"),
                Outcome::Background(id) => format!("bg {id}"),
            };
            format!("{at}{mark} {name:<6} {target:<44} {result}")
        }
        Entry::Turn { secs, .. } => format!("{at}     --- turn {secs:.1}s"),
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() > 76 {
        let cut: String = line.chars().take(75).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}
