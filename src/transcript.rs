//! Reading a session's transcript — `~/.claude/projects/<slug>/<id>.jsonl`.
//!
//! One JSON object per line, appended as the session runs. This module turns
//! that into the handful of things the centre pane actually shows, and follows
//! the file as it grows.
//!
//! Most of the file is not for us. In a real session roughly half the records
//! are `attachment` — hook results and token reminders — and are dropped here
//! rather than in the UI, because the cheapest line to render is the one that
//! never becomes an entry.
//!
//! Tool calls and their results arrive as separate records, so a call is
//! emitted [`Outcome::Pending`] and filled in when its result lands. That is
//! why [`Transcript`] owns its entries instead of handing them back: the fix-up
//! has to reach an entry that was emitted on an earlier poll.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

/// How much of an existing transcript to read when opening one. Enough for a
/// screenful of history on a busy session; the alternative is parsing an
/// 11 MB file to show forty lines.
pub const BACKFILL_BYTES: u64 = 256 * 1024;

/// How many entries to keep. Older ones fall off the top.
pub const MAX_ENTRIES: usize = 2000;

/// How far back to look for the call a result belongs to. Results follow their
/// call almost immediately; a bounded scan avoids keeping an index in step
/// with a deque that drops from the front.
const PAIR_WINDOW: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The call is out; no result yet. Rendered as "running".
    Pending,
    /// Finished, with a short summary: "37 lines", "+12 -3".
    Ok(String),
    Failed(String),
    /// Handed off to a background task with this id.
    Background(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// Something the user typed.
    Prompt { ts: String, text: String },
    /// A message from another session, which is also a flow-pane edge.
    CrossSessionMessage {
        ts: String,
        from: String,
        text: String,
    },
    /// Assistant prose.
    Say { ts: String, text: String },
    /// Thinking, kept as a size rather than content — the pane collapses it.
    Thought { ts: String, chars: usize },
    Tool {
        ts: String,
        id: String,
        name: String,
        /// The one identifying argument: a file name, a command, a url.
        target: String,
        outcome: Outcome,
        /// True when a subagent did this, not the session itself.
        sidechain: bool,
    },
    /// A turn finished, with how long it took.
    Turn { ts: String, secs: f64 },
}

impl Entry {
    pub fn ts(&self) -> &str {
        match self {
            Entry::Prompt { ts, .. }
            | Entry::CrossSessionMessage { ts, .. }
            | Entry::Say { ts, .. }
            | Entry::Thought { ts, .. }
            | Entry::Tool { ts, .. }
            | Entry::Turn { ts, .. } => ts,
        }
    }

    /// "14:11" out of an RFC 3339 timestamp, without pulling in a date crate
    /// to render five characters. Note this is UTC, as written.
    pub fn hhmm(&self) -> &str {
        self.ts().get(11..16).unwrap_or("")
    }
}

pub struct Transcript {
    path: PathBuf,
    file: File,
    offset: u64,
    /// Bytes after the last newline — a line the writer has not finished.
    partial: Vec<u8>,
    entries: VecDeque<Entry>,
}

impl Transcript {
    /// Open a transcript, reading back at most `backfill` bytes of history.
    pub fn open(path: impl AsRef<Path>, backfill: u64) -> Result<Transcript> {
        let path = path.as_ref().to_path_buf();
        let mut file =
            File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let len = file.metadata()?.len();

        let start = len.saturating_sub(backfill);
        file.seek(SeekFrom::Start(start))?;

        let mut t = Transcript {
            path,
            file,
            offset: start,
            partial: Vec::new(),
            entries: VecDeque::new(),
        };
        // Seeking into the middle of the file almost certainly lands inside a
        // line; drop whatever is left of it rather than parsing a fragment.
        let skip_first = start > 0;
        t.read_available(skip_first)?;
        Ok(t)
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read whatever has been appended. Returns how many entries were added.
    pub fn poll(&mut self) -> Result<usize> {
        let len = match self.file.metadata() {
            Ok(m) => m.len(),
            // The file can vanish under us if a project directory is cleaned
            // up; keep what we have rather than failing the pane.
            Err(_) => return Ok(0),
        };

        if len < self.offset {
            // Truncated or replaced. Start over rather than reading garbage
            // from the middle of whatever is there now.
            self.file = File::open(&self.path)?;
            self.offset = 0;
            self.partial.clear();
            self.entries.clear();
        }

        let before = self.entries.len();
        self.read_available(false)?;
        Ok(self.entries.len().saturating_sub(before))
    }

    fn read_available(&mut self, skip_first_line: bool) -> Result<()> {
        let mut buf = Vec::new();
        self.file.seek(SeekFrom::Start(self.offset))?;
        let read = self.file.read_to_end(&mut buf)?;
        if read == 0 {
            return Ok(());
        }
        self.offset += read as u64;

        self.partial.extend_from_slice(&buf);
        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut start = 0;
        for (i, b) in self.partial.iter().enumerate() {
            if *b == b'\n' {
                lines.push(self.partial[start..i].to_vec());
                start = i + 1;
            }
        }
        self.partial.drain(..start);

        for (i, line) in lines.iter().enumerate() {
            if skip_first_line && i == 0 {
                continue;
            }
            if line.is_empty() {
                continue;
            }
            // A line that does not parse is a line the writer was still
            // flushing, or a shape we do not know. Neither is worth an error.
            if let Ok(value) = serde_json::from_slice::<Value>(line) {
                self.ingest(&value);
            }
        }
        Ok(())
    }

    fn push(&mut self, entry: Entry) {
        if self.entries.len() >= MAX_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    fn ingest(&mut self, v: &Value) {
        let ts = v["timestamp"].as_str().unwrap_or_default().to_string();
        let sidechain = v["isSidechain"].as_bool().unwrap_or(false);

        match v["type"].as_str() {
            Some("user") => self.ingest_user(v, &ts),
            Some("assistant") => self.ingest_assistant(v, &ts, sidechain),
            Some("system") => {
                if v["subtype"].as_str() == Some("turn_duration")
                    && let Some(ms) = v["durationMs"].as_f64()
                {
                    self.push(Entry::Turn {
                        ts,
                        secs: ms / 1000.0,
                    });
                }
            }
            // attachment, permission-mode, ai-title, file-history-* and the
            // rest are bookkeeping the pane has no use for.
            _ => {}
        }
    }

    fn ingest_user(&mut self, v: &Value, ts: &str) {
        let content = &v["message"]["content"];

        if let Some(text) = content.as_str() {
            self.push(prompt_entry(ts, text));
            return;
        }

        for block in content.as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("text") => {
                    if let Some(text) = block["text"].as_str() {
                        self.push(prompt_entry(ts, text));
                    }
                }
                Some("tool_result") => {
                    let id = block["tool_use_id"].as_str().unwrap_or_default();
                    let failed = block["is_error"].as_bool().unwrap_or(false);
                    let outcome = summarize(&v["toolUseResult"], block, failed);
                    self.resolve(id, outcome);
                }
                _ => {}
            }
        }
    }

    fn ingest_assistant(&mut self, v: &Value, ts: &str, sidechain: bool) {
        for block in v["message"]["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("text") => {
                    if let Some(text) = block["text"].as_str()
                        && !text.trim().is_empty()
                    {
                        self.push(Entry::Say {
                            ts: ts.to_string(),
                            text: text.to_string(),
                        });
                    }
                }
                Some("thinking") => {
                    let chars = block["thinking"].as_str().unwrap_or_default().chars().count();
                    if chars > 0 {
                        self.push(Entry::Thought {
                            ts: ts.to_string(),
                            chars,
                        });
                    }
                }
                Some("tool_use") => {
                    let name = block["name"].as_str().unwrap_or("?").to_string();
                    let target = describe(&name, &block["input"]);
                    self.push(Entry::Tool {
                        ts: ts.to_string(),
                        id: block["id"].as_str().unwrap_or_default().to_string(),
                        name,
                        target,
                        outcome: Outcome::Pending,
                        sidechain,
                    });
                }
                _ => {}
            }
        }
    }

    /// Attach a result to the call it belongs to.
    fn resolve(&mut self, id: &str, outcome: Outcome) {
        if id.is_empty() {
            return;
        }
        for entry in self.entries.iter_mut().rev().take(PAIR_WINDOW) {
            if let Entry::Tool {
                id: tool_id,
                outcome: slot,
                ..
            } = entry
                && tool_id == id
            {
                *slot = outcome;
                return;
            }
        }
    }

    #[allow(dead_code)]
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The most recent `n` entries, oldest first — what the pane draws.
    pub fn tail(&self, n: usize) -> impl Iterator<Item = &Entry> {
        self.entries.iter().skip(self.entries.len().saturating_sub(n))
    }
}

/// A prompt, unless it is a message another session sent us — those carry
/// their sender in a wrapper and belong on the flow graph as an edge.
fn prompt_entry(ts: &str, text: &str) -> Entry {
    if let Some(from) = attr_value(text, "<cross-session-message", "from") {
        let body = between(text, '>', "</cross-session-message>").unwrap_or(text);
        return Entry::CrossSessionMessage {
            ts: ts.to_string(),
            from,
            text: body.trim().to_string(),
        };
    }
    Entry::Prompt {
        ts: ts.to_string(),
        text: text.to_string(),
    }
}

fn attr_value(text: &str, tag: &str, attr: &str) -> Option<String> {
    let start = text.find(tag)?;
    let rest = &text[start..];
    let key = format!("{attr}=\"");
    let at = rest.find(&key)? + key.len();
    let end = rest[at..].find('"')? + at;
    Some(rest[at..end].to_string())
}

fn between<'a>(text: &'a str, open: char, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len_utf8();
    let end = text.find(close)?;
    if end < start { None } else { Some(&text[start..end]) }
}

/// The one argument worth showing next to a tool name.
fn describe(name: &str, input: &Value) -> String {
    let file = |key: &str| {
        input[key]
            .as_str()
            .map(|p| {
                Path::new(p)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(p)
                    .to_string()
            })
            .unwrap_or_default()
    };

    let raw = match name {
        "Bash" => input["description"]
            .as_str()
            .or_else(|| input["command"].as_str())
            .unwrap_or_default()
            .to_string(),
        "Read" | "Write" | "Edit" | "NotebookEdit" => file("file_path"),
        "Glob" | "Grep" => input["pattern"].as_str().unwrap_or_default().to_string(),
        "WebFetch" | "WebSearch" => input["url"]
            .as_str()
            .or_else(|| input["query"].as_str())
            .unwrap_or_default()
            .to_string(),
        "Task" | "Agent" => input["description"].as_str().unwrap_or_default().to_string(),
        "Skill" => input["skill"].as_str().unwrap_or_default().to_string(),
        "SendMessage" => input["to"].as_str().unwrap_or_default().to_string(),
        "Artifact" => input["action"].as_str().unwrap_or("publish").to_string(),
        _ => input
            .as_object()
            .and_then(|o| o.values().find_map(|v| v.as_str()))
            .unwrap_or_default()
            .to_string(),
    };

    // One line, and short enough not to push the outcome off the pane.
    let line = raw.lines().next().unwrap_or_default();
    if line.chars().count() > 60 {
        let cut: String = line.chars().take(59).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}

/// Turn a result payload into the short string the pane shows on the right.
fn summarize(result: &Value, block: &Value, failed: bool) -> Outcome {
    if failed {
        let text = block["content"].as_str().unwrap_or("failed");
        return Outcome::Failed(first_line(text, 50));
    }
    if let Some(id) = result["backgroundTaskId"].as_str() {
        return Outcome::Background(id.to_string());
    }
    if let Some(hunks) = result["structuredPatch"].as_array()
        && !hunks.is_empty()
    {
        let (mut added, mut removed) = (0usize, 0usize);
        for hunk in hunks {
            for line in hunk["lines"].as_array().into_iter().flatten() {
                match line.as_str().and_then(|s| s.chars().next()) {
                    Some('+') => added += 1,
                    Some('-') => removed += 1,
                    _ => {}
                }
            }
        }
        return Outcome::Ok(format!("+{added} -{removed}"));
    }
    // A Write reports the file it wrote alongside a one-line confirmation.
    // Count what landed, not the confirmation.
    if result["filePath"].is_string()
        && let Some(content) = result["content"].as_str()
    {
        return Outcome::Ok(lines_label(content));
    }
    if let Some(out) = result["stdout"].as_str() {
        return Outcome::Ok(lines_label(out));
    }
    // Some results are a bare string, and some tools report nothing at all.
    let text = result
        .as_str()
        .or_else(|| block["content"].as_str())
        .unwrap_or_default();
    if text.is_empty() {
        Outcome::Ok("ok".into())
    } else {
        Outcome::Ok(lines_label(text))
    }
}

fn lines_label(text: &str) -> String {
    let trimmed = text.trim_end_matches('\n');
    if trimmed.is_empty() {
        return "ok".into();
    }
    match trimmed.lines().count() {
        1 => "1 line".into(),
        n => format!("{n} lines"),
    }
}

fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() > max {
        let cut: String = line.chars().take(max - 1).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Records shaped like the real ones, down to the key names.
    fn sample() -> Vec<String> {
        vec![
            // Noise: about half of a real transcript looks like this.
            r#"{"type":"attachment","attachment":{"type":"hook_success"},"timestamp":"2026-09-20T14:00:00.000Z"}"#.into(),
            r#"{"type":"user","isSidechain":false,"timestamp":"2026-09-20T14:01:00.000Z","message":{"content":"thread the service param through"}}"#.into(),
            r#"{"type":"assistant","isSidechain":false,"timestamp":"2026-09-20T14:01:05.000Z","message":{"content":[{"type":"thinking","thinking":"twelve chars"},{"type":"text","text":"I'll add the parameter."}]}}"#.into(),
            r#"{"type":"assistant","isSidechain":false,"timestamp":"2026-09-20T14:01:07.000Z","message":{"content":[{"type":"tool_use","id":"tu_1","name":"Edit","input":{"file_path":"/repo/src/AccountClient.kt","old_string":"a","new_string":"b"}}]}}"#.into(),
            r#"{"type":"user","isSidechain":false,"timestamp":"2026-09-20T14:01:09.000Z","toolUseResult":{"filePath":"/repo/src/AccountClient.kt","structuredPatch":[{"oldStart":1,"oldLines":3,"newStart":1,"newLines":4,"lines":[" ctx","+added one","+added two","-removed one"]}]},"message":{"content":[{"type":"tool_result","tool_use_id":"tu_1","content":"ok","is_error":false}]}}"#.into(),
            r#"{"type":"assistant","isSidechain":false,"timestamp":"2026-09-20T14:02:00.000Z","message":{"content":[{"type":"tool_use","id":"tu_2","name":"Bash","input":{"command":"./gradlew test","description":"Run the service tests","run_in_background":true}}]}}"#.into(),
            r#"{"type":"user","isSidechain":false,"timestamp":"2026-09-20T14:02:01.000Z","toolUseResult":{"stdout":"","stderr":"","interrupted":false,"backgroundTaskId":"bvba8nf94"},"message":{"content":[{"type":"tool_result","tool_use_id":"tu_2","content":"started"}]}}"#.into(),
            r#"{"type":"system","subtype":"turn_duration","durationMs":4200,"timestamp":"2026-09-20T14:02:05.000Z"}"#.into(),
        ]
    }

    fn write_lines(path: &Path, lines: &[String]) {
        let mut f = File::create(path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    #[test]
    fn parses_the_shapes_a_session_actually_writes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        write_lines(&p, &sample());

        let t = Transcript::open(&p, BACKFILL_BYTES).unwrap();
        let entries: Vec<_> = t.entries().collect();

        assert_eq!(entries.len(), 6, "the attachment record must be dropped");
        assert!(matches!(entries[0], Entry::Prompt { .. }));
        assert!(matches!(entries[1], Entry::Thought { chars: 12, .. }));
        assert!(matches!(entries[2], Entry::Say { .. }));
        assert_eq!(entries[0].hhmm(), "14:01");

        match entries[3] {
            Entry::Tool {
                name,
                target,
                outcome,
                ..
            } => {
                assert_eq!(name, "Edit");
                assert_eq!(target, "AccountClient.kt", "the path is shown as a basename");
                assert_eq!(*outcome, Outcome::Ok("+2 -1".into()));
            }
            other => panic!("expected a tool entry, got {other:?}"),
        }

        match entries[4] {
            Entry::Tool {
                target,
                outcome,
                ..
            } => {
                assert_eq!(target, "Run the service tests", "description beats command");
                assert_eq!(*outcome, Outcome::Background("bvba8nf94".into()));
            }
            other => panic!("expected a tool entry, got {other:?}"),
        }

        assert!(matches!(entries[5], Entry::Turn { secs, .. } if (secs - 4.2).abs() < 1e-9));
    }

    #[test]
    fn follows_appended_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        write_lines(&p, &sample()[..2]);

        let mut t = Transcript::open(&p, BACKFILL_BYTES).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t.poll().unwrap(), 0, "nothing new is quiet");

        let mut f = File::options().append(true).open(&p).unwrap();
        writeln!(f, "{}", sample()[2]).unwrap();
        assert_eq!(t.poll().unwrap(), 2, "thinking and text");
    }

    #[test]
    fn a_line_split_across_two_writes_is_held_until_complete() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        File::create(&p).unwrap();

        let mut t = Transcript::open(&p, BACKFILL_BYTES).unwrap();
        let whole = sample()[1].clone();
        let (head, tail) = whole.split_at(40);

        let mut f = File::options().append(true).open(&p).unwrap();
        write!(f, "{head}").unwrap();
        f.flush().unwrap();
        assert_eq!(t.poll().unwrap(), 0, "half a record is not an entry");

        write!(f, "{tail}\n").unwrap();
        f.flush().unwrap();
        assert_eq!(t.poll().unwrap(), 1, "and lands once the newline arrives");
    }

    #[test]
    fn a_truncated_file_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        write_lines(&p, &sample());

        let mut t = Transcript::open(&p, BACKFILL_BYTES).unwrap();
        assert_eq!(t.len(), 6);

        write_lines(&p, &sample()[1..3]);
        t.poll().unwrap();
        assert_eq!(t.len(), 3, "re-read from the beginning, not from the old offset");
    }

    #[test]
    fn backfill_drops_the_partial_line_it_lands_in() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        let lines = sample();
        write_lines(&p, &lines);

        // Reach back over the last two records and 20 bytes into the third,
        // so the seek lands mid-record the way a real backfill does.
        let last_two: u64 = lines[6..].iter().map(|l| l.len() as u64 + 1).sum();
        let t = Transcript::open(&p, last_two + 20).unwrap();

        let entries: Vec<_> = t.entries().collect();
        assert_eq!(
            entries.len(),
            1,
            "the straddled record is dropped, and the tool_result left orphaned \
             by it adds nothing: {entries:?}"
        );
        assert!(matches!(entries[0], Entry::Turn { .. }));
    }

    #[test]
    fn a_message_from_another_session_is_an_edge_not_a_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        write_lines(
            &p,
            &[r#"{"type":"user","timestamp":"2026-09-20T14:11:32.000Z","message":{"content":"<cross-session-message from=\"billing-service\">blocked: needs !412 merged</cross-session-message>"}}"#.into()],
        );

        let t = Transcript::open(&p, BACKFILL_BYTES).unwrap();
        match t.entries().next().unwrap() {
            Entry::CrossSessionMessage { from, text, .. } => {
                assert_eq!(from, "billing-service");
                assert_eq!(text, "blocked: needs !412 merged");
            }
            other => panic!("expected a cross-session message, got {other:?}"),
        }
    }

    #[test]
    fn a_write_reports_the_file_not_the_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        write_lines(&p, &[
            r#"{"type":"assistant","timestamp":"2026-09-20T14:04:00.000Z","message":{"content":[{"type":"tool_use","id":"tu_3","name":"Write","input":{"file_path":"/repo/src/registry.rs","content":"a\nb\nc"}}]}}"#.into(),
            r#"{"type":"user","timestamp":"2026-09-20T14:04:01.000Z","toolUseResult":{"type":"create","filePath":"/repo/src/registry.rs","content":"a\nb\nc","structuredPatch":[]},"message":{"content":[{"type":"tool_result","tool_use_id":"tu_3","content":"File created successfully at: /repo/src/registry.rs"}]}}"#.into(),
        ]);

        let t = Transcript::open(&p, BACKFILL_BYTES).unwrap();
        match t.entries().next().unwrap() {
            Entry::Tool { target, outcome, .. } => {
                assert_eq!(target, "registry.rs");
                assert_eq!(*outcome, Outcome::Ok("3 lines".into()), "an empty patch must not read as +0 -0");
            }
            other => panic!("expected a tool entry, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_tool_keeps_its_reason() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        write_lines(&p, &[
            r#"{"type":"assistant","timestamp":"2026-09-20T14:03:00.000Z","message":{"content":[{"type":"tool_use","id":"tu_9","name":"Bash","input":{"command":"cargo test","description":"Run the tests"}}]}}"#.into(),
            r#"{"type":"user","timestamp":"2026-09-20T14:03:30.000Z","toolUseResult":"error","message":{"content":[{"type":"tool_result","tool_use_id":"tu_9","is_error":true,"content":"error: could not compile `fleet`\nmore detail"}]}}"#.into(),
        ]);

        let t = Transcript::open(&p, BACKFILL_BYTES).unwrap();
        match t.entries().next().unwrap() {
            Entry::Tool { outcome, .. } => {
                assert_eq!(*outcome, Outcome::Failed("error: could not compile `fleet`".into()))
            }
            other => panic!("expected a tool entry, got {other:?}"),
        }
    }
}
