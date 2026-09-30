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
use serde::Serialize;

/// The schema travels in the binary, so `fleet board init` needs no checkout.
const SCHEMA: &str = include_str!("../schema.sql");


#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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

/// One agent as it was in a run: enough to bring it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunAgent {
    pub name: String,
    pub role: String,
    pub repo: String,
    pub session_id: Option<String>,
    pub retired: bool,
}

/// A stretch of work in one workspace, and the crew that did it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Run {
    pub id: i64,
    pub root: String,
    pub started_at: String,
    /// The last message or state change any of its agents made, or when it
    /// started if they made none.
    pub last_active: String,
    pub agents: Vec<RunAgent>,
    /// Tasks its agents touched, newest first, for telling runs apart.
    pub tasks: Vec<String>,
}

impl Run {
    /// The agents that would come back: not taken off on purpose, and with
    /// a conversation to resume.
    pub fn resumable(&self) -> impl Iterator<Item = &RunAgent> {
        self.agents
            .iter()
            .filter(|a| !a.retired && a.session_id.is_some())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    /// Where the board is, for the agents a fleet starts: they are handed it,
    /// and it is what makes them part of this fleet. None in memory.
    path: Option<std::path::PathBuf>,
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
        let mut db = Db::prepare(conn)?;
        db.path = Some(path.to_path_buf());
        Ok(db)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// A board that exists only for this process: tests, and the preview's
    /// invented fleet. The real one is a file several processes share.
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
        Ok(Db { conn, path: None })
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

    /// The flow log of one run, newest first: what has happened since it
    /// started. The board keeps every run a workspace has had, and the view
    /// is about the crew in front of you, not last week's.
    ///
    /// Compared as times rather than text: a run is stamped to the second
    /// and an event to the millisecond, and as text an event in the run's
    /// first second sorts before it.
    pub fn run_events(&self, run: i64, limit: usize) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.ts, e.kind, e.from_agent, e.to_agent, e.task_key, e.summary, e.body
             FROM events e JOIN runs r ON r.id = ?1
             WHERE julianday(e.ts) >= julianday(r.started_at)
             ORDER BY e.ts DESC, e.id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![run, limit as i64], |r| {
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

    /// One task with its brief and everything that has happened to it.
    pub fn show(&self, key: &str) -> Result<(Task, Option<String>, Vec<Event>)> {
        let task = self
            .board()?
            .into_iter()
            .find(|t| t.key == key)
            .with_context(|| format!("unknown task '{key}'"))?;
        let body: Option<String> = self.conn.query_row(
            "SELECT body FROM tasks WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )?;

        let mut stmt = self.conn.prepare(
            "SELECT ts, kind, from_agent, to_agent, task_key, summary, body
             FROM events WHERE task_key = ?1 ORDER BY ts, id",
        )?;
        let history = stmt
            .query_map(params![key], |r| {
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
        Ok((task, body, history))
    }

    /// The escape hatch the shell CLI had. Read-only on purpose: every write
    /// must go through a method that records the matching flow event, and a
    /// bare UPDATE here would be the one way to break that.
    pub fn query(&self, sql: &str) -> Result<(Vec<String>, Vec<Vec<String>>)> {
        let lowered = sql.trim_start().to_lowercase();
        if !(lowered.starts_with("select") || lowered.starts_with("with")) {
            bail!("only SELECT is allowed here — writes must go through the board commands");
        }
        let mut stmt = self.conn.prepare(sql)?;
        let columns: Vec<String> = stmt.column_names().into_iter().map(str::to_string).collect();
        let width = columns.len();
        let rows = stmt
            .query_map([], |r| {
                (0..width)
                    .map(|i| {
                        Ok(match r.get_ref(i)? {
                            rusqlite::types::ValueRef::Null => String::new(),
                            rusqlite::types::ValueRef::Integer(v) => v.to_string(),
                            rusqlite::types::ValueRef::Real(v) => v.to_string(),
                            rusqlite::types::ValueRef::Text(v) => {
                                String::from_utf8_lossy(v).into_owned()
                            }
                            rusqlite::types::ValueRef::Blob(_) => "<blob>".into(),
                        })
                    })
                    .collect::<rusqlite::Result<Vec<String>>>()
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((columns, rows))
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
        if task == depends_on {
            bail!("'{task}' cannot wait on itself");
        }
        // Before the insert, because afterwards the damage is done and the
        // only symptom is two tasks that never turn up in `ready` — no
        // error, nothing on the board saying why, just work that never
        // starts.
        if let Some(path) = self.path_back(task, depends_on)? {
            bail!(
                "'{task}' cannot wait on '{depends_on}': that closes a loop, \
                 because {path} already runs the other way, and nothing in a \
                 loop ever becomes ready"
            );
        }

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

    /// The chain by which `depends_on` already waits on `task`, if there is
    /// one. Its existence is what would make a new edge between them a loop.
    ///
    /// Reported as the whole chain rather than a yes: a loop closed at the
    /// twelfth task is not one anybody can see by reading the board, and
    /// "that would be circular" leaves them to find it themselves.
    fn path_back(&self, task: &str, depends_on: &str) -> Result<Option<String>> {
        let path: Option<String> = self
            .conn
            .query_row(
                // Walking from `depends_on` through what it waits on. The
                // depth cap is not about this graph, which has no loop yet
                // by construction — it is so that a database which somehow
                // already holds one cannot hang the command that refuses to
                // add another.
                "WITH RECURSIVE waits(id, path, depth) AS (
                     SELECT id, key, 0 FROM tasks WHERE key = ?2
                   UNION ALL
                     SELECT d.depends_on, w.path || ' -> ' || t.key, w.depth + 1
                       FROM task_deps d
                       JOIN waits w ON d.task_id = w.id
                       JOIN tasks t ON t.id = d.depends_on
                      WHERE w.depth < 64
                 )
                 SELECT path FROM waits
                   JOIN tasks t ON t.id = waits.id
                  WHERE t.key = ?1 AND waits.depth > 0
                  LIMIT 1",
                params![task, depends_on],
                |r| r.get(0),
            )
            .optional()?;
        Ok(path)
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
            (State::Done, Some(mr)) => format!("done — {mr}"),
            (State::Done, None) => "done".to_string(),
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
    /// Returns the command that finished, which is what a caller reports —
    /// "passed 7" tells nobody anything.
    pub fn bg_end(&self, id: i64, state: &str, detail: Option<&str>) -> Result<String> {
        // Read before the update, because the event has to name the agent
        // whose process this was. Without it the flow log shows "started:
        // gradlew test" against accounts-svc and "passed: gradlew test"
        // against nobody, which reads as two unrelated things.
        let owner: Option<String> = self
            .conn
            .query_row(
                "SELECT a.name FROM bg_tasks b JOIN agents a ON a.id = b.agent_id
                 WHERE b.id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
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
            owner.as_deref(),
            None,
            None,
            &format!("{state}: {command}"),
            detail,
            Some(&id.to_string()),
        )?;
        Ok(command)
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

impl Db {
    /// Begin a run in `root`, and return it.
    pub fn start_run(&self, root: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "INSERT INTO runs (root) VALUES (?1) RETURNING id",
            params![root],
            |r| r.get(0),
        )?)
    }

    /// The most recent run in `root`.
    pub fn latest_run(&self, root: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM runs WHERE root = ?1 ORDER BY id DESC LIMIT 1",
                params![root],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The run an agent started in `repo` belongs to, when nothing said so:
    /// the latest whose workspace holds the repo, or failing that the
    /// latest of all.
    pub fn run_for_repo(&self, repo: &str) -> Result<Option<i64>> {
        let within: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM runs
                 WHERE ?1 = root OR ?1 LIKE root || '/%'
                 ORDER BY id DESC LIMIT 1",
                params![repo],
                |r| r.get(0),
            )
            .optional()?;
        if within.is_some() {
            return Ok(within);
        }
        Ok(self
            .conn
            .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| r.get(0))
            .optional()?)
    }

    /// Put an agent in a run. Joining again brings a retired one back in,
    /// and keeps whatever session it already had.
    pub fn join_run(&self, run: i64, name: &str, role: &str, repo: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO run_agents (run_id, name, role, repo) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(run_id, name) DO UPDATE SET
               role = excluded.role, repo = excluded.repo, retired = 0",
            params![run, name, role, repo],
        )?;
        Ok(())
    }

    /// Record which conversation an agent in a run turned out to be.
    /// Record a session in whichever run the agent joined without one — the
    /// newest, when it joined several. For a session found later than the
    /// launch that recorded the run, by someone who does not know the run.
    pub fn set_session_in_latest_run(&self, name: &str, session_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE run_agents SET session_id = ?2
              WHERE name = ?1 AND session_id IS NULL
                AND run_id = (SELECT MAX(run_id) FROM run_agents WHERE name = ?1)",
            params![name, session_id],
        )?;
        Ok(())
    }

    pub fn set_run_session(&self, run: i64, name: &str, session_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE run_agents SET session_id = ?3 WHERE run_id = ?1 AND name = ?2",
            params![run, name, session_id],
        )?;
        Ok(())
    }

    /// Taken off on purpose: not brought back when the run is resumed.
    pub fn retire_in_run(&self, run: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE run_agents SET retired = 1 WHERE run_id = ?1 AND name = ?2",
            params![run, name],
        )?;
        Ok(())
    }

    /// Off the rail because its process is gone — not because anybody
    /// retired it. The run keeps it, so it can be resumed.
    pub fn end_agent(&self, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE agents SET ended_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
             WHERE name = ?1 AND ended_at IS NULL",
            params![name],
        )?;
        Ok(())
    }

    /// Runs in `root` that have anyone in them, newest first.
    pub fn runs(&self, root: &str) -> Result<Vec<Run>> {
        let ids: Vec<i64> = {
            let mut stmt = self.conn.prepare(
                "SELECT r.id FROM runs r
                 WHERE r.root = ?1 AND EXISTS (SELECT 1 FROM run_agents a WHERE a.run_id = r.id)
                 ORDER BY r.id DESC",
            )?;
            stmt.query_map(params![root], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        ids.into_iter()
            .filter_map(|id| self.run(id).transpose())
            .collect()
    }

    pub fn run(&self, id: i64) -> Result<Option<Run>> {
        let Some((root, started_at)): Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT root, started_at FROM runs WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };

        let agents: Vec<RunAgent> = {
            let mut stmt = self.conn.prepare(
                "SELECT name, role, repo, session_id, retired FROM run_agents
                 WHERE run_id = ?1 ORDER BY role = 'chief' DESC, joined_at, name",
            )?;
            stmt.query_map(params![id], |r| {
                Ok(RunAgent {
                    name: r.get(0)?,
                    role: r.get(1)?,
                    repo: r.get(2)?,
                    session_id: r.get(3)?,
                    retired: r.get::<_, i64>(4)? != 0,
                })
            })?
            .collect::<rusqlite::Result<_>>()?
        };

        // What its agents did, from when it started until the next run in
        // the same workspace began: a name reused in a later run is that
        // run's, not this one's.
        let window = "e.ts >= r.started_at
             AND e.ts < COALESCE(
                 (SELECT MIN(n.started_at) FROM runs n WHERE n.root = r.root AND n.id > r.id),
                 '9999')
             AND (e.from_agent IN (SELECT name FROM run_agents WHERE run_id = r.id)
               OR e.to_agent IN (SELECT name FROM run_agents WHERE run_id = r.id))";
        let last_active: String = self.conn.query_row(
            &format!(
                "SELECT COALESCE(MAX(e.ts), r.started_at) FROM runs r
                 LEFT JOIN events e ON {window}
                 WHERE r.id = ?1 GROUP BY r.id"
            ),
            params![id],
            |r| r.get(0),
        )?;
        let tasks: Vec<String> = {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT e.task_key FROM runs r JOIN events e ON {window}
                 WHERE r.id = ?1 AND e.task_key IS NOT NULL
                 GROUP BY e.task_key ORDER BY MAX(e.ts) DESC LIMIT 4"
            ))?;
            stmt.query_map(params![id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };

        Ok(Some(Run {
            id,
            root,
            started_at,
            last_active,
            agents,
            tasks,
        }))
    }

    /// Give a board from before runs existed one run to resume from.
    ///
    /// The agents table has always kept each name's latest session, which
    /// is the crew from the last fleet anyone ran — worth resuming, and
    /// otherwise lost the moment a new chief overwrote its row. Done once:
    /// with any run on the board there is nothing to adopt.
    pub fn adopt_legacy_run(&self, root: &str) -> Result<Option<i64>> {
        let any: i64 = self.conn.query_row("SELECT count(*) FROM runs", [], |r| r.get(0))?;
        if any > 0 {
            return Ok(None);
        }
        let agents: Vec<(String, String, Option<String>, String)> = {
            let mut stmt = self.conn.prepare(
                "SELECT name, role, repo, session_id FROM agents
                 WHERE session_id IS NOT NULL",
            )?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        if agents.is_empty() {
            return Ok(None);
        }
        let run = self.start_run(root)?;
        // Dated from the earliest of them, not from now: the run is the work
        // they already did, and a window that opened at adoption would hold
        // none of it — no tasks, and "just now" for a crew from last week.
        self.conn.execute(
            "UPDATE runs SET started_at = COALESCE(
                 (SELECT MIN(spawned_at) FROM agents WHERE session_id IS NOT NULL),
                 started_at)
             WHERE id = ?1",
            params![run],
        )?;
        for (name, role, repo, session) in agents {
            let repo = repo.unwrap_or_else(|| root.to_string());
            self.join_run(run, &name, &role, &repo)?;
            self.set_run_session(run, &name, &session)?;
        }
        Ok(Some(run))
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
    fn finishing_with_a_merge_request_records_it_in_the_log_too() {
        let db = seeded();
        db.transition("ENG-2553-1", State::Done, Some("https://git/x/412"))
            .unwrap();
        assert_eq!(db.events(1).unwrap()[0].summary, "done — https://git/x/412");
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

        let finished = db.bg_end(id, "passed", Some("51 of 51")).unwrap();
        assert_eq!(finished, "./gradlew test");
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

        // Both ends of the process name the agent it belonged to. Without it
        // the flow log shows the start against accounts-svc and the finish
        // against nobody, reading as two unrelated things.
        let events = db.events(10).unwrap();
        let ended = events
            .iter()
            .find(|e| e.summary.starts_with("passed:"))
            .expect("finishing writes an event");
        assert_eq!(ended.from_agent.as_deref(), Some("accounts-svc"));
        let started = events
            .iter()
            .find(|e| e.summary.starts_with("started:"))
            .unwrap();
        assert_eq!(started.from_agent, ended.from_agent);
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

    fn chain(db: &Db, keys: &[&str]) {
        for key in keys {
            db.add_task(&NewTask {
                key,
                title: "a task",
                repo: "/repo",
                ..Default::default()
            })
            .unwrap();
        }
    }

    #[test]
    fn a_task_cannot_wait_on_itself() {
        let db = Db::open_in_memory().unwrap();
        chain(&db, &["A"]);
        let err = db.add_dep("A", "A").unwrap_err().to_string();
        assert!(err.contains("cannot wait on itself"), "{err}");
    }

    #[test]
    fn two_tasks_cannot_wait_on_each_other() {
        let db = Db::open_in_memory().unwrap();
        chain(&db, &["A", "B"]);
        db.add_dep("B", "A").unwrap();

        let err = db.add_dep("A", "B").unwrap_err().to_string();
        assert!(err.contains("closes a loop"), "{err}");
        // Nothing was written: a refusal that half-applies is worse than the
        // loop it was refusing.
        assert!(!db.dep_exists("A", "B").unwrap());
    }

    #[test]
    fn a_loop_closed_the_long_way_round_is_refused_too() {
        // The case nobody spots by reading the board: four tasks, and the
        // edge that closes it looks like any other.
        let db = Db::open_in_memory().unwrap();
        chain(&db, &["A", "B", "C", "D"]);
        db.add_dep("B", "A").unwrap();
        db.add_dep("C", "B").unwrap();
        db.add_dep("D", "C").unwrap();

        let err = db.add_dep("A", "D").unwrap_err().to_string();
        assert!(err.contains("closes a loop"), "{err}");
        // And says which way round, so it can be found without re-deriving
        // it from twelve rows.
        assert!(err.contains("D -> C -> B -> A"), "{err}");
    }

    #[test]
    fn a_task_two_others_wait_on_is_not_a_loop() {
        // The shape that matters: one task unblocks several, and they join
        // again later. Refusing this would make the check useless.
        let db = Db::open_in_memory().unwrap();
        chain(&db, &["shared", "left", "right", "join"]);
        db.add_dep("left", "shared").unwrap();
        db.add_dep("right", "shared").unwrap();
        db.add_dep("join", "left").unwrap();
        db.add_dep("join", "right").unwrap();

        assert_eq!(db.ready().unwrap().len(), 1, "only 'shared' is ready");
        db.transition("shared", State::Done, None).unwrap();
        let ready: Vec<_> = db.ready().unwrap().into_iter().map(|t| t.key).collect();
        assert_eq!(ready, vec!["left", "right"], "both, and not the join");
    }

    #[test]
    fn saying_the_same_dependency_twice_is_still_fine() {
        // It is already there, so it is not a loop — the check must not read
        // the edge being added as one that already runs the other way.
        let db = Db::open_in_memory().unwrap();
        chain(&db, &["A", "B"]);
        db.add_dep("B", "A").unwrap();
        db.add_dep("B", "A").unwrap();
    }

    #[test]
    fn a_runs_log_starts_where_the_run_did() {
        let db = Db::open_in_memory().unwrap();
        let old = db.start_run("/w").unwrap();
        db.conn
            .execute("UPDATE runs SET started_at = '2026-09-29T08:50:09Z' WHERE id = ?1", [old])
            .unwrap();
        db.log_event("note", Some("chief"), None, None, "yesterday", None, None).unwrap();
        db.conn
            .execute("UPDATE events SET ts = '2026-09-29T08:50:09.955Z'", [])
            .unwrap();
        let new = db.start_run("/w").unwrap();
        db.log_event("note", Some("chief"), None, None, "today", None, None).unwrap();

        let today: Vec<_> = db.run_events(new, 50).unwrap().into_iter().map(|e| e.summary).collect();
        assert_eq!(today, ["today"]);
        // An event in the run's first second is the run's, though as text it
        // sorts before the run's own stamp.
        let all: Vec<_> = db.run_events(old, 50).unwrap().into_iter().map(|e| e.summary).collect();
        assert_eq!(all, ["today", "yesterday"]);
    }

    #[test]
    fn a_run_keeps_the_session_a_later_chief_overwrites() {
        // The agents table keeps one row per name: today's chief repoints
        // it, and last week's conversation would be lost with it.
        let db = Db::open_in_memory().unwrap();
        let old = db.start_run("/w").unwrap();
        db.join_run(old, "chief", "chief", "/w").unwrap();
        db.set_run_session(old, "chief", "sess-last-week").unwrap();
        db.upsert_agent("chief", Some("chief"), Some("/w"), Some("sess-last-week"), None, None)
            .unwrap();

        let new = db.start_run("/w").unwrap();
        db.join_run(new, "chief", "chief", "/w").unwrap();
        db.upsert_agent("chief", None, None, Some("sess-today"), None, None).unwrap();
        db.set_run_session(new, "chief", "sess-today").unwrap();

        let then = db.run(old).unwrap().unwrap();
        assert_eq!(then.agents[0].session_id.as_deref(), Some("sess-last-week"));
    }

    #[test]
    fn an_agent_that_died_comes_back_and_one_retired_on_purpose_does_not() {
        let db = Db::open_in_memory().unwrap();
        let run = db.start_run("/w").unwrap();
        for (name, sid) in [("chief", "s1"), ("billing-svc", "s2"), ("storefront", "s3")] {
            db.join_run(run, name, if name == "chief" { "chief" } else { "worker" }, "/w").unwrap();
            db.set_run_session(run, name, sid).unwrap();
        }
        db.retire_in_run(run, "storefront").unwrap();

        let r = db.run(run).unwrap().unwrap();
        let back: Vec<&str> = r.resumable().map(|a| a.name.as_str()).collect();
        assert_eq!(back, vec!["chief", "billing-svc"]);
    }

    #[test]
    fn an_agent_that_never_reported_a_session_cannot_be_resumed() {
        let db = Db::open_in_memory().unwrap();
        let run = db.start_run("/w").unwrap();
        db.join_run(run, "never-adopted", "worker", "/w").unwrap();
        assert_eq!(db.run(run).unwrap().unwrap().resumable().count(), 0);
    }

    #[test]
    fn the_chief_is_listed_first() {
        let db = Db::open_in_memory().unwrap();
        let run = db.start_run("/w").unwrap();
        db.join_run(run, "alpha", "worker", "/w").unwrap();
        db.join_run(run, "chief", "chief", "/w").unwrap();
        let names: Vec<String> = db.run(run).unwrap().unwrap().agents.into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["chief", "alpha"]);
    }

    #[test]
    fn runs_are_listed_per_workspace_newest_first_and_empty_ones_are_not() {
        let db = Db::open_in_memory().unwrap();
        let a = db.start_run("/w").unwrap();
        db.join_run(a, "chief", "chief", "/w").unwrap();
        let _elsewhere = db.start_run("/other").unwrap();
        let _empty = db.start_run("/w").unwrap();
        let b = db.start_run("/w").unwrap();
        db.join_run(b, "chief", "chief", "/w").unwrap();

        let ids: Vec<i64> = db.runs("/w").unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![b, a]);
    }

    #[test]
    fn what_a_run_did_is_what_its_agents_did_before_the_next_run_began() {
        // A name reused in a later run is that run's: its tasks must not be
        // credited to the one before.
        let db = Db::open_in_memory().unwrap();
        let first = db.start_run("/w").unwrap();
        db.join_run(first, "chief", "chief", "/w").unwrap();
        db.conn
            .execute("UPDATE runs SET started_at = '2026-09-01T09:00:00Z' WHERE id = ?1", params![first])
            .unwrap();
        db.log_event("message", Some("chief"), Some("x"), Some("ENG-1-1"), "go", None, None).unwrap();
        db.conn
            .execute("UPDATE events SET ts = '2026-09-01T10:00:00.000Z' WHERE task_key = 'ENG-1-1'", [])
            .unwrap();

        let second = db.start_run("/w").unwrap();
        db.join_run(second, "chief", "chief", "/w").unwrap();
        db.conn
            .execute("UPDATE runs SET started_at = '2026-09-02T09:00:00Z' WHERE id = ?1", params![second])
            .unwrap();
        db.log_event("message", Some("chief"), Some("y"), Some("ENG-2-1"), "go", None, None).unwrap();

        let r1 = db.run(first).unwrap().unwrap();
        assert_eq!(r1.tasks, vec!["ENG-1-1"]);
        assert_eq!(r1.last_active, "2026-09-01T10:00:00.000Z");
        let r2 = db.run(second).unwrap().unwrap();
        assert_eq!(r2.tasks, vec!["ENG-2-1"]);
    }

    #[test]
    fn a_session_found_late_lands_in_the_run_the_agent_joined() {
        let db = Db::open_in_memory().unwrap();
        let old = db.start_run("/w").unwrap();
        db.join_run(old, "chief", "chief", "/w").unwrap();
        db.set_run_session(old, "chief", "s-old").unwrap();
        let new = db.start_run("/w").unwrap();
        db.join_run(new, "chief", "chief", "/w").unwrap();

        db.set_session_in_latest_run("chief", "s-new").unwrap();
        let session = |run| db.run(run).unwrap().unwrap().agents[0].session_id.clone();
        assert_eq!(session(new).as_deref(), Some("s-new"));
        assert_eq!(session(old).as_deref(), Some("s-old"), "an earlier run keeps its own");
    }

    #[test]
    fn a_board_from_before_runs_existed_is_given_one_to_resume_from_once() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_agent("chief", Some("chief"), Some("/w"), Some("s-chief"), None, None).unwrap();
        db.upsert_agent("eng-2155", None, Some("/w/storefront"), Some("s-jet"), None, None).unwrap();
        db.upsert_agent("never-ran", None, Some("/w/x"), None, None, None).unwrap();

        let run = db.adopt_legacy_run("/w").unwrap().expect("a run to resume");
        let r = db.run(run).unwrap().unwrap();
        let back: Vec<(&str, Option<&str>)> =
            r.resumable().map(|a| (a.name.as_str(), a.session_id.as_deref())).collect();
        assert_eq!(back, vec![("chief", Some("s-chief")), ("eng-2155", Some("s-jet"))]);
        assert_eq!(db.adopt_legacy_run("/w").unwrap(), None, "and only once");
    }

    #[test]
    fn an_adopted_run_is_dated_from_its_agents_so_their_work_is_in_it() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_agent("chief", Some("chief"), Some("/w"), Some("s"), None, None).unwrap();
        db.conn
            .execute("UPDATE agents SET spawned_at = '2026-09-20T09:00:00Z'", [])
            .unwrap();
        db.log_event("message", Some("chief"), Some("x"), Some("ENG-2155-1"), "go", None, None).unwrap();
        db.conn
            .execute("UPDATE events SET ts = '2026-09-20T10:00:00.000Z'", [])
            .unwrap();

        let run = db.adopt_legacy_run("/w").unwrap().unwrap();
        let r = db.run(run).unwrap().unwrap();
        assert_eq!(r.started_at, "2026-09-20T09:00:00Z");
        assert_eq!(r.tasks, vec!["ENG-2155-1"], "its work, not an empty window from now");
    }

    #[test]
    fn a_spawn_with_no_run_named_joins_the_workspace_it_is_in() {
        let db = Db::open_in_memory().unwrap();
        let w = db.start_run("/w").unwrap();
        let other = db.start_run("/elsewhere").unwrap();
        assert_eq!(db.run_for_repo("/w/service/billing-service").unwrap(), Some(w));
        assert_eq!(db.run_for_repo("/w").unwrap(), Some(w));
        // "/w2" is not inside "/w", whatever the strings share.
        assert_eq!(db.run_for_repo("/w2/thing").unwrap(), Some(other), "falls back to the latest");
    }

    #[test]
    fn an_agent_that_died_leaves_the_rail_without_being_retired() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_agent("billing-svc", None, Some("/w"), Some("s"), None, None).unwrap();
        db.end_agent("billing-svc").unwrap();
        assert!(db.agents().unwrap().iter().all(|a| a.name != "billing-svc"), "off the rail");
        let retired = db
            .events(10)
            .unwrap()
            .iter()
            .any(|e| e.summary.contains("retired"));
        assert!(!retired, "and not recorded as something anybody chose");
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
    fn show_gathers_a_task_its_brief_and_its_history() {
        let db = seeded();
        db.claim("ENG-2553-1", "accounts-svc").unwrap();
        db.transition("ENG-2553-1", State::Running, None).unwrap();

        let (task, _body, history) = db.show("ENG-2553-1").unwrap();
        assert_eq!(task.agent.as_deref(), Some("accounts-svc"));
        let summaries: Vec<_> = history.into_iter().map(|e| e.summary).collect();
        assert_eq!(
            summaries,
            vec!["queued: shared column", "assigned to accounts-svc", "started"],
            "oldest first, which is how a history reads"
        );

        assert!(db.show("ENG-9999-1").is_err());
    }

    #[test]
    fn the_escape_hatch_reads_but_does_not_write() {
        let db = seeded();
        let (columns, rows) = db.query("SELECT key, state FROM tasks ORDER BY key").unwrap();
        assert_eq!(columns, vec!["key", "state"]);
        assert_eq!(rows[0], vec!["ENG-2553-1", "queued"]);

        // A bare UPDATE would skip the flow event that every state change
        // writes, which is the one thing holding the board and the log together.
        assert!(db.query("UPDATE tasks SET state = 'done'").is_err());
        assert!(db.query("DELETE FROM tasks").is_err());
        assert_eq!(
            db.board().unwrap().iter().filter(|t| t.state == State::Done).count(),
            0
        );
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
