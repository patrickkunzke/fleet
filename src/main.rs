//! For now this binary only proves the registry watcher works end to end.
//! The TUI, and the `board` subcommand that replaces cli/board.sh, come next.

mod registry;

use std::time::Duration;

use anyhow::Result;

use crate::registry::{Change, Registry, Watcher};

fn main() -> Result<()> {
    let follow = std::env::args().any(|a| a == "--watch" || a == "-w");
    let dir = registry::default_dir();
    let projects = registry::default_projects_dir();

    let mut reg = Registry::new(&dir);
    reg.refresh();

    let mut sessions: Vec<_> = reg.interactive().collect();
    sessions.sort_by_key(|s| s.started_at);
    println!("{} live in {}", sessions.len(), dir.display());
    for s in sessions {
        let transcript = s
            .transcript_path(&projects)
            .map(|p| {
                std::fs::metadata(&p)
                    .map(|m| format!("{} KB", m.len() / 1024))
                    .unwrap_or_default()
            })
            .unwrap_or_else(|| "no transcript".into());
        println!(
            "  {:<4} {:<28} {:<5} {:<22} {}",
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
    let watcher = Watcher::new(&dir)?;
    loop {
        // The timeout is the safety net: it catches processes that died
        // without removing their file, which produces no filesystem event.
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
