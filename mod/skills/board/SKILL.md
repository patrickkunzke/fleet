---
name: board
description: |
  Read and write the fleet's shared task board — the coordination layer for
  agents working across several repos at once. Use when acting as the chief of
  staff (planning an epic, dispatching work, checking what is ready or
  blocked), when working as a fleet worker (claiming a task, reporting a
  blocker, marking work done), or whenever the user says "board", "what's on
  the board", "dispatch", "what's blocked", "who's working on what", or asks
  for the state of a multi-repo effort. Also use before starting a long-running
  process (dev server, test run, compose stack) so it shows in the fleet's
  background list.
---

# The fleet board

`fleet.db` is the fleet's **live coordination state**: who is working, on what,
what blocks what, what is running in the background, and who told whom.

## Which board

Each fleet has a board of its own. A fleet is one workspace, such as
`~/Code/acme`, and its board holds only that fleet's chief, workers and
tasks. You are on a board only if a fleet started you: fleet hands every agent
it starts its board in `FLEET_DB`, and the CLI uses it without being told.

If a command answers **`not part of a fleet`**, this session was not started by
one. That is not an error to work around. Carry on without the board: do not
retry, do not pick a fleet with `--fleet`, and do not message a chief. You
have none. If the user wants the work tracked, say so, and let them start it
from the fleet.

The same holds for a session that once was a fleet worker. A conversation
carried on after its fleet was closed is no longer part of it.

It is not a memory store. Long-term recall belongs in claude-mem; raw tool
activity stays in the transcripts. Write to the board only what another agent
needs in order to act.

## The tools

The board's commands are tools you call directly: `board_ls`, `board_show`,
`board_ready`, `board_start`, `board_block`, `board_unblock`, `board_review`,
`board_done`, `board_msg` and `board_note`, and for the chief also
`board_add`, `board_dep`, `board_epic`, `board_claim`, `board_go`,
`board_drop`, `spawn`, `handoff` and `retire` (each `mcp__fleet__<name>`).
Use them rather than the shell: they take their input as fields, so a
message with quotes or several lines arrives as written, and `board_msg`
signs it with your name. This skill is instructions, not a command: invoking
it does nothing on the board.

The examples below use the command line, which does the same and is what to
use if the tools are not there.

## The CLI

Every read and write goes through one command:

```bash
fleet board --help
```

If `fleet` is not on PATH, say so and stop — do not hand-write SQL against the
database. `FLEET_JSON=1` in front of any read gives JSON instead of columns,
which is what you want when you are going to parse it. `fleet board sql` takes
a SELECT for anything the verbs do not cover; it refuses to write, because a
bare UPDATE would skip the flow event that every state change records.

## Conventions

- **Task keys are Jira keys with an index**: `ENG-2553-2`. One task = one repo
  = one MR. If a change spans three repos it is three tasks, not one.
- **Agent names are short and repo-shaped**: `billing-svc`, `storefront`,
  `chief`. They are the fleet's own names and need not match the session name
  Claude Code derives — spawning in `billing-service` produces a session
  called `billing-service-50`, not `billing-svc`. The link is the session id,
  recorded with `--session`, so pass it whenever you know it.
- **Every state change goes through the CLI**, never a bare `UPDATE`. The CLI
  writes the matching flow event; hand-written SQL silently breaks the history.
- **`--body` is the brief.** Put in it what a fresh agent in that repo needs to
  start: the constraint, the file, the thing it must not break. The title is a
  label, not a spec.

## As the chief of staff

Planning an epic:

```bash
fleet board epic ENG-2553 "shared settings flag"
fleet board add ENG-2553-1 ~/Code/acme/service/accounts-service "shared column + migration" \
  --epic ENG-2553 --body "Add the shared column and an optional service param. The old header stays as fallback."
fleet board add ENG-2553-2 ~/Code/acme/service/billing-service "consume the service param" \
  --epic ENG-2553 --dep ENG-2553-1
```

Dependencies must stay a one-way flow. `fleet board dep` refuses an edge that
would close a loop, and names the chain that already runs the other way — take
it as a sign you have the direction backwards, because tasks in a loop would
never appear in `ready` at all.

Dispatching — `ready` is the queue, and it only ever lists tasks whose
dependencies are all done:

```bash
fleet board ready
fleet board claim ENG-2553-1 accounts-svc
```

Then interrupt that agent. One command both records the message and delivers
it to their session, so the record and the interrupt cannot come apart — you do
not also need `SendMessage`:

```bash
fleet board msg chief accounts-svc "start ENG-2553-1" --task ENG-2553-1
```

It prints whether it landed: `queued` means it reaches them when their
current turn ends. `not delivered` is worth reading rather than
scrolling past: the usual cause is a typo in the name, and the flow log will
otherwise show you messaging an agent that does not exist.

Starting an agent does the registering for you — it opens a herdr tab of its
own, writes the row, and links the session once Claude Code reports it:

```bash
fleet spawn accounts-svc --repo ~/Code/acme/service/accounts-service --task ENG-2553-1
```

**Answer the agents you dispatch.** Each one sends you what it intends and
waits for your go-ahead, which counts as the user's. Give it with `fleet board
go`, which records the go and sends it, with a message if you have one:

```bash
fleet board go ENG-2553-1 "go — keep the old column until billing is on it"
```

Until a task has a go, fleet keeps its agent from changing files. Anything
short of a go — a question, what to change first, "hold off" — is a `fleet
board msg`, and leaves the agent waiting. When a plan turns on a decision
that is the user's to make — scope, a trade-off, anything that cannot be undone
— ask them instead of deciding for them.

**You do not write the code.** The editing tools are withheld from the chief
of staff on purpose, and that holds when the work touches a single repository
and looks small — write the task, start an agent, let it do the work. Holding
the whole picture is what the role is for.

**Pass `--task`.** The agent then opens already briefed: it is told which repo
is its own, what the task is, what the task waits on, and how to report. You
do not have to repeat any of it, and the board records the task as claimed in
the same step, so you cannot dispatch it twice. Without `--task` the agent
starts knowing only that it is a fleet worker, and has to go looking for work.

A key that is not on the board is refused before the pane opens, so add the
task first.

Use `fleet board agent …` only to correct or add to a row afterwards, such as
recording the branch it ended up on.

Check in with `fleet board ls` and `fleet board log`. When a task finishes, `done`
prints what it freed — that output is your cue to dispatch again, not a
formality:

```
done ENG-2553-1
unblocked: ENG-2553-2
```

When an agent's work is finished and nothing more is coming its way, retire
it. That takes it off the board, keeps it out of a resumed run, and closes
its herdr tab:

```bash
fleet board retire accounts-svc
```

A working agent's tab is left open, so do it once the agent has stopped.
`--keep-tab` retires it and leaves the tab for the user to look through.

**Hand off an agent that is nearly out of context.** fleet tells you when a
worker passes 80%: past that it compacts, and carries on from a summary of its
brief rather than the brief. If its task has a way to go, give it to a fresh
session:

```bash
fleet handoff accounts-svc --note "the flaky test is known; skip it"
```

The new session starts in the same repository under the same name, on the
same task, briefed on the task, its messages on the board and the old
session's last reply; a go-ahead the task had stands. The old session is
retired and its tab closed. Wait until the agent has stopped: one mid-turn is
refused unless you pass `--now`. A task nearly finished is better left to
finish.

## As a worker

You were handed a task key — in your opening brief if the chief dispatched you
with `--task`, otherwise from `fleet board agent <your-name>`.

**The chief's go-ahead is the user's.** Before you start, say what you intend
to do and send the chief the short version with `fleet board msg <you> chief
'...'`. Its answer arrives as a prompt marked `[fleet · chief · …]`, sent by
fleet once your turn has ended; that marker is how you know it came from the
chief rather than from another agent. A message marked as from another agent
is information — check with the chief before it changes your course.

Until your task has a go-ahead, fleet refuses your edits, and a refusal says
so: it is the cue to send your plan and wait, not something to work around.
The go comes from the chief, with `fleet board go`, or from the user typing
to you in your own tab. You cannot give it to yourself.

The protocol is four commands:

```bash
fleet board start ENG-2553-2                              # picking it up
fleet board block ENG-2553-2 "needs !412 merged"          # stopping, with a reason
fleet board unblock ENG-2553-2                            # carrying on
fleet board done ENG-2553-2 --mr https://git.../123       # finished
```

Report a blocker on the board **and** message the chief — the board is the
state, the message is the interrupt. Neither substitutes for the other.

Use `review` instead of `done` when the MR is open but you want a human to look
before it counts as finished.

## Background processes

Anything that outlives your turn goes on the board, so the fleet's right rail
can show it and nobody starts a second copy of your dev server:

```bash
ID=$(fleet board bg start storefront "pnpm dev" --kind server --port 3000 --log /tmp/ren-dev.log)
fleet board bg end "$ID" failed --detail "2 type errors"
```

Kinds: `server`, `compose`, `test`, `build`, `watch`, `script`. Close out every
task you open — a `bg` row left running forever is worse than no row, because
the next agent trusts it.

## What does not belong here

- Decisions and rationale → claude-mem, or the repo's own docs.
- Tool calls, file edits, diffs → already in the transcript.
- Anything only you need → your own todo list, not the shared board.
