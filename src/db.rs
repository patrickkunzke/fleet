//! `~/.claude-fleet/fleet.db` — the fleet's coordination state.
//!
//! Tasks, agents, the task DAG, background processes, and the semantic flow
//! log. Deliberately not a memory store: claude-mem keeps what is worth
//! remembering next month, the transcripts keep raw activity, and this keeps
//! what another agent has to act on right now.
//!
//! Every state change routes through [`Db::transition`], which writes the
//! matching flow event in the same transaction. That is the whole reason the
//! panes and the history cannot drift apart, and it is why nothing else in the
//! crate updates `tasks.state` directly.

use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

/// The schema travels in the binary, so `fleet board init` needs no checkout.
const SCHEMA: &str = include_str!("../schema.sql");

pub fn default_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    std::path::PathBuf::from(home)
        .join(".claude-fleet")
        .join("fleet.db")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Queued,
    Running,
    Blocked,
    Review,
    Done,
    Dropped,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Running => "running",
            State::Blocked => "blocked",
            State::Review => "review",
            State::Done => "done",
            State::Dropped => "dropped",
        }
    }

    pub fn parse(s: &str) -> Result<State> {
        Ok(match s {
            "queued" => State::Queued,
            "running" => State::Running,
            "blocked" => State::Blocked,
            "review" => State::Review,
            "done" => State::Done,
            "dropped" => State::Dropped,
            other => bail!("unknown task state '{other}'"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub key: String,
    pub title: String,
    pub state: State,
    pub repo: String,
    pub epic_key: Option<String>,
    pub agent: Option<String>,
    pub mr_url: Option<String>,
    pub blocked_on: Option<String>,
    /// Dependencies that are not done yet, as keys.
    pub waiting_on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub name: String,
    pub role: String,
    pub repo: Option<String>,
    pub session_id: Option<String>,
    pub tmux_target: Option<String>,
    pub branch: Option<String>,
    pub task_key: Option<String>,
    pub task_title: Option<String>,
    pub bg_running: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgTask {
    pub id: i64,
    pub agent: Option<String>,
    pub repo: Option<String>,
    pub command: String,
    pub kind: String,
    pub port: Option<i64>,
    pub state: String,
    pub detail: Option<String>,
    pub started_at: String,
    /// How long it has been running, or ran for. Computed by SQLite so that
    /// reading a timestamp back does not need a date library for the sake of
    /// one subtraction.
    pub elapsed_secs: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub ts: String,
    pub kind: String,
    pub from_agent: Option<String>,
    pub to_agent: Option<String>,
    pub task_key: Option<String>,
    pub summary: String,
    pub body: Option<String>,
}

/// What a new task needs. Everything optional has a sane empty meaning.
// The write path below is exercised by the tests, and goes live when the board
// subcommands move off cli/board.sh in one step.
#[allow(dead_code)]
#[derive(Debug, Default, Clone)]
pub struct NewTask<'a> {
    pub key: &'a str,
    pub title: &'a str,
    pub repo: &'a str,
    pub epic: Option<&'a str>,
    pub body: Option<&'a str>,
    pub deps: &'a [&'a str],
    pub position: i64,
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Db> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        Db::prepare(conn)
    }

    /// Tests only — the real database is a file several processes share.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Db> {
        Db::prepare(Connection::open_in_memory()?)
    }

    fn prepare(conn: Connection) -> Result<Db> {
        // Several agents and the TUI hold this file open at once. WAL lets
        // readers run while a writer commits; the timeout covers the moment
        // two agents report at the same instant.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA).context("applying the schema")?;
        Ok(Db { conn })
    }

    // ------------------------------------------------------------- reads ---

    pub fn board(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, title, state, repo, epic_key, agent, mr_url, blocked_on, waiting_on
             FROM v_board",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let waiting: Option<String> = r.get(8)?;
                Ok(Task {
                    key: r.get(0)?,
                    title: r.get(1)?,
                    state: State::parse(&r.get::<_, String>(2)?)
                        .map_err(|e| rusqlite::Error::InvalidColumnName(e.to_string()))?,
                    repo: r.get(3)?,
                    epic_key: r.get(4)?,
                    agent: r.get(5)?,
                    mr_url: r.get(6)?,
                    blocked_on: r.get(7)?,
                    waiting_on: waiting
                        .map(|s| s.split(", ").map(str::to_string).collect())
                        .unwrap_or_default(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The dispatch queue: queued tasks whose dependencies are all done.
    pub fn ready(&self) -> Result<Vec<Task>> {
        Ok(self
            .board()?
            .into_iter()
            .filter(|t| t.state == State::Queued && t.waiting_on.is_empty())
            .collect())
    }

    pub fn agents(&self) -> Result<Vec<Agent>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, role, repo, session_id, tmux_target, branch,
                    task_key, task_title, bg_running
             FROM v_agents",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Agent {
                    name: r.get(0)?,
                    role: r.get(1)?,
                    repo: r.get(2)?,
                    session_id: r.get(3)?,
                    tmux_target: r.get(4)?,
                    branch: r.get(5)?,
                    task_key: r.get(6)?,
                    task_title: r.get(7)?,
                    bg_running: r.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Running background processes, plus anything that ended in the last
    /// hour — a build that just failed is still what you want to see.
    pub fn background(&self) -> Result<Vec<BgTask>> {
        let mut stmt = self.conn.prepare(
            "SELECT b.id, a.name, b.repo, b.command, b.kind, b.port, b.state, b.detail,
                    b.started_at,
                    CAST(strftime('%s', COALESCE(b.ended_at, 'now')) -
                         strftime('%s', b.started_at) AS INTEGER)
             FROM bg_tasks b LEFT JOIN agents a ON a.id = b.agent_id
             WHERE b.state = 'running' OR b.ended_at > datetime('now', '-1 hour')
             ORDER BY b.started_at DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(BgTask {
                    id: r.get(0)?,
                    agent: r.get(1)?,
                    repo: r.get(2)?,
                    command: r.get(3)?,
                    kind: r.get(4)?,
                    port: r.get(5)?,
                    state: r.get(6)?,
                    detail: r.get(7)?,
                    started_at: r.get(8)?,
                    elapsed_secs: r.get::<_, Option<i64>>(9)?.unwrap_or(0),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The flow log, newest first.
    ///
    /// Ties on `id` as well as time: two events a few microseconds apart share
    /// a millisecond timestamp, and ordering by time alone lets a task look as
    /// though it finished before it started.
    pub fn events(&self, limit: usize) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, kind, from_agent, to_agent, task_key, summary, body
             FROM events ORDER BY ts DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit as i64], |r| {
                Ok(Event {
                    ts: r.get(0)?,
                    kind: r.get(1)?,
                    from_agent: r.get(2)?,
                    to_agent: r.get(3)?,
                    task_key: r.get(4)?,
                    summary: r.get(5)?,
                    body: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ------------------------------------------------------------ writes ---
    // Tested, not yet reachable from the CLI — see the note on NewTask.

    #[allow(dead_code)]
    pub fn upsert_epic(&self, key: &str, title: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO epics (key, title) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET title = excluded.title",
            params![key, title],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn add_task(&self, t: &NewTask) -> Result<()> {
        if let Some(epic) = t.epic {
            self.conn.execute(
                "INSERT OR IGNORE INTO epics (key, title) VALUES (?1, ?1)",
                params![epic],
            )?;
        }
        self.conn.execute(
            "INSERT INTO tasks (key, epic_id, title, body, repo, position)
             VALUES (?1, (SELECT id FROM epics WHERE key = ?2), ?3, ?4, ?5, ?6)",
            params![t.key, t.epic, t.title, t.body, t.repo, t.position],
        )?;
        for dep in t.deps {
            self.add_dep(t.key, dep)?;
        }
        self.log_event("task", Some("chief"), None, Some(t.key), &format!("queued: {}", t.title), None, None)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn add_dep(&self, task: &str, depends_on: &str) -> Result<()> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO task_deps (task_id, depends_on)
             SELECT t.id, d.id FROM tasks t, tasks d WHERE t.key = ?1 AND d.key = ?2",
            params![task, depends_on],
        )?;
        if n == 0 && !self.dep_exists(task, depends_on)? {
            bail!("cannot add dependency: '{task}' or '{depends_on}' does not exist");
        }
        Ok(())
    }

    fn dep_exists(&self, task: &str, depends_on: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM task_deps d
             JOIN tasks t ON t.id = d.task_id JOIN tasks p ON p.id = d.depends_on
             WHERE t.key = ?1 AND p.key = ?2",
            params![task, depends_on],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(dead_code)]
    pub fn upsert_agent(
        &self,
        name: &str,
        role: Option<&str>,
        repo: Option<&str>,
        session_id: Option<&str>,
        tmux_target: Option<&str>,
        branch: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO agents (name, role, repo) VALUES (?1, ?2, ?3)",
            params![name, role.unwrap_or("worker"), repo],
        )?;
        // COALESCE so a later call that only carries a session id does not
        // wipe the repo an earlier one set.
        self.conn.execute(
            "UPDATE agents SET
               role        = COALESCE(?2, role),
               repo        = COALESCE(?3, repo),
               session_id  = COALESCE(?4, session_id),
               tmux_target = COALESCE(?5, tmux_target),
               branch      = COALESCE(?6, branch),
               ended_at    = NULL
             WHERE name = ?1",
            params![name, role, repo, session_id, tmux_target, branch],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn retire_agent(&self, name: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE agents SET ended_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE name = ?1",
            params![name],
        )?;
        if n == 0 {
            bail!("unknown agent '{name}'");
        }
        self.log_event("note", Some(name), None, None, "agent retired", None, None)
    }

    #[allow(dead_code)]
    pub fn claim(&self, task: &str, agent: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE tasks SET agent_id = (SELECT id FROM agents WHERE name = ?2)
             WHERE key = ?1 AND EXISTS (SELECT 1 FROM agents WHERE name = ?2)",
            params![task, agent],
        )?;
        if n == 0 {
            bail!("cannot claim: no task '{task}' or no agent '{agent}'");
        }
        self.log_event(
            "task",
            Some("chief"),
            Some(agent),
            Some(task),
            &format!("assigned to {agent}"),
            None,
            None,
        )
    }

    /// Move a task and record it. Returns the tasks this freed, which is only
    /// ever non-empty for a move to `done` — that list is the chief's cue to
    /// dispatch again, so it is a return value rather than something the
    /// caller has to remember to ask for.
    #[allow(dead_code)]
    pub fn transition(&self, task: &str, to: State, reason: Option<&str>) -> Result<Vec<String>> {
        // The previous state decides the wording, and the wording has to match
        // what cli/board.sh writes: both implementations feed one flow log, so
        // "started" and "running" cannot both mean the same move.
        let before: Option<(Option<String>, String)> = self
            .conn
            .query_row(
                "SELECT a.name, t.state FROM tasks t LEFT JOIN agents a ON a.id = t.agent_id
                 WHERE t.key = ?1",
                params![task],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((owner, from)) = before else {
            bail!("unknown task '{task}'");
        };

        let n = self.conn.execute(
            "UPDATE tasks SET
               state      = ?2,
               blocked_on = CASE WHEN ?2 = 'blocked' THEN ?3 ELSE NULL END,
               started_at = CASE WHEN ?2 = 'running' AND started_at IS NULL
                                 THEN strftime('%Y-%m-%dT%H:%M:%SZ','now') ELSE started_at END,
               done_at    = CASE WHEN ?2 = 'done'
                                 THEN strftime('%Y-%m-%dT%H:%M:%SZ','now') ELSE NULL END
             WHERE key = ?1",
            params![task, to.as_str(), reason],
        )?;
        debug_assert_eq!(n, 1);

        let summary = match (to, reason) {
            (State::Running, _) if from == "blocked" => "unblocked".to_string(),
            (State::Running, _) => "started".to_string(),
            (State::Blocked, Some(why)) => format!("blocked: {why}"),
            (State::Review, Some(mr)) => format!("ready for review - {mr}"),
            (State::Review, None) => "ready for review".to_string(),
            (State::Done, _) => "done".to_string(),
            (State::Dropped, Some(why)) => format!("dropped: {why}"),
            (state, _) => state.as_str().to_string(),
        };
        self.log_event("task", owner.as_deref(), None, Some(task), &summary, None, None)?;

        if to != State::Done {
            return Ok(Vec::new());
        }
        self.freed_by(task)
    }

    /// Tasks that were waiting on `task` and now have nothing left to wait for.
    ///
    /// Covers `blocked` as well as `queued`: an agent may have parked its task
    /// itself before the dependency landed, and that one still needs waking.
    fn freed_by(&self, task: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.key FROM tasks t
             WHERE t.state IN ('queued','blocked')
               AND t.id IN (SELECT task_id FROM task_deps
                            WHERE depends_on = (SELECT id FROM tasks WHERE key = ?1))
               AND NOT EXISTS (SELECT 1 FROM task_deps d JOIN tasks p ON p.id = d.depends_on
                               WHERE d.task_id = t.id AND p.state <> 'done')
             ORDER BY t.position, t.id",
        )?;
        let rows = stmt
            .query_map(params![task], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    #[allow(dead_code)]
    pub fn set_mr(&self, task: &str, url: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET mr_url = ?2 WHERE key = ?1",
            params![task, url],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn bg_start(
        &self,
        agent: &str,
        command: &str,
        kind: &str,
        port: Option<i64>,
        log_path: Option<&str>,
        repo: Option<&str>,
    ) -> Result<i64> {
        let id: i64 = self
            .conn
            .query_row(
                "INSERT INTO bg_tasks (agent_id, repo, command, kind, port, log_path)
                 VALUES ((SELECT id FROM agents WHERE name = ?1), ?2, ?3, ?4, ?5, ?6)
                 RETURNING id",
                params![agent, repo, command, kind, port, log_path],
                |r| r.get(0),
            )
            .context("recording a background task")?;
        self.log_event(
            "bg",
            Some(agent),
            None,
            None,
            &format!("started: {command}"),
            None,
            Some(&id.to_string()),
        )?;
        Ok(id)
    }

    #[allow(dead_code)]
    pub fn bg_end(&self, id: i64, state: &str, detail: Option<&str>) -> Result<()> {
        let command: Option<String> = self
            .conn
            .query_row(
                "UPDATE bg_tasks SET state = ?2, detail = ?3,
                   ended_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
                 WHERE id = ?1 RETURNING command",
                params![id, state, detail],
                |r| r.get(0),
            )
            .optional()?;
        let Some(command) = command else {
            bail!("no background task with id {id}");
        };
        self.log_event(
            "bg",
            None,
            None,
            None,
            &format!("{state}: {command}"),
            detail,
            Some(&id.to_string()),
        )
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(dead_code)]
    pub fn log_event(
        &self,
        kind: &str,
        from: Option<&str>,
        to: Option<&str>,
        task: Option<&str>,
        summary: &str,
        body: Option<&str>,
        reference: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events (kind, from_agent, to_agent, task_key, summary, body, ref)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![kind, from, to, task, summary, body, reference],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.upsert_epic("ENG-2553", "shared settings flag").unwrap();
        db.upsert_agent("chief", Some("chief"), None, None, None, None)
            .unwrap();
        db.upsert_agent("accounts-svc", None, Some("/repo/setting"), None, None, None)
            .unwrap();
        db.upsert_agent("billing-svc", None, Some("/repo/content"), None, None, None)
            .unwrap();
        db.add_task(&NewTask {
            key: "ENG-2553-1",
            title: "shared column",
            repo: "/repo/setting",
            epic: Some("ENG-2553"),
            ..Default::default()
        })
        .unwrap();
        db.add_task(&NewTask {
            key: "ENG-2553-2",
            title: "consume the param",
            repo: "/repo/content",
            epic: Some("ENG-2553"),
            deps: &["ENG-2553-1"],
            ..Default::default()
        })
        .unwrap();
        db
    }

    #[test]
    fn ready_holds_back_anything_with_an_unfinished_dependency() {
        let db = seeded();
        let ready: Vec<_> = db.ready().unwrap().into_iter().map(|t| t.key).collect();
        assert_eq!(ready, vec!["ENG-2553-1"]);

        let board = db.board().unwrap();
        let second = board.iter().find(|t| t.key == "ENG-2553-2").unwrap();
        assert_eq!(second.waiting_on, vec!["ENG-2553-1"]);
        assert_eq!(second.epic_key.as_deref(), Some("ENG-2553"));
    }

    #[test]
    fn finishing_a_task_reports_what_it_freed() {
        let db = seeded();
        db.claim("ENG-2553-1", "accounts-svc").unwrap();
        db.transition("ENG-2553-1", State::Running, None).unwrap();

        let freed = db
            .transition("ENG-2553-1", State::Done, None)
            .unwrap();
        assert_eq!(freed, vec!["ENG-2553-2"]);
    }

    #[test]
    fn a_task_an_agent_parked_itself_is_still_freed() {
        let db = seeded();
        db.claim("ENG-2553-2", "billing-svc").unwrap();
        // The worker started before the dependency landed and parked itself.
        db.transition("ENG-2553-2", State::Blocked, Some("needs !412"))
            .unwrap();
        assert_eq!(
            db.board()
                .unwrap()
                .iter()
                .find(|t| t.key == "ENG-2553-2")
                .unwrap()
                .blocked_on
                .as_deref(),
            Some("needs !412")
        );

        let freed = db.transition("ENG-2553-1", State::Done, None).unwrap();
        assert_eq!(
            freed,
            vec!["ENG-2553-2"],
            "a blocked task is not skipped just because it is not queued"
        );
    }

    #[test]
    fn unblocking_clears_the_reason() {
        let db = seeded();
        db.transition("ENG-2553-2", State::Blocked, Some("needs !412"))
            .unwrap();
        db.transition("ENG-2553-2", State::Running, None).unwrap();

        let t = db
            .board()
            .unwrap()
            .into_iter()
            .find(|t| t.key == "ENG-2553-2")
            .unwrap();
        assert_eq!(t.state, State::Running);
        assert_eq!(t.blocked_on, None);
    }

    #[test]
    fn every_transition_leaves_a_flow_event() {
        let db = seeded();
        db.claim("ENG-2553-1", "accounts-svc").unwrap();
        db.transition("ENG-2553-1", State::Running, None).unwrap();
        db.transition("ENG-2553-1", State::Done, None).unwrap();

        let summaries: Vec<_> = db
            .events(20)
            .unwrap()
            .into_iter()
            .filter(|e| e.task_key.as_deref() == Some("ENG-2553-1"))
            .map(|e| e.summary)
            .collect();
        // Newest first.
        assert_eq!(
            summaries,
            vec![
                "done",
                "started",
                "assigned to accounts-svc",
                "queued: shared column"
            ]
        );
    }

    #[test]
    fn leaving_blocked_reads_as_unblocked_not_as_a_fresh_start() {
        let db = seeded();
        db.transition("ENG-2553-2", State::Blocked, Some("needs !412"))
            .unwrap();
        db.transition("ENG-2553-2", State::Running, None).unwrap();

        // cli/board.sh writes exactly these words for the same two moves.
        let summaries: Vec<_> = db.events(2).unwrap().into_iter().map(|e| e.summary).collect();
        assert_eq!(summaries, vec!["unblocked", "blocked: needs !412"]);
    }

    #[test]
    fn a_transition_carries_the_agent_that_owns_the_task() {
        let db = seeded();
        db.claim("ENG-2553-1", "accounts-svc").unwrap();
        db.transition("ENG-2553-1", State::Running, None).unwrap();

        let e = db.events(1).unwrap().remove(0);
        assert_eq!(e.from_agent.as_deref(), Some("accounts-svc"));
    }

    #[test]
    fn background_tasks_round_trip_and_count_against_their_agent() {
        let db = seeded();
        let id = db
            .bg_start(
                "accounts-svc",
                "./gradlew test",
                "test",
                None,
                Some("/tmp/t.log"),
                Some("/repo/setting"),
            )
            .unwrap();
        assert!(id > 0, "the id must come back, not a zero from another connection");

        let agent = db
            .agents()
            .unwrap()
            .into_iter()
            .find(|a| a.name == "accounts-svc")
            .unwrap();
        assert_eq!(agent.bg_running, 1);

        db.bg_end(id, "passed", Some("51 of 51")).unwrap();
        let bg = db.background().unwrap();
        let row = bg.iter().find(|b| b.id == id).unwrap();
        assert_eq!(row.state, "passed");
        assert_eq!(row.detail.as_deref(), Some("51 of 51"));
        assert!(
            row.elapsed_secs >= 0,
            "a finished task reports how long it took, not how long ago it started"
        );

        assert_eq!(
            db.agents()
                .unwrap()
                .into_iter()
                .find(|a| a.name == "accounts-svc")
                .unwrap()
                .bg_running,
            0
        );
    }

    #[test]
    fn updating_an_agent_does_not_wipe_what_it_already_had() {
        let db = seeded();
        db.upsert_agent("accounts-svc", None, None, Some("sess-1"), None, None)
            .unwrap();

        let a = db
            .agents()
            .unwrap()
            .into_iter()
            .find(|a| a.name == "accounts-svc")
            .unwrap();
        assert_eq!(a.session_id.as_deref(), Some("sess-1"));
        assert_eq!(a.repo.as_deref(), Some("/repo/setting"), "the repo survived");
    }

    #[test]
    fn nonsense_references_are_refused_rather_than_silently_dropped() {
        let db = seeded();
        assert!(db.claim("ENG-9999-1", "accounts-svc").is_err());
        assert!(db.claim("ENG-2553-1", "nobody").is_err());
        assert!(db.add_dep("ENG-2553-1", "ENG-9999-1").is_err());
        assert!(db.transition("ENG-9999-1", State::Done, None).is_err());
        assert!(db.bg_end(4242, "passed", None).is_err());
        assert!(db.retire_agent("nobody").is_err());
    }

    #[test]
    fn the_schema_applies_twice_without_complaint() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nested").join("fleet.db");
        let db = Db::open(&p).unwrap();
        db.upsert_epic("ENG-1", "first").unwrap();
        drop(db);

        let db = Db::open(&p).unwrap();
        assert!(db.board().unwrap().is_empty());
    }
}
