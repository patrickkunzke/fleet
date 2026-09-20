#!/usr/bin/env bash
# fleet board — the coordination CLI for a multi-repo agent fleet.
#
# This shell script IS the interface contract. The Rust binary will grow the
# same subcommands with the same output, so the /board skill never has to
# change when the implementation does.
#
#   FLEET_DB    path to the database   (default ~/.claude-fleet/fleet.db)
#   FLEET_JSON  set to 1 for JSON out  (default: aligned columns)

set -euo pipefail

FLEET_DB="${FLEET_DB:-$HOME/.claude-fleet/fleet.db}"
SCHEMA="${FLEET_SCHEMA:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/schema.sql}"

die() { printf 'fleet: %s\n' "$*" >&2; exit 1; }

# Single-quote a value for inline SQL. Everything user-supplied goes through
# this — task titles contain apostrophes and bodies contain newlines.
q() {
  local s=${1-}
  [ -n "$s" ] || { printf 'NULL'; return; }
  # The pattern and replacement come from variables on purpose: writing them
  # inline as \' leaves the backslash in the replacement on bash 3.2.
  local one="'" two="''"
  printf "'%s'" "${s//$one/$two}"
}

sql() {
  local fmt=(-batch)
  if [ "${FLEET_JSON:-0}" = "1" ]; then fmt+=(-json); else fmt+=(-box); fi
  sqlite3 "${fmt[@]}" "$FLEET_DB" "$1"
}

# No formatting — for writes and single-value reads.
run() { sqlite3 -batch "$FLEET_DB" "$1"; }

need_db() { [ -f "$FLEET_DB" ] || die "no database at $FLEET_DB — run 'fleet board init'"; }

agent_id() {
  local id; id=$(run "SELECT id FROM agents WHERE name = $(q "$1");")
  [ -n "$id" ] || die "unknown agent '$1' — register it with 'fleet board agent $1 --repo <path>'"
  printf '%s' "$id"
}

task_exists() {
  local n; n=$(run "SELECT count(*) FROM tasks WHERE key = $(q "$1");")
  [ "$n" = "1" ] || die "unknown task '$1'"
}

# Record a semantic event. Every state change goes through here so the flow
# pane and the history stay in step with the board.
event() { # kind from to task summary [body] [ref]
  run "INSERT INTO events (kind, from_agent, to_agent, task_key, summary, body, ref)
       VALUES ($(q "$1"), $(q "${2-}"), $(q "${3-}"), $(q "${4-}"), $(q "$5"), $(q "${6-}"), $(q "${7-}"));"
}

usage() {
  cat <<'EOF'
fleet board — read and write the fleet's task board

  init                                 create the database
  ls [--state S] [--epic K] [--repo R] the board
  ready                                queued tasks whose deps are all done
  show <TASK>                          one task in full
  log [N]                              recent flow events (default 20)

  epic <KEY> <title>                   create or retitle an epic
  add <TASK> <repo> <title> [opts]     queue a task
       --epic K  --body TEXT  --dep TASK  --pos N
  dep <TASK> <depends-on-TASK>         add a dependency

  agent <name> [opts]                  register or update an agent
       --role chief|worker  --repo PATH  --session ID  --tmux TARGET  --branch B
  agents                               live agents and what they hold
  retire <name>                        mark an agent ended

  claim <TASK> <agent>                 assign
  start <TASK>                         queued  -> running
  block <TASK> <reason>                running -> blocked
  unblock <TASK>                       blocked -> running
  review <TASK> [--mr URL]             running -> review
  done <TASK> [--mr URL]               -> done, unblocks dependents
  drop <TASK> [reason]                 -> dropped

  msg <from> <to> <summary> [--body T] [--task K]   log a message
  note <summary> [--body T] [--task K]              log a note

  bg start <agent> <command> [--kind K] [--port P] [--log F] [--repo R]
  bg end <id> passed|failed|killed [--detail T]
  bg ls

  sql <query>                          escape hatch
EOF
}

cmd_init() {
  mkdir -p "$(dirname "$FLEET_DB")"
  [ -f "$SCHEMA" ] || die "schema not found at $SCHEMA"
  sqlite3 -batch "$FLEET_DB" < "$SCHEMA" >/dev/null
  printf 'fleet: ready at %s\n' "$FLEET_DB"
}

cmd_ls() {
  need_db
  local where="1=1"
  while [ $# -gt 0 ]; do
    case "$1" in
      --state) where="$where AND state = $(q "$2")"; shift 2 ;;
      --epic)  where="$where AND epic_key = $(q "$2")"; shift 2 ;;
      --repo)  where="$where AND repo LIKE $(q "%$2%")"; shift 2 ;;
      *) die "ls: unknown option '$1'" ;;
    esac
  done
  sql "SELECT epic_key AS epic, key, state, agent, title, waiting_on, mr_url AS mr
       FROM v_board WHERE $where;"
}

cmd_ready()  { need_db; sql "SELECT epic_key AS epic, key, repo, title FROM v_ready;"; }
cmd_agents() { need_db; sql "SELECT * FROM v_agents;"; }

cmd_show() {
  need_db; task_exists "$1"
  sql "SELECT * FROM v_board WHERE key = $(q "$1");"
  sql "SELECT ts, kind, from_agent, to_agent, summary
       FROM events WHERE task_key = $(q "$1") ORDER BY ts, id;"
  run "SELECT body FROM tasks WHERE key = $(q "$1");"
}

cmd_log() {
  need_db
  sql "SELECT ts, kind, from_agent AS \"from\", to_agent AS \"to\", task_key AS task, summary
       FROM events ORDER BY ts DESC, id DESC LIMIT ${1:-20};"
}

cmd_epic() {
  need_db
  [ $# -ge 2 ] || die "epic: need <KEY> <title>"
  run "INSERT INTO epics (key, title) VALUES ($(q "$1"), $(q "$2"))
       ON CONFLICT(key) DO UPDATE SET title = excluded.title;"
  printf 'epic %s\n' "$1"
}

cmd_add() {
  need_db
  [ $# -ge 3 ] || die "add: need <TASK> <repo> <title>"
  local key="$1" repo="$2" title="$3"; shift 3
  local epic="" body="" pos=0 deps=()
  while [ $# -gt 0 ]; do
    case "$1" in
      --epic) epic="$2"; shift 2 ;;
      --body) body="$2"; shift 2 ;;
      --dep)  deps+=("$2"); shift 2 ;;
      --pos)  pos="$2"; shift 2 ;;
      *) die "add: unknown option '$1'" ;;
    esac
  done
  [ -d "$repo" ] || die "add: repo '$repo' is not a directory"
  repo="$(cd "$repo" && pwd)"

  local epic_sql="NULL"
  if [ -n "$epic" ]; then
    run "INSERT OR IGNORE INTO epics (key, title) VALUES ($(q "$epic"), $(q "$epic"));"
    epic_sql="(SELECT id FROM epics WHERE key = $(q "$epic"))"
  fi

  run "INSERT INTO tasks (key, epic_id, title, body, repo, position)
       VALUES ($(q "$key"), $epic_sql, $(q "$title"), $(q "$body"), $(q "$repo"), $pos);"

  local d
  for d in ${deps+"${deps[@]}"}; do cmd_dep "$key" "$d" >/dev/null; done

  event task chief "" "$key" "queued: $title"
  printf 'queued %s  %s\n' "$key" "$title"
}

cmd_dep() {
  need_db; task_exists "$1"; task_exists "$2"
  run "INSERT OR IGNORE INTO task_deps (task_id, depends_on)
       VALUES ((SELECT id FROM tasks WHERE key = $(q "$1")),
               (SELECT id FROM tasks WHERE key = $(q "$2")));"
  printf '%s waits on %s\n' "$1" "$2"
}

cmd_agent() {
  need_db
  [ $# -ge 1 ] || die "agent: need <name>"
  local name="$1"; shift
  local sets=() role="" repo=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --role)    role="$2"; sets+=("role = $(q "$2")"); shift 2 ;;
      --repo)    repo="$(cd "$2" && pwd)"; sets+=("repo = $(q "$repo")"); shift 2 ;;
      --session) sets+=("session_id = $(q "$2")"); shift 2 ;;
      --tmux)    sets+=("tmux_target = $(q "$2")"); shift 2 ;;
      --branch)  sets+=("branch = $(q "$2")"); shift 2 ;;
      --note)    sets+=("note = $(q "$2")"); shift 2 ;;
      *) die "agent: unknown option '$1'" ;;
    esac
  done
  run "INSERT OR IGNORE INTO agents (name, role, repo)
       VALUES ($(q "$name"), $(q "${role:-worker}"), $(q "$repo"));"
  if [ ${#sets[@]} -gt 0 ]; then
    local joined; joined=$(IFS=,; printf '%s' "${sets[*]}")
    run "UPDATE agents SET $joined, ended_at = NULL WHERE name = $(q "$name");"
  fi
  printf 'agent %s\n' "$name"
}

cmd_retire() {
  need_db; agent_id "$1" >/dev/null
  run "UPDATE agents SET ended_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE name = $(q "$1");"
  event note "$1" "" "" "agent retired"
  printf 'retired %s\n' "$1"
}

cmd_claim() {
  need_db; task_exists "$1"; local aid; aid=$(agent_id "$2")
  run "UPDATE tasks SET agent_id = $aid WHERE key = $(q "$1");"
  event task chief "$2" "$1" "assigned to $2"
  printf '%s -> %s\n' "$1" "$2"
}

# Move a task and log it. transition <key> <new state> <summary> [extra SET]
transition() {
  need_db; task_exists "$1"
  local extra="${4:+, $4}"
  run "UPDATE tasks SET state = $(q "$2")$extra WHERE key = $(q "$1");"
  local owner; owner=$(run "SELECT COALESCE(a.name,'') FROM tasks t
                            LEFT JOIN agents a ON a.id = t.agent_id
                            WHERE t.key = $(q "$1");")
  event task "$owner" "" "$1" "$3"
}

cmd_start()  { transition "$1" running "started" "started_at = strftime('%Y-%m-%dT%H:%M:%SZ','now'), blocked_on = NULL"; printf 'running %s\n' "$1"; }
cmd_unblock(){ transition "$1" running "unblocked" "blocked_on = NULL"; printf 'running %s\n' "$1"; }

cmd_block() {
  [ $# -ge 2 ] || die "block: need <TASK> <reason>"
  transition "$1" blocked "blocked: $2" "blocked_on = $(q "$2")"
  printf 'blocked %s — %s\n' "$1" "$2"
}

cmd_review() {
  local key="$1"; shift; local mr=""
  while [ $# -gt 0 ]; do case "$1" in --mr) mr="$2"; shift 2 ;; *) die "review: unknown option '$1'" ;; esac; done
  transition "$key" review "ready for review${mr:+ — $mr}" "${mr:+mr_url = $(q "$mr")}"
  printf 'review %s\n' "$key"
}

cmd_done() {
  local key="$1"; shift; local mr=""
  while [ $# -gt 0 ]; do case "$1" in --mr) mr="$2"; shift 2 ;; *) die "done: unknown option '$1'" ;; esac; done
  transition "$key" done "done${mr:+ — $mr}" "done_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')${mr:+, mr_url = $(q "$mr")}"
  printf 'done %s\n' "$key"
  # Anything that was only waiting on this is now dispatchable — say so, loudly,
  # because that is the chief's cue to act. Covers 'blocked' as well as
  # 'queued': an agent may have parked the task itself before the dep landed.
  local freed
  freed=$(run "SELECT group_concat(t.key, ' ') FROM tasks t
               WHERE t.state IN ('queued','blocked')
                 AND t.id IN (SELECT task_id FROM task_deps WHERE depends_on =
                       (SELECT id FROM tasks WHERE key = $(q "$key")))
                 AND NOT EXISTS (SELECT 1 FROM task_deps d JOIN tasks p ON p.id = d.depends_on
                                  WHERE d.task_id = t.id AND p.state <> 'done');")
  [ -n "$freed" ] && printf 'unblocked: %s\n' "$freed"
  return 0
}

cmd_drop() {
  transition "$1" dropped "dropped${2:+: $2}"
  printf 'dropped %s\n' "$1"
}

cmd_msg() {
  need_db
  [ $# -ge 3 ] || die "msg: need <from> <to> <summary>"
  local from="$1" to="$2" summary="$3"; shift 3
  local body="" task=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --body) body="$2"; shift 2 ;;
      --task) task="$2"; shift 2 ;;
      *) die "msg: unknown option '$1'" ;;
    esac
  done
  event message "$from" "$to" "$task" "$summary" "$body"
  printf '%s -> %s: %s\n' "$from" "$to" "$summary"
}

cmd_note() {
  need_db
  [ $# -ge 1 ] || die "note: need <summary>"
  local summary="$1"; shift
  local body="" task=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --body) body="$2"; shift 2 ;;
      --task) task="$2"; shift 2 ;;
      *) die "note: unknown option '$1'" ;;
    esac
  done
  event note "" "" "$task" "$summary" "$body"
  printf 'noted\n'
}

cmd_bg() {
  need_db
  case "${1:-}" in
    start)
      shift
      [ $# -ge 2 ] || die "bg start: need <agent> <command>"
      local aid; aid=$(agent_id "$1"); local command="$2"; shift 2
      local kind=script port=NULL log="" repo=""
      while [ $# -gt 0 ]; do
        case "$1" in
          --kind) kind="$2"; shift 2 ;;
          --port) port="$2"; shift 2 ;;
          --log)  log="$2"; shift 2 ;;
          --repo) repo="$2"; shift 2 ;;
          *) die "bg start: unknown option '$1'" ;;
        esac
      done
      # RETURNING, not last_insert_rowid(): every run() is a fresh sqlite3
      # process, so the rowid of the previous connection is long gone.
      local id
      id=$(run "INSERT INTO bg_tasks (agent_id, repo, command, kind, port, log_path)
                VALUES ($aid, $(q "$repo"), $(q "$command"), $(q "$kind"), $port, $(q "$log"))
                RETURNING id;")
      event bg "" "" "" "started: $command" "" "$id"
      printf '%s\n' "$id"
      ;;
    end)
      shift
      [ $# -ge 2 ] || die "bg end: need <id> <passed|failed|killed>"
      local id="$1" state="$2"; shift 2
      local detail=""
      while [ $# -gt 0 ]; do
        case "$1" in --detail) detail="$2"; shift 2 ;; *) die "bg end: unknown option '$1'" ;; esac
      done
      run "UPDATE bg_tasks SET state = $(q "$state"), detail = $(q "$detail"),
             ended_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id = $id;"
      local cmd_text; cmd_text=$(run "SELECT command FROM bg_tasks WHERE id = $id;")
      [ -n "$cmd_text" ] || die "bg end: no background task with id $id"
      event bg "" "" "" "$state: $cmd_text" "$detail" "$id"
      printf '%s %s\n' "$state" "$cmd_text"
      ;;
    ls|"")
      sql "SELECT b.id, a.name AS agent, b.kind, b.command, b.state, b.port, b.detail, b.started_at
           FROM bg_tasks b LEFT JOIN agents a ON a.id = b.agent_id
           WHERE b.state = 'running' OR b.ended_at > datetime('now','-1 hour')
           ORDER BY b.started_at DESC;"
      ;;
    *) die "bg: unknown subcommand '$1'" ;;
  esac
}

case "${1:-ls}" in
  init)    shift; cmd_init "$@" ;;
  ls)      shift; cmd_ls "$@" ;;
  ready)   shift; cmd_ready "$@" ;;
  show)    shift; cmd_show "$@" ;;
  log)     shift; cmd_log "$@" ;;
  epic)    shift; cmd_epic "$@" ;;
  add)     shift; cmd_add "$@" ;;
  dep)     shift; cmd_dep "$@" ;;
  agent)   shift; cmd_agent "$@" ;;
  agents)  shift; cmd_agents "$@" ;;
  retire)  shift; cmd_retire "$@" ;;
  claim)   shift; cmd_claim "$@" ;;
  start)   shift; cmd_start "$@" ;;
  block)   shift; cmd_block "$@" ;;
  unblock) shift; cmd_unblock "$@" ;;
  review)  shift; cmd_review "$@" ;;
  done)    shift; cmd_done "$@" ;;
  drop)    shift; cmd_drop "$@" ;;
  msg)     shift; cmd_msg "$@" ;;
  note)    shift; cmd_note "$@" ;;
  bg)      shift; cmd_bg "$@" ;;
  sql)     shift; need_db; sql "$1" ;;
  -h|--help|help) usage ;;
  *) die "unknown subcommand '$1' — try 'fleet board help'" ;;
esac
