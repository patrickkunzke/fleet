-- fleet.db — the live coordination layer for a multi-repo agent fleet.
--
-- Scope: intent and state that agents must agree on RIGHT NOW — who is working,
-- on what, what blocks what, what is running in the background, and who told
-- whom. It is deliberately NOT a memory store.
--
-- Long-term recall lives in claude-mem (~/.claude-mem/claude-mem.db, read-only
-- from here). Liveness lives in the Claude Code session registry
-- (~/.claude/sessions/<pid>.json). Raw tool activity lives in the transcripts
-- (~/.claude/projects/<slug>/<session_id>.jsonl). This database joins to all
-- three by session_id and never duplicates them.

PRAGMA journal_mode = WAL;          -- several agents write concurrently
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;

CREATE TABLE IF NOT EXISTS schema_version (
  version    INTEGER PRIMARY KEY,
  applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
INSERT OR IGNORE INTO schema_version (version) VALUES (1);
-- 2: agents.tmux_target renamed to target. Done in db.rs, before this runs.
INSERT OR IGNORE INTO schema_version (version) VALUES (2);


-- ---------------------------------------------------------------- agents ---
-- One row per agent the fleet SPAWNED. Not one row per Claude Code session:
-- the registry already has those. An agent keeps its identity across restarts;
-- session_id is repointed when it is respawned.

CREATE TABLE IF NOT EXISTS agents (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  name        TEXT NOT NULL UNIQUE,         -- matches the registry's "name" when live
  role        TEXT NOT NULL DEFAULT 'worker'
                CHECK (role IN ('chief', 'worker')),
  repo        TEXT,                         -- absolute path; NULL for the chief
  session_id  TEXT,                         -- ~/.claude/sessions/*.json .sessionId
  target      TEXT,                         -- e.g. "herdr:acme-billing-service"
  branch      TEXT,
  spawned_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  ended_at    TEXT,
  note        TEXT
);

CREATE INDEX IF NOT EXISTS idx_agents_session ON agents(session_id);
CREATE INDEX IF NOT EXISTS idx_agents_live    ON agents(ended_at) WHERE ended_at IS NULL;


-- ----------------------------------------------------------------- epics ---

CREATE TABLE IF NOT EXISTS epics (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  key        TEXT NOT NULL UNIQUE,          -- "ENG-2553"
  title      TEXT NOT NULL,
  state      TEXT NOT NULL DEFAULT 'active'
               CHECK (state IN ('active', 'paused', 'done')),
  position   INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  note       TEXT
);


-- ----------------------------------------------------------------- tasks ---
-- What the chief of staff writes down and the workers pick up.
--
-- state is the kanban column. 'blocked' is a state rather than a flag because
-- a blocked task is not running: whoever owns it has stopped and said why.

CREATE TABLE IF NOT EXISTS tasks (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  key        TEXT NOT NULL UNIQUE,          -- "ENG-2553-2"
  epic_id    INTEGER REFERENCES epics(id) ON DELETE SET NULL,
  title      TEXT NOT NULL,
  body       TEXT,                          -- the brief the worker is handed
  repo       TEXT NOT NULL,                 -- absolute path
  agent_id   INTEGER REFERENCES agents(id) ON DELETE SET NULL,
  state      TEXT NOT NULL DEFAULT 'queued'
               CHECK (state IN ('queued', 'running', 'blocked', 'review', 'done', 'dropped')),
  blocked_on TEXT,                          -- prose reason, set with state='blocked'
  branch     TEXT,
  mr_url     TEXT,
  position   INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  started_at TEXT,
  done_at    TEXT
);

CREATE INDEX IF NOT EXISTS idx_tasks_state ON tasks(state);
CREATE INDEX IF NOT EXISTS idx_tasks_epic  ON tasks(epic_id, position);
CREATE INDEX IF NOT EXISTS idx_tasks_agent ON tasks(agent_id);
CREATE INDEX IF NOT EXISTS idx_tasks_repo  ON tasks(repo);

CREATE TRIGGER IF NOT EXISTS tasks_touch AFTER UPDATE ON tasks
WHEN NEW.updated_at = OLD.updated_at
BEGIN
  UPDATE tasks SET updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = NEW.id;
END;


-- ------------------------------------------------------------ task_deps ---
-- A DAG. "task_id cannot start until depends_on is done."
--
-- The CHECK keeps a task off its own back; the acyclic half is enforced in
-- add_dep, which walks the existing edges before writing a new one. It has to
-- be, because a loop is not a row SQLite can look at and refuse — and the
-- only symptom would be tasks that never appear in v_ready, with nothing
-- anywhere saying why.

CREATE TABLE IF NOT EXISTS task_deps (
  task_id    INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  depends_on INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  PRIMARY KEY (task_id, depends_on),
  CHECK (task_id <> depends_on)
);

CREATE INDEX IF NOT EXISTS idx_deps_reverse ON task_deps(depends_on);


-- ------------------------------------------------------------------ runs ---
-- One stretch of work in one workspace: a chief and the agents it started,
-- running together. What `fleet resume` brings back.
--
-- Separate from agents because that table keeps one row per name and repoints
-- its session_id on every respawn: the chief started today overwrites the one
-- from last week. This keeps which sessions belonged together, so a whole crew
-- can be resumed as it was.

CREATE TABLE IF NOT EXISTS runs (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  root       TEXT NOT NULL,               -- the workspace fleet was started in
  started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_runs_root ON runs(root, id);

CREATE TABLE IF NOT EXISTS run_agents (
  run_id     INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  name       TEXT NOT NULL,
  role       TEXT NOT NULL DEFAULT 'worker' CHECK (role IN ('chief', 'worker')),
  repo       TEXT NOT NULL,
  session_id TEXT,                        -- what `claude --resume` is given
  -- Taken off the rail on purpose, and so not brought back. An agent whose
  -- process merely died is still part of the run.
  retired    INTEGER NOT NULL DEFAULT 0,
  joined_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  PRIMARY KEY (run_id, name)
);

INSERT OR IGNORE INTO schema_version (version) VALUES (2);


-- ---------------------------------------------------------------- events ---
-- The flow log: SEMANTIC events only — who told whom what, and task
-- transitions. Tool calls and file edits are NOT written here; they are in the
-- transcripts, which the TUI tails separately. Keeping the volume low is what
-- makes this table queryable as a history.

CREATE TABLE IF NOT EXISTS events (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  ts         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  kind       TEXT NOT NULL
               CHECK (kind IN ('message', 'task', 'bg', 'note')),
  from_agent TEXT,                          -- agents.name, free text if unknown
  to_agent   TEXT,
  task_key   TEXT,
  summary    TEXT NOT NULL,                 -- one line; what the flow pane shows
  body       TEXT,                          -- full text; what the detail pane shows
  session_id TEXT,                          -- jump target for the TUI
  ref        TEXT                           -- MR url, bg_task id, whatever fits
);

CREATE INDEX IF NOT EXISTS idx_events_ts   ON events(ts DESC);
CREATE INDEX IF NOT EXISTS idx_events_task ON events(task_key);
CREATE INDEX IF NOT EXISTS idx_events_pair ON events(from_agent, to_agent);


-- -------------------------------------------------------------- bg_tasks ---
-- Processes that outlive a turn: dev servers, compose stacks, test and build
-- runs. The right rail reads this table.

CREATE TABLE IF NOT EXISTS bg_tasks (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  agent_id   INTEGER REFERENCES agents(id) ON DELETE SET NULL,
  repo       TEXT,
  command    TEXT NOT NULL,
  kind       TEXT NOT NULL DEFAULT 'script'
               CHECK (kind IN ('server', 'compose', 'test', 'build', 'watch', 'script')),
  pid        INTEGER,
  port       INTEGER,
  log_path   TEXT,
  state      TEXT NOT NULL DEFAULT 'running'
               CHECK (state IN ('running', 'passed', 'failed', 'killed')),
  detail     TEXT,                          -- "34 of 51 passed", "2 type errors"
  started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  ended_at   TEXT
);

CREATE INDEX IF NOT EXISTS idx_bg_state ON bg_tasks(state);
CREATE INDEX IF NOT EXISTS idx_bg_agent ON bg_tasks(agent_id);


-- --------------------------------------------------------------- inbox ---
-- Messages waiting for the recipient's own session to pick them up.
--
-- An agent fleet starts carries a Claude Code mod that checks in here every
-- few seconds (mailboxes) and, once its session is idle, takes what is
-- waiting (inbox) and submits it as a prompt of its own. A message to an
-- agent whose mailbox has gone quiet is typed into its pane instead, as
-- before, and never gets a row here. A row is deleted when it is taken: the
-- message itself stays in events.

CREATE TABLE IF NOT EXISTS mailboxes (
  agent      TEXT PRIMARY KEY,              -- agents.name
  session_id TEXT NOT NULL,
  polled_at  INTEGER NOT NULL               -- unix seconds
);

CREATE TABLE IF NOT EXISTS inbox (
  event_id   INTEGER PRIMARY KEY REFERENCES events(id) ON DELETE CASCADE,
  agent      TEXT NOT NULL                  -- events.to_agent
);

CREATE INDEX IF NOT EXISTS idx_inbox_agent ON inbox(agent, event_id);

-- What the mod saw at its last check-in: the tool the session was running,
-- and how full its context window was. The graph shows both while the
-- mailbox is fresh; a stale row means nothing.
CREATE TABLE IF NOT EXISTS vitals (
  agent      TEXT PRIMARY KEY,              -- agents.name
  tool       TEXT,                          -- NULL between tool calls
  context    INTEGER                        -- percent, NULL before the first answer
);


-- ------------------------------------------------------------ approvals ---
-- A worker's go-ahead on a task. fleet's mod in the worker's session holds
-- back Edit and Write until the task it works on has one. Given by the chief
-- with `fleet board go`, or by the person at the keyboard typing a prompt in
-- the worker's own pane. Its own table rather than a column on tasks, so an
-- older board needs no migration.

CREATE TABLE IF NOT EXISTS approvals (
  task_key    TEXT PRIMARY KEY,             -- tasks.key
  by          TEXT NOT NULL,                -- 'user', or the agent that gave it
  approved_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);


-- ------------------------------------------------------------------- kv ---
-- Fleet-wide scratch state the TUI and the chief both read.
-- Known keys: focus_epic, last_dispatch_at.

CREATE TABLE IF NOT EXISTS kv (
  k          TEXT PRIMARY KEY,
  v          TEXT,
  updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);


-- ----------------------------------------------------------------- views ---

-- Queued tasks whose dependencies are all done. This is the dispatch queue:
-- the chief reads it, picks the top, and spawns or messages an agent.
CREATE VIEW IF NOT EXISTS v_ready AS
SELECT t.*, e.key AS epic_key
FROM tasks t
LEFT JOIN epics e ON e.id = t.epic_id
WHERE t.state = 'queued'
  AND NOT EXISTS (
    SELECT 1 FROM task_deps d
    JOIN tasks p ON p.id = d.depends_on
    WHERE d.task_id = t.id AND p.state <> 'done'
  )
ORDER BY e.position, t.position, t.id;

-- One row per task with its blockers spelled out, for the board pane.
CREATE VIEW IF NOT EXISTS v_board AS
SELECT
  t.key, t.title, t.state, t.repo, t.mr_url, t.blocked_on,
  e.key  AS epic_key,
  e.title AS epic_title,
  a.name AS agent,
  (SELECT group_concat(p.key, ', ')
     FROM task_deps d JOIN tasks p ON p.id = d.depends_on
    WHERE d.task_id = t.id AND p.state <> 'done') AS waiting_on
FROM tasks t
LEFT JOIN epics  e ON e.id = t.epic_id
LEFT JOIN agents a ON a.id = t.agent_id
ORDER BY e.position, t.position, t.id;

-- Live agents with their current task, for the graph.
CREATE VIEW IF NOT EXISTS v_agents AS
SELECT
  a.name, a.role, a.repo, a.session_id, a.target, a.branch,
  t.key   AS task_key,
  t.title AS task_title,
  t.state AS task_state,
  (SELECT count(*) FROM bg_tasks b
    WHERE b.agent_id = a.id AND b.state = 'running') AS bg_running
FROM agents a
LEFT JOIN tasks t ON t.agent_id = a.id AND t.state IN ('running', 'blocked')
WHERE a.ended_at IS NULL
ORDER BY a.role DESC, a.spawned_at;
