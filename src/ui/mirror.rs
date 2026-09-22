//! A live view of an agent's actual terminal.
//!
//! The centre pane shows the real `claude` REPL — spinners, permission
//! prompts, its own colours — not a re-rendering of what it wrote. tmux keeps
//! owning the process, so an agent survives this program restarting and
//! `↵` can still hand you the unmodified terminal.
//!
//! How: `pipe-pane` streams everything the pane prints into a file, and a
//! vt100 parser turns that back into a screen. The stream only carries what
//! happens *after* it is turned on, so the parser is first seeded with
//! `capture-pane`, otherwise a session you select mid-flight starts blank.
//!
//! The transcript pane is still the fallback, and still the only thing that
//! can show a session that is not in our tmux, or one that has ended.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use anyhow::{Context, Result};
use ratatui::prelude::*;
use tui_term::widget::PseudoTerminal;

use crate::tmux::{Pane, Tmux};

/// Restart the stream once its file passes this. An agent left running for a
/// day would otherwise fill the disk with scrollback nobody reads.
const MAX_STREAM_BYTES: u64 = 4 * 1024 * 1024;

/// Lines of scrollback the parser keeps. Deep enough to read back through a
/// long turn with the wheel; the transcript is there for anything older.
const SCROLLBACK: usize = 2000;

pub struct Mirror {
    tmux: Tmux,
    pane: Pane,
    parser: vt100::Parser,
    stream: PathBuf,
    file: Option<File>,
    offset: u64,
    size: (u16, u16),
}

impl Mirror {
    /// Start mirroring a pane at the size it will be drawn.
    pub fn attach(tmux: &Tmux, pane: Pane, cols: u16, rows: u16) -> Result<Mirror> {
        let (cols, rows) = (cols.max(20), rows.max(4));
        tmux.set_size(&pane, cols, rows)?;

        let dir = std::env::temp_dir().join("fleet-mirror");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        // '%12' is not a filename anywhere pleasant.
        let stream = dir.join(format!(
            "{}-{}.stream",
            pane.session,
            pane.id.trim_start_matches('%')
        ));
        let _ = std::fs::remove_file(&stream);

        let mut mirror = Mirror {
            tmux: tmux.clone(),
            pane,
            parser: vt100::Parser::new(rows, cols, SCROLLBACK),
            stream,
            file: None,
            offset: 0,
            size: (cols, rows),
        };
        mirror.seed()?;
        mirror.start_pipe()?;
        Ok(mirror)
    }

    /// Paint the pane's current screen into the parser, so selecting a
    /// long-running agent shows what it is doing rather than a blank pane
    /// until its next line of output.
    fn seed(&mut self) -> Result<()> {
        let screen = self.tmux.capture(&self.pane)?;
        // capture-pane hands back lines; a terminal needs carriage returns to
        // put the cursor back at column zero on each one.
        let seeded = screen.replace('\n', "\r\n");
        self.parser.process(seeded.as_bytes());
        Ok(())
    }

    fn start_pipe(&mut self) -> Result<()> {
        self.tmux.pipe_to(&self.pane, &self.stream)?;
        Ok(())
    }

    pub fn pane(&self) -> &Pane {
        &self.pane
    }

    /// Match the tmux pane to the area we are about to draw it in.
    ///
    /// Without this the agent wraps its output to a width nobody is looking
    /// at, and every line arrives broken in the wrong place.
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        let (cols, rows) = (cols.max(20), rows.max(4));
        if (cols, rows) == self.size {
            return false;
        }
        self.size = (cols, rows);
        let _ = self.tmux.set_size(&self.pane, cols, rows);

        // Re-seed rather than resize the existing screen. The text already in
        // the parser was wrapped by tmux at the old width, and stretching that
        // grid leaves every one of those breaks where it was. Asking tmux for
        // the screen again gets it rewrapped by the program that owns it.
        self.parser = vt100::Parser::new(rows, cols, SCROLLBACK);
        let _ = self.seed();
        true
    }

    /// Scroll the mirrored screen, in lines. Positive goes back in time.
    ///
    /// Ours, not the pane's: tmux would have to be put into copy mode, which
    /// produces no output for `pipe-pane` to carry, so the view would freeze
    /// rather than scroll. The parser already keeps the history.
    /// A wheel notch over this pane, delivered the way the program inside
    /// expects it.
    ///
    /// Three cases, and only the last is ours to handle. A program that
    /// asked for the mouse gets a mouse report: Claude Code does, and it
    /// keeps its own history. A program on the alternate screen that did
    /// not gets arrow keys, which is what tmux sends in the same situation —
    /// the alternate screen has no scrollback, so there is nothing for us to
    /// move through. Only a plain pane is scrolled through our view of it.
    pub fn scroll(&mut self, delta: isize) -> Result<()> {
        let mode = self.tmux.mode(&self.pane)?;
        let up = delta > 0;
        let notches = delta.unsigned_abs();

        if mode.mouse {
            for _ in 0..notches {
                self.wheel(up, mode.sgr)?;
            }
        } else if mode.alternate {
            let key = if up { "Up" } else { "Down" };
            for _ in 0..notches {
                self.tmux.send_key(&self.pane, key)?;
            }
        } else {
            self.scroll_by(delta);
        }
        Ok(())
    }

    /// One wheel report, aimed at the middle of the pane: a report carries a
    /// position, some programs route by it, and the centre is inside every
    /// pane and belongs to no edge.
    fn wheel(&self, up: bool, sgr: bool) -> Result<()> {
        let (rows, cols) = self.parser.screen().size();
        let col = (cols / 2).max(1);
        let row = (rows / 2).max(1);
        let button = if up { 64u16 } else { 65 };
        let bytes = if sgr {
            format!("\x1b[<{button};{col};{row}M").into_bytes()
        } else {
            // X10 biases everything by 32 and cannot express a coordinate
            // past 223. The cap is the protocol's, not ours.
            let at = |n: u16| (32 + n.min(223)) as u8;
            vec![0x1b, b'[', b'M', at(button), at(col), at(row)]
        };
        self.tmux.send_raw(&self.pane, &bytes)
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let at = self.parser.screen().scrollback() as isize;
        let wanted = (at + delta).max(0) as usize;
        self.parser.screen_mut().set_scrollback(wanted);
    }

    /// Back to the live edge.
    pub fn to_live(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }

    pub fn scrolled_back(&self) -> bool {
        self.parser.screen().scrollback() > 0
    }

    /// Feed the parser whatever the pane has printed. True when the screen
    /// changed, so the caller can skip a redraw when it did not.
    pub fn poll(&mut self) -> bool {
        if self.file.is_none() {
            match File::open(&self.stream) {
                Ok(f) => self.file = Some(f),
                // tmux creates the file on the pane's first byte of output; an
                // idle agent simply has not produced one yet.
                Err(_) => return false,
            }
        }

        let Some(file) = self.file.as_mut() else {
            return false;
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            // Rotated underneath us; take it from the top.
            self.offset = 0;
        }
        if len == self.offset {
            return false;
        }

        let mut buf = Vec::new();
        if file.seek(SeekFrom::Start(self.offset)).is_err() || file.read_to_end(&mut buf).is_err() {
            return false;
        }
        self.offset += buf.len() as u64;
        self.parser.process(&buf);

        if self.offset > MAX_STREAM_BYTES {
            self.rotate();
        }
        true
    }

    /// Start a fresh stream file, keeping the screen we already have.
    ///
    /// Re-seeding is not needed: the parser's state is the screen, and it
    /// survives the file being replaced underneath it.
    fn rotate(&mut self) {
        let _ = self.tmux.stop_pipe(&self.pane);
        self.file = None;
        self.offset = 0;
        let _ = std::fs::remove_file(&self.stream);
        let _ = self.start_pipe();
    }

    /// The mirrored screen, for reading a selection out of it.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(PseudoTerminal::new(self.parser.screen()), area);
    }

    /// What the pane currently reads as.
    #[cfg(test)]
    pub fn contents(&self) -> String {
        self.parser.screen().contents()
    }

    fn detach(&mut self) {
        let _ = self.tmux.stop_pipe(&self.pane);
        let _ = std::fs::remove_file(&self.stream);
    }
}

impl Drop for Mirror {
    fn drop(&mut self) {
        // Leaving pipe-pane running would have tmux writing to a file nobody
        // reads, for as long as the agent lives.
        self.detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Scratch {
        tmux: Tmux,
    }

    impl Scratch {
        fn new(tag: &str) -> Option<Scratch> {
            let name = format!("fleet-mirror-{tag}-{}", std::process::id());
            // A server of its own, so these tests cannot queue behind each
            // other's sleeping panes — nor touch the user's tmux.
            let tmux = Tmux::detect(Some(&name)).ok()?.on_socket(&name);
            tmux.ensure_session().ok()?;
            Some(Scratch { tmux })
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = self.tmux.kill_server();
        }
    }

    /// Poll until the screen says something, or give up.
    fn settle(mirror: &mut Mirror, wanted: &str) -> String {
        for _ in 0..40 {
            mirror.poll();
            let seen = mirror.contents();
            if seen.contains(wanted) {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        mirror.contents()
    }

    #[test]
    fn the_wheel_reaches_a_program_that_asked_for_the_mouse() {
        // Claude Code does exactly this: alternate screen, so there is no
        // scrollback for us to move, and mouse capture, so the wheel is its
        // to handle. Scrolling our own view of such a pane moves nothing.
        let Some(s) = Scratch::new("wheel") else { return };
        let dir = tempfile::tempdir().unwrap();
        let got = dir.path().join("input");
        let pane = s
            .tmux
            .spawn(
                "grabby",
                dir.path(),
                &format!(
                    // Raw mode, as any program that reads the mouse must
                    // use: a wheel report carries no newline, and a tty in
                    // canonical mode would hold it until one arrived.
                    "sh -c 'stty raw -echo; \
                     printf \"\\033[?1049h\\033[?1000h\\033[?1006h\"; cat > {}'",
                    got.display()
                ),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(500));

        let mut mirror = Mirror::attach(&s.tmux, pane, 40, 10).unwrap();
        // Asked of tmux, not of the byte stream: the modes were set before
        // we attached, which is the ordinary case for an agent that has been
        // running for hours.
        let mode = s.tmux.mode(mirror.pane()).unwrap();
        assert!(mode.mouse, "the pane never asked for the mouse");
        assert!(mode.alternate, "nor moved to the alternate screen");

        mirror.scroll(1).unwrap();
        for _ in 0..40 {
            if std::fs::read(&got).is_ok_and(|b| !b.is_empty()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let seen = String::from_utf8_lossy(&std::fs::read(&got).unwrap_or_default()).to_string();
        assert!(seen.contains("\x1b[<64;"), "not an SGR wheel-up report: {seen:?}");
        assert!(seen.ends_with('M'), "{seen:?}");

        mirror.scroll(-1).unwrap();
        for _ in 0..40 {
            if std::fs::read_to_string(&got).is_ok_and(|s| s.contains("65;")) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let seen = std::fs::read_to_string(&got).unwrap_or_default();
        assert!(seen.contains("\x1b[<65;"), "wheel-down is a different button: {seen:?}");
    }

    #[test]
    fn a_plain_pane_is_scrolled_through_our_own_view_of_it() {
        // A shell or a log has real scrollback and never asked for the
        // mouse; there is nothing to forward the wheel to.
        let Some(s) = Scratch::new("plain") else { return };
        let dir = tempfile::tempdir().unwrap();
        let pane = s
            .tmux
            .spawn("plain", dir.path(), "sh -c 'echo plain here; sleep 20'")
            .unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let mut mirror = Mirror::attach(&s.tmux, pane, 40, 10).unwrap();
        settle(&mut mirror, "plain here");
        let mode = s.tmux.mode(mirror.pane()).unwrap();
        assert!(!mode.mouse, "a shell does not ask for the mouse");
        assert!(!mode.alternate, "nor use the alternate screen");

        // So the wheel moves our own view, and nothing is typed at it.
        mirror.scroll(1).unwrap();
    }

    #[test]
    fn a_drag_over_the_pane_reads_back_what_the_agent_printed() {
        use crate::ui::selection::Selection;

        let Some(s) = Scratch::new("select") else { return };
        let dir = tempfile::tempdir().unwrap();
        let pane = s
            .tmux
            .spawn(
                "selectable",
                dir.path(),
                "sh -c 'printf \"MR !412 is green\\nsecond line\\n\"; sleep 20'",
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let mut mirror = Mirror::attach(&s.tmux, pane, 40, 10).unwrap();
        settle(&mut mirror, "MR !412");

        // The whole of the first line: the end column is the cell under the
        // pointer, which is selected like everywhere else.
        let mut sel = Selection::start((0, 0));
        sel.drag_to((0, 15));
        assert_eq!(sel.text(mirror.screen()).trim_end(), "MR !412 is green");

        // And across the break into the next.
        sel.drag_to((1, 5));
        let both = sel.text(mirror.screen());
        assert!(both.starts_with("MR !412 is green"), "{both:?}");
        assert!(both.contains('\n'), "the line break comes with it: {both:?}");
        assert!(both.trim_end().ends_with("second"), "{both:?}");
    }

    #[test]
    fn it_shows_what_the_pane_printed_before_we_attached() {
        let Some(s) = Scratch::new("seed") else { return };
        let dir = tempfile::tempdir().unwrap();
        let pane = s
            .tmux
            .spawn("seeded", dir.path(), "sh -c 'echo already here; sleep 20'")
            .unwrap();
        std::thread::sleep(Duration::from_millis(500));

        // Attaching after the output happened is the normal case: agents are
        // running long before you select one.
        let mirror = Mirror::attach(&s.tmux, pane, 40, 10).unwrap();
        assert!(
            mirror.contents().contains("already here"),
            "the seed must carry the existing screen: {:?}",
            mirror.contents()
        );
    }

    #[test]
    fn it_follows_what_the_pane_prints_afterwards() {
        let Some(s) = Scratch::new("follow") else { return };
        let dir = tempfile::tempdir().unwrap();
        let pane = s
            .tmux
            .spawn("live", dir.path(), "sh -c 'sleep 30'")
            .unwrap();

        let mut mirror = Mirror::attach(&s.tmux, pane.clone(), 40, 10).unwrap();
        s.tmux.send_line(&pane, "echo arrived-later").unwrap();

        let seen = settle(&mut mirror, "arrived-later");
        assert!(seen.contains("arrived-later"), "got {seen:?}");
    }

    #[test]
    fn the_pane_is_resized_to_the_area_it_is_drawn_in() {
        let Some(s) = Scratch::new("resize") else { return };
        let dir = tempfile::tempdir().unwrap();
        let pane = s.tmux.spawn("sized", dir.path(), "sleep 20").unwrap();

        let mut mirror = Mirror::attach(&s.tmux, pane.clone(), 40, 10).unwrap();
        assert!(mirror.resize(72, 20), "a new size is a change");
        assert!(!mirror.resize(72, 20), "the same size is not");

        let size = s
            .tmux
            .window_size(&pane)
            .unwrap();
        assert_eq!(size, (72, 20), "tmux must agree with what we will draw");
    }

    #[test]
    fn detaching_stops_the_pipe_and_removes_the_stream() {
        let Some(s) = Scratch::new("detach") else { return };
        let dir = tempfile::tempdir().unwrap();
        let pane = s.tmux.spawn("temp", dir.path(), "sh -c 'sleep 20'").unwrap();

        let mirror = Mirror::attach(&s.tmux, pane.clone(), 40, 10).unwrap();
        let stream = mirror.stream.clone();
        s.tmux.send_line(&pane, "echo something").unwrap();
        std::thread::sleep(Duration::from_millis(600));
        assert!(stream.exists(), "the stream file is created on first output");

        drop(mirror);
        assert!(!stream.exists(), "dropping the mirror cleans up after itself");

        // And tmux is no longer writing anywhere.
        s.tmux.send_line(&pane, "echo more").unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert!(!stream.exists());
    }

    #[test]
    fn an_idle_pane_that_has_printed_nothing_is_not_an_error() {
        let Some(s) = Scratch::new("idle") else { return };
        let dir = tempfile::tempdir().unwrap();
        let pane = s.tmux.spawn("quiet", dir.path(), "sleep 20").unwrap();

        let mut mirror = Mirror::attach(&s.tmux, pane, 40, 10).unwrap();
        // tmux only creates the file on the first byte; polling before that
        // must be a quiet no, not a failure.
        assert!(!mirror.poll());
    }
}
