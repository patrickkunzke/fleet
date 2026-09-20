//! The centre pane: the selected agent's session.
//!
//! Two ways of showing one, and the first is preferred wherever it works:
//!
//! 1. **The pane itself**, mirrored from tmux. The real REPL — spinners,
//!    permission prompts, its own colours. This is what an agent running in
//!    our tmux gets.
//! 2. **The transcript**, re-rendered from the jsonl. The only thing that can
//!    show a session which is not in our tmux, or one that has ended, and it
//!    reads back further than a pane's screen. A structured view rather than
//!    the thing itself, so it is the fallback, not the default.
//!
//! The transcript view follows the tail the way a terminal does: new output
//! pushes the view along until you scroll back, and then it holds still until
//! you return to the bottom. Being yanked to the bottom by an agent's next
//! tool call is the single most annoying thing a live log can do.

use anyhow::{Result, bail};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::tmux::{Pane as TmuxPane, Tmux};
use crate::ui::keys::Key;
use crate::transcript::{BACKFILL_BYTES, Entry, Outcome, Transcript};
use crate::ui::fleet::{Presence, Row};
use crate::ui::mirror::Mirror;
use crate::ui::theme;

#[derive(Default)]
pub struct Pane {
    /// The live terminal, when the agent is one of ours in tmux.
    mirror: Option<Mirror>,
    /// Which session this pane is showing, so a changed selection reopens.
    session_id: Option<String>,
    /// Whether `follow` has ever run. Without it the first call on an agent
    /// with no session looks identical to "nothing changed", and the pane
    /// stays blank instead of saying why.
    targeted: bool,
    transcript: Option<Transcript>,
    /// Lines held back from the bottom. Zero means following the tail.
    scroll: usize,
    /// How far back the last draw could have scrolled. Only rendering knows
    /// this — an entry is one line or twenty depending on the pane's width —
    /// so it is recorded there and used to clamp the next keystroke.
    max_scroll: std::cell::Cell<usize>,
    note: Option<String>,
}

impl Pane {
    /// Point the pane at whatever is selected, reopening only on a change.
    pub fn follow(
        &mut self,
        row: Option<&Row>,
        projects_dir: &std::path::Path,
        tmux: Option<&Tmux>,
        area: (u16, u16),
    ) {
        let wanted = row.and_then(|r| r.session_id.clone());
        if self.targeted && wanted == self.session_id {
            return;
        }
        self.targeted = true;
        self.session_id = wanted.clone();
        // Dropping the old mirror is what stops tmux piping the pane we are
        // no longer looking at.
        self.mirror = None;
        self.transcript = None;
        self.scroll = 0;
        self.note = None;

        // The live pane first: it is the actual terminal rather than our
        // reading of it.
        if let (Some(tmux), Some(target)) = (tmux, row.and_then(|r| r.tmux_target.as_deref()))
            && let Ok(Some(pane)) = tmux.find(target)
        {
            match Mirror::attach(tmux, pane, area.0, area.1) {
                Ok(m) => {
                    self.mirror = Some(m);
                    return;
                }
                // Falling through to the transcript is better than an empty
                // pane, so this is a note rather than a failure.
                Err(e) => self.note = Some(format!("cannot mirror the pane: {e}")),
            }
        }

        let Some(id) = wanted else {
            self.note = Some("no session linked".into());
            return;
        };
        // The transcript is found by session id rather than by the agent's
        // repo: an agent that moved, or one adopted from elsewhere, still has
        // exactly one file.
        let path = std::fs::read_dir(projects_dir)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path().join(format!("{id}.jsonl")))
            .find(|p| p.is_file());

        match path {
            Some(p) => match Transcript::open(&p, BACKFILL_BYTES) {
                Ok(t) => self.transcript = Some(t),
                Err(e) => self.note = Some(format!("cannot read the transcript: {e}")),
            },
            None => self.note = Some("no transcript yet".into()),
        }
    }

    /// Read whatever the agent has written since last time. Returns true when
    /// there is something new, so the caller can skip a redraw when there is
    /// not.
    pub fn poll(&mut self) -> bool {
        if let Some(m) = self.mirror.as_mut() {
            return m.poll();
        }
        let Some(t) = self.transcript.as_mut() else {
            return false;
        };
        match t.poll() {
            Ok(added) => {
                // Scrolled back: hold the view where it is by pushing the
                // offset along with the new lines.
                if self.scroll > 0 {
                    self.scroll += added;
                }
                added > 0
            }
            Err(e) => {
                self.note = Some(format!("transcript stopped: {e}"));
                self.transcript = None;
                true
            }
        }
    }

    /// Is the centre showing the real terminal rather than our reading of it?
    pub fn is_live(&self) -> bool {
        self.mirror.is_some()
    }

    /// The tmux pane behind the mirror, when there is one.
    pub fn tmux_pane(&self) -> Option<&TmuxPane> {
        self.mirror.as_ref().map(|m| m.pane())
    }

    /// Deliver one keystroke to the agent.
    pub fn send(&self, tmux: &Tmux, key: &Key) -> Result<()> {
        let Some(pane) = self.tmux_pane() else {
            bail!("this agent has no pane to type into");
        };
        match key {
            Key::Literal(text) => tmux.send_text(pane, text),
            Key::Named(name) => tmux.send_key(pane, name),
        }
    }

    /// Scroll the mirrored screen. Ignored on a transcript, which has its own.
    pub fn scroll_mirror(&mut self, delta: isize) -> bool {
        match self.mirror.as_mut() {
            Some(m) => {
                m.scroll_by(delta);
                true
            }
            None => false,
        }
    }

    pub fn mirror_to_live(&mut self) {
        if let Some(m) = self.mirror.as_mut() {
            m.to_live();
        }
    }

    /// Hand the user the actual pane, from inside tmux, where selecting it is
    /// enough to move them.
    pub fn zoom(&self, tmux: &Tmux) -> Option<String> {
        let m = self.mirror.as_ref()?;
        match tmux.zoom(m.pane()) {
            Ok(()) => None,
            Err(e) => Some(format!("cannot zoom: {e}")),
        }
    }

    /// Give the whole terminal over to the pane until the user detaches.
    ///
    /// What `↵` does when fleet is not itself running inside tmux, which is
    /// most of the time — the point of tmux here is to hold the agents, not
    /// to be somewhere you have to sit.
    pub fn attach(&self, tmux: &Tmux) -> Option<String> {
        let m = self.mirror.as_ref()?;
        tmux.attach(m.pane()).err().map(|e| format!("cannot attach: {e}"))
    }

    pub fn scroll_by(&mut self, delta: isize, page: usize) {
        let max = self.max_scroll.get() as isize;
        let next = self.scroll as isize + delta * page as isize;
        self.scroll = next.clamp(0, max.max(0)) as usize;
    }

    pub fn to_tail(&mut self) {
        self.scroll = 0;
    }

    /// Looking at the live edge, whichever view is up.
    pub fn following(&self) -> bool {
        match &self.mirror {
            Some(m) => !m.scrolled_back(),
            None => self.scroll == 0,
        }
    }

    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        row: Option<&Row>,
        // Whether keystrokes are going here, which the header says so that
        // nobody types into the wrong place.
        focused: bool,
        // A divider only means something when there is a pane on the other
        // side of it; at the screen edge it is a stray line.
        bordered: bool,
    ) {
        let borders = if bordered { Borders::RIGHT } else { Borders::NONE };
        let block = Block::default()
            .borders(borders)
            .border_style(Style::default().fg(theme::BORDER));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let [head, head_rule, body] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(inner);

        self.render_head(frame, head, row, focused);
        theme::rule(frame, head_rule);

        if let Some(m) = self.mirror.as_mut() {
            // No gutter here. Every other pane is text we lay out; this one
            // is a terminal, and a terminal inset inside a box reads as a
            // picture of one.
            m.resize(body.width, body.height);
            m.render(frame, body);
            return;
        }

        let width = body.width.saturating_sub(theme::GUTTER * 2);
        let mut lines: Vec<Line> = Vec::new();
        match (&self.transcript, &self.note) {
            (Some(t), _) => {
                for entry in t.entries() {
                    render_entry(entry, width, &mut lines);
                }
            }
            (None, Some(note)) => lines.push(Line::from(Span::styled(note.clone(), theme::faint()))),
            (None, None) => {}
        }

        // Anchor at the bottom: a session pane that starts at the top of a
        // thousand-line history shows you the least useful part of it.
        let height = body.height as usize;
        let total = lines.len();
        self.max_scroll.set(total.saturating_sub(height));
        let end = total.saturating_sub(self.scroll.min(self.max_scroll.get()));
        let start = end.saturating_sub(height);
        let visible: Vec<Line> = lines[start..end].to_vec();

        frame.render_widget(Paragraph::new(visible), theme::pad(body));
    }

    fn render_head(&self, frame: &mut Frame, area: Rect, row: Option<&Row>, focused: bool) {
        let Some(row) = row else { return };
        let (glyph, colour) = match row.presence {
            Presence::Working => ("●", theme::BUSY),
            Presence::Waiting => ("○", theme::OK),
            _ => ("×", theme::FAINT),
        };

        let mut left = vec![
            Span::styled(glyph, Style::default().fg(colour)),
            Span::raw(" "),
            Span::styled(
                row.name.clone(),
                Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(branch) = &row.branch {
            left.push(Span::styled(format!("  {branch}"), theme::faint()));
        } else {
            left.push(Span::styled(format!("  {}", row.repo), theme::faint()));
        }

        // Measured into the same line rather than drawn over it: a
        // right-aligned widget on top of this one ate the repo name at any
        // width where the two met.
        let right = if !self.following() {
            vec![Span::styled("scrolled back", theme::accent())]
        } else if !self.is_live() && row.session_id.is_some() {
            vec![Span::styled("transcript", theme::faint())]
        } else if focused && self.is_live() {
            vec![Span::styled("typing here", theme::accent())]
        } else {
            Vec::new()
        };

        let inner = theme::pad(area);
        frame.render_widget(
            Paragraph::new(theme::spread(left, right, inner.width)),
            inner,
        );
    }
}

fn render_entry(entry: &Entry, width: u16, out: &mut Vec<Line<'static>>) {
    match entry {
        Entry::Prompt { text, .. } => {
            for (i, line) in wrap(text, width.saturating_sub(2)).into_iter().enumerate() {
                out.push(Line::from(vec![
                    Span::styled(if i == 0 { "› " } else { "  " }, theme::accent()),
                    Span::styled(line, Style::default().fg(theme::TEXT)),
                ]));
            }
            out.push(Line::raw(""));
        }
        Entry::CrossSessionMessage { from, text, .. } => {
            out.push(Line::from(vec![
                Span::styled("← ", theme::accent()),
                Span::styled(from.clone(), Style::default().fg(theme::OK)),
            ]));
            for line in wrap(text, width.saturating_sub(2)) {
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(line, Style::default().fg(theme::TEXT)),
                ]));
            }
            out.push(Line::raw(""));
        }
        Entry::Say { text, .. } => {
            for line in wrap(text, width) {
                out.push(Line::from(Span::styled(
                    line,
                    Style::default().fg(theme::TEXT),
                )));
            }
            out.push(Line::raw(""));
        }
        Entry::Thought { chars, .. } => {
            out.push(Line::from(Span::styled(
                format!("  thought for {}", thousands(*chars)),
                theme::faint(),
            )));
        }
        Entry::Tool {
            name,
            target,
            outcome,
            sidechain,
            ..
        } => {
            let (glyph, colour) = match outcome {
                Outcome::Pending => ("◐", theme::BUSY),
                Outcome::Ok(_) => ("⏺", theme::OK),
                Outcome::Failed(_) => ("⏺", theme::ACCENT),
                Outcome::Background(_) => ("⏵", theme::BUSY),
            };
            let result = match outcome {
                Outcome::Pending => "running".to_string(),
                Outcome::Ok(s) => s.clone(),
                Outcome::Failed(s) => s.clone(),
                Outcome::Background(id) => format!("bg {id}"),
            };
            let result_style = match outcome {
                Outcome::Failed(_) => theme::accent(),
                Outcome::Pending | Outcome::Background(_) => Style::default().fg(theme::BUSY),
                Outcome::Ok(_) => theme::faint(),
            };

            // Name and outcome are fixed; the target absorbs what is left, so
            // the right-hand column stays a column.
            let used = 2 + 7 + 1 + result.chars().count() + 2;
            let room = (width as usize).saturating_sub(used).max(8);
            let target = elide(target, room);
            let gap = (width as usize)
                .saturating_sub(2 + 7 + 1 + target.chars().count() + result.chars().count());

            out.push(Line::from(vec![
                Span::styled(if *sidechain { "  ⌞" } else { " " }, theme::faint()),
                Span::styled(glyph, Style::default().fg(colour)),
                Span::raw(" "),
                Span::styled(format!("{name:<7}"), theme::dim()),
                Span::styled(target, Style::default().fg(theme::TEXT)),
                Span::raw(" ".repeat(gap)),
                Span::styled(result, result_style),
            ]));
        }
        Entry::Turn { secs, .. } => {
            out.push(Line::from(Span::styled(
                format!("  ── {secs:.1}s"),
                theme::faint(),
            )));
            out.push(Line::raw(""));
        }
    }
}

fn thousands(n: usize) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn elide(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let cut: String = s.chars().take(width.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// Wrap on word boundaries, breaking a word only when it cannot fit alone.
///
/// Written here rather than pulled in: the pane needs the line count it is
/// about to draw so it can anchor at the bottom, and a widget that wraps
/// internally will not tell you that.
fn wrap(text: &str, width: u16) -> Vec<String> {
    let width = (width as usize).max(8);
    let mut out = Vec::new();

    for paragraph in text.lines() {
        if paragraph.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let len = line.chars().count();
            let word_len = word.chars().count();
            if line.is_empty() {
                line.push_str(word);
            } else if len + 1 + word_len <= width {
                line.push(' ');
                line.push_str(word);
            } else {
                out.push(std::mem::take(&mut line));
                line.push_str(word);
            }
            // A single word longer than the pane: cut it rather than let the
            // widget clip it silently.
            while line.chars().count() > width {
                let head: String = line.chars().take(width).collect();
                let tail: String = line.chars().skip(width).collect();
                out.push(head);
                line = tail;
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::fleet::Role;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::io::Write;

    fn row(session: Option<&str>) -> Row {
        Row {
            name: "billing-svc".into(),
            role: Role::Worker,
            repo: "billing-service".into(),
            presence: Presence::Working,
            detail: "ENG-2553-2".into(),
            bg_running: 0,
            uptime: None,
            session_id: session.map(str::to_string),
            branch: Some("feature/ENG-2553-2".into()),
            tmux_target: None,
            pid: Some(1),
        }
    }

    /// A projects directory holding one transcript, as Claude Code lays it out.
    fn projects(id: &str, lines: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("-repo-content");
        std::fs::create_dir_all(&project).unwrap();
        let mut f = std::fs::File::create(project.join(format!("{id}.jsonl"))).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        dir
    }

    const PROMPT: &str = r#"{"type":"user","timestamp":"2026-09-20T14:01:00.000Z","message":{"content":"thread the service param through"}}"#;
    const SAY: &str = r#"{"type":"assistant","timestamp":"2026-09-20T14:01:05.000Z","message":{"content":[{"type":"text","text":"I'll add the parameter."}]}}"#;
    const TOOL: &str = r#"{"type":"assistant","timestamp":"2026-09-20T14:01:07.000Z","message":{"content":[{"type":"tool_use","id":"tu_1","name":"Edit","input":{"file_path":"/repo/src/AccountClient.kt"}}]}}"#;
    const RESULT: &str = r#"{"type":"user","timestamp":"2026-09-20T14:01:09.000Z","toolUseResult":{"filePath":"/repo/src/AccountClient.kt","structuredPatch":[{"lines":[" a","+b","-c"]}]},"message":{"content":[{"type":"tool_result","tool_use_id":"tu_1","content":"ok"}]}}"#;

    fn drawn(pane: &mut Pane, row: Option<&Row>, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| pane.render(f, f.area(), row, false, true)).unwrap();
        format!("{}", term.backend())
    }

    #[test]
    fn it_shows_the_selected_agents_transcript() {
        let dir = projects("sess-1", &[PROMPT, SAY, TOOL, RESULT]);
        let mut pane = Pane::default();
        let row = row(Some("sess-1"));
        pane.follow(Some(&row), dir.path(), None, (64, 10));

        let out = drawn(&mut pane, Some(&row), 64, 16);
        assert!(out.contains("billing-svc"), "the header: {out}");
        assert!(out.contains("feature/ENG-2553-2"), "the branch: {out}");
        assert!(out.contains("thread the service param"), "the prompt: {out}");
        assert!(out.contains("I'll add the parameter"), "the prose: {out}");
        assert!(out.contains("Edit"), "the tool: {out}");
        assert!(out.contains("AccountClient.kt"), "its target: {out}");
        assert!(out.contains("+1 -1"), "and its outcome: {out}");
    }

    #[test]
    fn an_unfinished_call_reads_as_running() {
        let dir = projects("sess-1", &[TOOL]);
        let mut pane = Pane::default();
        let row = row(Some("sess-1"));
        pane.follow(Some(&row), dir.path(), None, (64, 10));

        let out = drawn(&mut pane, Some(&row), 64, 10);
        assert!(out.contains("running"), "{out}");
    }

    #[test]
    fn an_agent_with_no_session_says_so_instead_of_showing_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut pane = Pane::default();
        let row = row(None);
        pane.follow(Some(&row), dir.path(), None, (64, 10));

        let out = drawn(&mut pane, Some(&row), 64, 10);
        assert!(out.contains("no session linked"), "{out}");
    }

    #[test]
    fn a_session_whose_transcript_has_not_appeared_yet_says_that_instead() {
        let dir = tempfile::tempdir().unwrap();
        let mut pane = Pane::default();
        let row = row(Some("sess-missing"));
        pane.follow(Some(&row), dir.path(), None, (64, 10));

        let out = drawn(&mut pane, Some(&row), 64, 10);
        assert!(out.contains("no transcript yet"), "{out}");
    }

    #[test]
    fn changing_the_selection_reopens_but_reselecting_the_same_one_does_not() {
        let dir = projects("sess-1", &[PROMPT, SAY, TOOL, RESULT]);
        let mut pane = Pane::default();
        let row = row(Some("sess-1"));

        pane.follow(Some(&row), dir.path(), None, (64, 10));
        drawn(&mut pane, Some(&row), 64, 5); // scrolling is bounded by the last draw
        pane.scroll_by(1, 3);
        let scrolled = pane.scroll;
        assert!(scrolled > 0);

        // Same session: the scroll position is where the user put it.
        pane.follow(Some(&row), dir.path(), None, (64, 10));
        assert_eq!(pane.scroll, scrolled);

        // A different one starts at the tail.
        let elsewhere = super::tests::row(Some("sess-2"));
        pane.follow(Some(&elsewhere), dir.path(), None, (64, 10));
        assert_eq!(pane.scroll, 0);
    }

    #[test]
    fn new_output_does_not_yank_a_scrolled_back_view_to_the_bottom() {
        let dir = projects("sess-1", &[PROMPT, SAY]);
        let path = dir.path().join("-repo-content").join("sess-1.jsonl");
        let mut pane = Pane::default();
        let row = row(Some("sess-1"));
        pane.follow(Some(&row), dir.path(), None, (64, 10));

        drawn(&mut pane, Some(&row), 64, 4);
        pane.scroll_by(1, 1);
        let before = pane.scroll;
        assert!(!pane.following());

        let mut f = std::fs::File::options().append(true).open(&path).unwrap();
        writeln!(f, "{TOOL}").unwrap();
        assert!(pane.poll());

        assert!(
            pane.scroll > before,
            "the offset moves with the new line so the view holds still"
        );

        pane.to_tail();
        assert!(pane.following());
    }

    #[test]
    fn the_header_says_when_you_are_not_looking_at_the_live_edge() {
        let dir = projects("sess-1", &[PROMPT, SAY, TOOL, RESULT]);
        let mut pane = Pane::default();
        let row = row(Some("sess-1"));
        pane.follow(Some(&row), dir.path(), None, (64, 10));

        assert!(!drawn(&mut pane, Some(&row), 64, 6).contains("scrolled back"));
        pane.scroll_by(1, 2);
        assert!(drawn(&mut pane, Some(&row), 64, 6).contains("scrolled back"));
    }

    #[test]
    fn a_transcript_only_agent_has_nothing_to_type_into() {
        let dir = tempfile::tempdir().unwrap();
        let mut pane = Pane::default();
        let row = row(Some("sess-missing"));
        pane.follow(Some(&row), dir.path(), None, (64, 10));

        assert!(pane.tmux_pane().is_none());
        let out = drawn(&mut pane, Some(&row), 64, 10);
        assert!(
            !out.contains("typing here"),
            "nothing claims to take keystrokes where there is no pane: {out}"
        );
    }

    #[test]
    fn wrapping_breaks_on_words_and_only_splits_one_that_cannot_fit() {
        assert_eq!(
            wrap("the resolver prefers the explicit service argument", 20),
            vec!["the resolver prefers", "the explicit service", "argument"]
        );
        // Long enough that no boundary helps.
        let long = "a".repeat(25);
        assert_eq!(wrap(&long, 10), vec!["aaaaaaaaaa", "aaaaaaaaaa", "aaaaa"]);
        // Blank lines in the source survive as blank lines.
        assert_eq!(wrap("one\n\ntwo", 20), vec!["one", "", "two"]);
    }

    #[test]
    fn it_draws_in_a_narrow_pane_without_panicking() {
        let dir = projects("sess-1", &[PROMPT, SAY, TOOL, RESULT]);
        let mut pane = Pane::default();
        let row = row(Some("sess-1"));
        pane.follow(Some(&row), dir.path(), None, (64, 10));

        for (w, h) in [(20, 4), (30, 6), (200, 50)] {
            let _ = drawn(&mut pane, Some(&row), w, h);
        }
    }
}
