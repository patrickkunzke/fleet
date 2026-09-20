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

It is not a memory store. Long-term recall belongs in claude-mem; raw tool
activity stays in the transcripts. Write to the board only what another agent
needs in order to act.

## The CLI

Every read and write goes through one command. Find it once per session:

```bash
FLEET="$(command -v fleet || echo "$HOME/Code/side-projects/fleet/cli/board.sh")"
"$FLEET" help
```

If neither exists, say so and stop — do not hand-write SQL against the
database. `FLEET_JSON=1` in front of any read gives JSON instead of a table,
which is what you want when you are going to parse it.

## Conventions

- **Task keys are Jira keys with an index**: `ENG-2553-2`. One task = one repo
  = one MR. If a change spans three repos it is three tasks, not one.
- **Agent names are short and repo-shaped**: `billing-svc`, `storefront`,
  `chief`. They must match the session name in `~/.claude/sessions/*.json`
  so the TUI can join them to a live process.
- **Every state change goes through the CLI**, never a bare `UPDATE`. The CLI
  writes the matching flow event; hand-written SQL silently breaks the history.
- **`--body` is the brief.** Put in it what a fresh agent in that repo needs to
  start: the constraint, the file, the thing it must not break. The title is a
  label, not a spec.

## As the chief of staff

Planning an epic:

```bash
"$FLEET" epic ENG-2553 "shared settings flag"
"$FLEET" add ENG-2553-1 ~/Code/acme/service/accounts-service "shared column + migration" \
  --epic ENG-2553 --body "Add the shared column and an optional service param. the old header stays as fallback."
"$FLEET" add ENG-2553-2 ~/Code/acme/service/billing-service "consume the service param" \
  --epic ENG-2553 --dep ENG-2553-1
```

Dispatching — `ready` is the queue, and it only ever lists tasks whose
dependencies are all done:

```bash
"$FLEET" ready
"$FLEET" claim ENG-2553-1 accounts-svc
```

Then message that agent with `SendMessage`, and log it so the flow pane sees it:

```bash
"$FLEET" msg chief accounts-svc "start ENG-2553-1" --task ENG-2553-1
```

Register an agent when you spawn one, so the board can join it to its session:

```bash
"$FLEET" agent accounts-svc --repo ~/Code/acme/service/accounts-service \
  --tmux fleet:accounts-svc --session "$SESSION_ID" --branch feature/ENG-2553-1
```

Check in with `"$FLEET" ls` and `"$FLEET" log`. When a task finishes, `done`
prints what it freed — that output is your cue to dispatch again, not a
formality:

```
done ENG-2553-1
unblocked: ENG-2553-2
```

## As a worker

You were handed a task key. The protocol is four commands:

```bash
"$FLEET" start ENG-2553-2                              # picking it up
"$FLEET" block ENG-2553-2 "needs !412 merged"          # stopping, with a reason
"$FLEET" unblock ENG-2553-2                            # carrying on
"$FLEET" done ENG-2553-2 --mr https://git.../123       # finished
```

Report a blocker on the board **and** message the chief — the board is the
state, the message is the interrupt. Neither substitutes for the other.

Use `review` instead of `done` when the MR is open but you want a human to look
before it counts as finished.

## Background processes

Anything that outlives your turn goes on the board, so the fleet's right rail
can show it and nobody starts a second copy of your dev server:

```bash
ID=$("$FLEET" bg start storefront "pnpm dev" --kind server --port 3000 --log /tmp/ren-dev.log)
"$FLEET" bg end "$ID" failed --detail "2 type errors"
```

Kinds: `server`, `compose`, `test`, `build`, `watch`, `script`. Close out every
task you open — a `bg` row left running forever is worse than no row, because
the next agent trusts it.

## What does not belong here

- Decisions and rationale → claude-mem, or the repo's own docs.
- Tool calls, file edits, diffs → already in the transcript.
- Anything only you need → your own todo list, not the shared board.
