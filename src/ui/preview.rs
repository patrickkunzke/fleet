//! A fixture fleet, drawn once and handed back to the shell.
//!
//! Layout work is the part of this that needs the tightest loop, and it is
//! also the part that has nothing to do with real agents: a column that is
//! one off is one off whether the pane behind it holds Claude Code or a cat.
//! So the preview invents the whole fleet — a seeded in-memory board, with
//! no herdr behind it — and draws one frame inline, in colour, where the
//! shell prompt was.
//!
//! Nothing here touches a fleet's board, the registry, or herdr. Running it
//! while a real fleet is up is safe, which is the whole point: the
//! alternative was killing the agents to look at a margin.

use std::io;
use std::path::PathBuf;

use anyhow::Result;
use crossterm::tty::IsTty;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::{TerminalOptions, Viewport};

use crate::db::{Db, NewTask, State};
use crate::ui::flow::View;
use crate::ui::{App, crew};

/// Build the fixture board. Every presence and every task state appears
/// once, so a palette change shows up here rather than in production.
fn seed(repo: &str) -> Result<Db> {
    let db = Db::open_in_memory()?;

    db.upsert_epic("ENG-2553", "shared settings flag")?;
    db.upsert_epic("ENG-2610", "node 24")?;

    db.upsert_agent("chief", Some("chief"), Some(repo), Some("s-chief"), None, None)?;
    db.upsert_agent("accounts-svc", None, Some("/w/accounts-service"), Some("s-set"), None, None)?;
    db.upsert_agent("storefront", None, Some("/w/storefront"), Some("s-ren"), None, None)?;
    db.upsert_agent("billing-svc", None, Some("/w/billing-service"), Some("s-con"), None, None)?;
    // One of each ending: a session that died, and one that never linked.
    db.upsert_agent("admin", None, Some("/w/admin"), Some("s-han"), None, None)?;
    db.upsert_agent("workspace", None, Some("/w/workspace"), None, None, None)?;

    let tasks = [
        ("ENG-2553-1", "share the column", "/w/accounts-service", "ENG-2553", &[][..]),
        ("ENG-2553-2", "consume the parameter", "/w/billing-service", "ENG-2553", &["ENG-2553-1"][..]),
        ("ENG-2553-3", "fall back to the old header", "/w/accounts-service", "ENG-2553", &["ENG-2553-1"][..]),
        ("ENG-2610-1", "cache v4", "/w/storefront", "ENG-2610", &[][..]),
        ("ENG-2610-2", "logger v9", "/w/storefront", "ENG-2610", &["ENG-2610-1"][..]),
    ];
    for (i, (key, title, repo, epic, deps)) in tasks.iter().enumerate() {
        db.add_task(&NewTask {
            key,
            title,
            repo,
            epic: Some(epic),
            deps,
            position: i as i64,
            ..Default::default()
        })?;
    }

    db.claim("ENG-2553-1", "accounts-svc")?;
    db.transition("ENG-2553-1", State::Running, None)?;
    db.claim("ENG-2610-1", "storefront")?;
    db.transition("ENG-2610-1", State::Running, None)?;
    db.claim("ENG-2553-2", "billing-svc")?;
    db.transition("ENG-2553-2", State::Blocked, Some("waits on ENG-2553-1"))?;

    // Two agents talking is what the flow graph is for, and nothing else in
    // the fixture produces an edge between them.
    db.log_event("message", Some("chief"), Some("accounts-svc"), Some("ENG-2553-1"),
        "take the column, billing-svc follows", None, None)?;
    db.log_event("message", Some("accounts-svc"), Some("billing-svc"), Some("ENG-2553-2"),
        "column is in, the parameter is yours", None, None)?;

    let dev = db.bg_start("storefront", "pnpm dev", "server", Some(3000), None, Some("/w/storefront"))?;
    db.bg_start("accounts-svc", "./gradlew bootRun", "server", Some(8080), None, Some("/w/accounts-service"))?;
    let test = db.bg_start("accounts-svc", "./gradlew test", "test", None, None, Some("/w/accounts-service"))?;
    db.bg_end(test, "passed", Some("51/51"))?;
    let _ = dev;

    Ok(db)
}

pub fn run(width: u16, height: u16, view: Option<&str>, plain: bool) -> Result<()> {
    let repo = std::env::current_dir()?;
    let db = seed(&repo.to_string_lossy())?;
    let mut app = App::new(db, PathBuf::from(":memory:"), Some(repo), None);
    app.refresh();
    dress(&mut app.rows);
    app.view = match view {
        Some("log") => View::Log,
        _ => View::Graph,
    };

    // Piped to a file or a diff there is no terminal to draw into, and the
    // inline viewport needs one. Falling back is friendlier than the error
    // crossterm gives, which names a device rather than the pipe.
    if plain || !io::stdout().is_tty() {
        plain_frame(&mut app, width, height)
    } else {
        inline_frame(&mut app, width, height)
    }
}

/// Give the fixture rows the liveness the registry would have given them.
///
/// The board says who exists; only a running process says who is working.
/// The preview has no processes, so rather than inventing registry files it
/// says outright what each row is — which also lets one frame carry every
/// presence at once.
fn dress(rows: &mut [crew::Row]) {
    use crew::Presence::*;
    for row in rows.iter_mut() {
        let (presence, uptime) = match row.name.as_str() {
            "chief" => (Waiting, Some("2h14m")),
            "accounts-svc" => (Working, Some("41m")),
            "storefront" => (Working, Some("18m")),
            "billing-svc" => (Waiting, Some("6m")),
            "admin" => (Gone, None),
            _ => (Unlinked, None),
        };
        row.presence = presence;
        row.uptime = uptime.map(str::to_string);
        // The second line is a task when there is one and a reason when
        // there is not, and the reason follows the presence we just changed.
        if matches!(row.detail.as_str(), "session ended" | "no session yet" | "no task") {
            row.detail = match presence {
                Gone => "session ended",
                Unlinked => "no session yet",
                _ => "no task",
            }
            .into();
        }
    }
}

/// Draw where the prompt was, in colour, and leave it in the scrollback.
fn inline_frame(app: &mut App, width: u16, height: u16) -> Result<()> {
    let mut term = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )?;
    // The real app owns the whole terminal; inline mode owns `height` rows of
    // it, and a preview wider than the window would wrap every line into
    // nonsense. Narrower is fine — that is the case being tested.
    let actual = term.size()?.width;
    if width > actual {
        anyhow::bail!("the preview is {width} columns and the terminal is {actual}");
    }
    term.draw(|f| {
        let area = ratatui::layout::Rect {
            width: width.min(f.area().width),
            height: height.min(f.area().height),
            ..f.area()
        };
        app.draw_into(f, area);
    })?;
    // Put the shell's cursor back under the frame rather than on top of it.
    let bottom = term.get_frame().area().bottom();
    term.set_cursor_position((0, bottom.saturating_sub(1)))?;
    println!();
    Ok(())
}

/// The same frame as text, for a diff or a test.
fn plain_frame(app: &mut App, width: u16, height: u16) -> Result<()> {
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(width, height))?;
    term.draw(|f| app.draw(f))?;
    print!("{}", term.backend());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn previewed(width: u16, height: u16) -> String {
        let db = seed("/w/fleet").unwrap();
        let mut app = App::new(db, PathBuf::from(":memory:"), Some(PathBuf::from("/w/fleet")), None);
        app.refresh();
        dress(&mut app.rows);
        let mut term = ratatui::Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        format!("{}", term.backend())
    }

    #[test]
    fn the_fixture_fills_every_pane() {
        let out = previewed(110, 30);
        assert!(out.contains("chief"), "the graph: {out}");
        assert!(out.contains("ENG-2553"), "the tasks: {out}");
        assert!(out.contains("pnpm dev"), "the background half: {out}");
        assert!(
            !out.contains("nothing on the board"),
            "a preview with an empty pane is not previewing that pane: {out}"
        );
    }

    #[test]
    fn it_carries_every_presence_so_one_frame_shows_the_whole_palette() {
        let db = seed("/w/fleet").unwrap();
        let mut app = App::new(db, PathBuf::from(":memory:"), None, None);
        app.refresh();
        dress(&mut app.rows);

        use crew::Presence::*;
        for wanted in [Working, Waiting, Gone, Unlinked] {
            assert!(
                app.rows.iter().any(|r| r.presence == wanted),
                "no row is {wanted:?}, so its glyph and colour go unseen"
            );
        }
    }

    #[test]
    fn a_row_made_live_stops_claiming_its_session_ended() {
        let db = seed("/w/fleet").unwrap();
        let mut app = App::new(db, PathBuf::from(":memory:"), None, None);
        app.refresh();
        dress(&mut app.rows);

        // The board alone cannot tell a live agent from a dead one, so every
        // row arrives here reading "session ended". Saying that under a row
        // drawn as working is the bug this catches.
        let chief = app.rows.iter().find(|r| r.name == "chief").unwrap();
        assert_eq!(chief.presence, crew::Presence::Waiting);
        assert_ne!(chief.detail, "session ended");
    }

    #[test]
    fn drawing_into_a_rectangle_leaves_the_rest_of_the_buffer_alone() {
        // What inline mode needs: a preview narrower than the window must not
        // write into the columns beside it.
        let db = seed("/w/fleet").unwrap();
        let mut app = App::new(db, PathBuf::from(":memory:"), None, None);
        app.refresh();

        let mut term = ratatui::Terminal::new(TestBackend::new(120, 20)).unwrap();
        term.draw(|f| {
            let area = ratatui::layout::Rect::new(0, 0, 90, 20);
            app.draw_into(f, area);
        })
        .unwrap();

        let buf = term.backend().buffer();
        for y in 0..20 {
            for x in 90..120 {
                assert_eq!(
                    buf.cell((x, y)).unwrap().symbol(),
                    " ",
                    "column {x} is outside the frame"
                );
            }
        }
    }
}
