# fleet

A [herdr](https://herdr.dev) plugin that orchestrates Claude Code sessions
working across several repos at once — a chief-of-staff session that plans and
dispatches, worker sessions that do the work, and one view to watch them
interact. herdr draws every agent's terminal; fleet is the crew, the board and
the graph. It runs only inside herdr: there is no standalone mode.

The equivalent of Claude Projects' coordinator/thread model, built out of what
already runs on the machine.

## Status

| Piece | State |
|---|---|
| `schema.sql` — the coordination database | done |
| `skills/board` — the `/board` skill | done |
| `src/registry.rs` — session discovery and watching | done |
| `src/db.rs` — fleet.db reads and writes, and runs | done |
| `src/scope.rs` — one board per fleet, and who is in one | done |
| `fleet board` — the whole board, in the binary | done |
| `src/agent.rs` — starting, resuming, repo discovery | done |
| `src/brief.rs` — what an agent is told when it starts | done |
| `src/msg.rs` — the line a message arrives as | done |
| `src/herdr.rs` — typed calls to the herdr CLI | done |
| `src/host.rs` — agents in herdr tabs | done |
| `src/plugin.rs` + `herdr-plugin.toml` — the plugin's actions, fleet mode | done |
| `src/ui/` — the fleet view: graph, log, board, pickers | done |
| `src/ui/hosting.rs` — herdr's events, sidebar labels, notifications | done |
| `src/ui/preview.rs` — the fixture fleet, for layout work | done |

`cargo test` runs 179 tests.

## What it is built on

Nothing here invents state that Claude Code already keeps. It joins four
sources, and owns only the first:

| Source | Holds | Access |
|---|---|---|
| `~/.claude-fleet/fleets/<fleet>/fleet.db` | one fleet's tasks, agents, flow events, background processes | ours, read/write |
| `~/.claude/sessions/<pid>.json` | live sessions: name, cwd, status, socket | read, watched |
| `~/.claude/projects/<slug>/<id>.jsonl` | raw tool activity per session | read, tailed |
| `~/.claude-mem/claude-mem.db` | long-term recall across every project | read only |

The split that matters: **fleet.db is coordination, claude-mem is memory.**
claude-mem is observational and lags behind by a background worker, which makes
it excellent for "what did we decide about the shared flag" and useless for
"has !412 landed yet". Anything an agent must act on right now goes in
fleet.db; anything worth remembering next month goes to claude-mem.

### One board per fleet

A fleet is one workspace — the directory fleet is started in, such as
`~/Code/acme` — and it has a board of its own, named after the directory
(`fleet fleets` lists them). Its chief, its workers and its tasks are on that
board and no other, and a message reaches only an agent of the same fleet.

Membership comes from how a session was started, not from where it stands.
Every agent a fleet starts is handed its board in `FLEET_DB`; a Claude
session fleet did not start has none, and `fleet board` refuses it with
`not part of a fleet`. It has to be that way round: a repository inside the
landscape is not thereby in the landscape's fleet, and a conversation carried
on after its fleet closed is no longer a worker in it. From a shell, name a
fleet: `fleet board ls --fleet acme`.

In herdr, agents are named for their fleet as well — `acme-chief` — since
herdr wants live agent names unique across its workspaces and every fleet has
a chief. On the board, and in messages, it is still `chief`.

## Install (development)

```bash
./install.sh
```

Builds the binary, links it to `~/.local/bin/fleet` (the agents call
`fleet board` and `fleet spawn` by name), links `skills/board` into
`~/.claude/skills/`. Then link the plugin into herdr: see
[Setting it up](#setting-it-up). There is no database to create: each fleet makes its own
the first time fleet starts in its workspace.

## Inside herdr

herdr owns terminals properly — scrolling, selecting, pasting, the mouse,
reattaching after a reboot — so fleet does not draw terminals at all. herdr
draws every agent in a tab of its own; fleet is the tab that shows the crew,
with the chief on its left.

```
herdr sidebar      tabs in the workspace
─────────────      ───────────────────────────────────────────────────
▾ acme          fleet · billing-service · storefront
    ● chief
    ○ content-…    ┌ chief ──────────┬ fleet ─────────────────────────┐
    ● renaissa…    │ claude          │ graph, or log     │ the board  │
                   └─────────────────┴────────────────────────────────┘
```

- **The fleet tab** has the chief on the left, half the width: it is the
  one you talk to, and the board is what you watch while you do. The view on
  the right has the graph and the board. It has no agent rail: herdr's
  sidebar lists the live agents, and the graph's cards are every agent,
  started or not. `←→` moves between cards, and `↵` switches to the agent's
  tab, or to the chief's pane. `l` flips between the graph and the log, `n`
  starts an agent, `r` brings a crew back, `x` retires one, `q` quits. None of
  them needs a prefix: no key in this tab belongs to an agent.
- **Each agent is a herdr tab** named after it, in the repository it works
  in. herdr's sidebar shows which ones are working and which are waiting on
  you — and, on each agent's second line where herdr would say `claude`, what
  it is on: `ENG-2553-2 · waits on ENG-2553-1`, `ENG-2553-2 · blocked: needs
  the flag`, `ENG-2553-2 · review`. The chief's line is the board at a
  glance: `3 open · 1 blocked · 1 in review`. The labels expire ten minutes
  after fleet stops, rather than going stale.
- **State is herdr's.** The fleet tab follows herdr's event stream, so an
  agent's state on its card changes when herdr sees it change, and an agent
  stopped at a question or permission dialog shows as `! needs you` — which
  the session registry alone could not tell.
- **Notifications** when a task is blocked, ready for review, or done, through
  herdr's own (`[ui.toast] delivery`; herdr's default is off). Only new
  events: opening fleet does not replay yesterday's. An agent at a dialog is
  not announced twice — herdr signals that itself.
- **The chief** starts on the fleet view's left the first time. It is briefed
  exactly as before and still runs without the editing tools, and
  `fleet spawn` run by the chief opens the new agent's tab in the same
  workspace. Quitting the view with `q` closes only its own pane while the
  chief is there; opening fleet again puts the view back on its right.
- **Fleet mode** for a new workspace, two ways. `fleet.new` makes a workspace
  at the focused pane's directory and opens fleet in it, chief and all. And a
  workspace opened at a directory listed in `~/.claude-fleet/auto-open` gets
  the same by itself; any other workspace is left alone, since every one
  would be a Claude session started for nothing. Either way the view takes
  the new workspace's first tab rather than leaving an empty shell beside it.
- **Messages** from `fleet board msg` go in through `herdr agent prompt`,
  which takes the pane's bracketed paste into account. A message to an agent
  sitting at a permission dialog is refused before anything is typed, and is
  on the board for when the dialog is answered.
- **Resuming**: after herdr restarts, the tabs come back as shells. Open
  fleet and it offers the run; choosing it starts each agent again in its own
  tab, back in its own conversation, with its role and (for the chief) the
  missing tools as before.

### Setting it up

```bash
herdr plugin link ~/Code/side-projects/fleet
fleet herdr doctor
```

Then, in `~/.config/herdr/config.toml`:

```toml
[session]
# herdr would resume agents itself, as plain `claude --resume <id>`: no role,
# and a chief with its editing tools back. fleet does it instead.
resume_agents_on_restore = false

[[keys.command]]
key = "prefix+f"
type = "plugin_action"
command = "fleet.open"
description = "fleet"

[[keys.command]]
key = "prefix+shift+f"
type = "plugin_action"
command = "fleet.new"
description = "new workspace in fleet mode"
```

`prefix+f` in a workspace opened at the landscape (`~/Code/acme`)
opens its fleet tab, or brings it forward. `prefix+shift+f` opens a new
workspace in fleet mode where the focused pane is. Typing `fleet` in any herdr
pane runs the view in that pane; outside herdr it refuses, and says why.

To have a landscape's workspaces open in fleet mode by themselves, list it:

```bash
echo '~/Code/acme' >> ~/.claude-fleet/auto-open
```

One directory a line, `#` for comments. It matches the workspace's own
directory exactly, so a workspace opened in one repository inside the
landscape stays an ordinary one.

Three things herdr taught, which is why the code looks the way it does:

- **`claude` is started by its full path** (`~/.local/bin/claude`, or
  `FLEET_CLAUDE`). A new pane's shell rebuilds PATH, and with a Node version
  manager in `.zshrc` an old npm install of Claude Code comes first — on this
  machine 2.0.76, which rejects the current settings file and sits at a
  dialog. herdr's own `agent start` runs whichever `claude` the shell finds,
  so fleet types the command itself and names the agent once herdr sees it.
- **The brief is read from files** (`~/.claude-fleet/briefs/`) by the launch
  line, `--append-system-prompt "$(cat …)"`. A page of prose typed at a shell
  prompt is one stray quote from `quote>`.
- **A new agent's session is linked late.** Claude Code registers it only
  after the folder-trust dialog is answered, which can be minutes later in a
  repository it has not seen. The fleet tab asks herdr for it on every
  refresh until it has it.

Other plugins are fine alongside: herdr-sidebar puts a narrow file list in
every tab, and fleet leaves it alone; zoetrope's `prefix+shift+z` on an
agent's tab shows that agent's own session as a graph.

herdr's client code (`src/herdr.rs`) is adapted from
[herdr-projects](https://github.com/eliasstravik/herdr-projects), MIT; see
[NOTICE](NOTICE).

## Use

The view opens from the plugin's actions (see above). Everything else is for
the agents, and for you from a shell in a herdr pane:

```bash
fleet spawn billing-svc --repo ~/Code/acme/service/billing-service
fleet fleets                                # every fleet and its directory

fleet board epic ENG-2553 "shared settings flag"
fleet board add ENG-2553-1 ~/Code/acme/service/accounts-service "shared column" --epic ENG-2553
fleet board add ENG-2553-2 ~/Code/acme/service/billing-service "consume param" --epic ENG-2553 --dep ENG-2553-1
fleet board ready     # only ENG-2553-1 — the other one is waiting
fleet board ls
fleet board log
```

`FLEET_JSON=1` before any read gives JSON. `FLEET_DB` names the board, which is
how an agent finds its fleet's and how the tests run against a scratch copy;
`--fleet <name>` picks one from a shell. `fleet board sql`
takes a SELECT and refuses anything else: every write goes through a verb that
records the matching flow event, and a bare UPDATE is the one way to break
that.

Opening fleet in a workspace starts a **chief of staff** there if one is not
already running. That is who you talk to; it plans the work and starts the
agents that do it. The graph shows only agents the fleet started, not every
session on the machine.

**Every agent opens already briefed**, in two halves. Who it is goes into the
system prompt with `--append-system-prompt`, where it outranks whatever a hook
injects as context later and survives compaction; what to do now goes in as the
first turn. Both as arguments rather than typed into the pane afterwards,
because a REPL that has not said it is ready will swallow half of a prompt.

**The chief cannot edit.** It starts with `--disallowed-tools Edit Write
NotebookEdit`, because asking was not enough: given a ticket touching a single
repository it did the work itself, which is the one thing it is not for. Its
role says so outright, including for the small single-repo case. Not airtight —
Bash can still write a file — but it removes the path of least resistance,
which is what matters against a session that drifts.

A worker keeps its tools and is told which repo is its own and not to leave it,
and, when dispatched with `--task`, the task, the body, what it waits on, and
how to report:

```bash
fleet spawn billing-svc --repo ~/Code/acme/service/billing-service --task ENG-2553-2
```

That also claims the task on the board in the same step, so it cannot be
dispatched twice, and a key that is not on the board is refused before the
tab opens. `--role chief` starts a chief instead — the other brief, the deny
list, and a row the graph draws as one. `--command` overrides the whole thing
and skips the briefing, which is how the plumbing is tested without starting a
real agent.

**The graph is the fleet as it is shaped:** the chief's card over a row of
agent cards, a line from it into each one. A line is heavy where the most has
gone along it and dashed where nothing has, and coloured by what the agent at
its end is doing — amber working, teal waiting — which is zoetrope's rule that
liveness should read on the structure itself. A message is drawn travelling
along its line as it is sent, and the line stays lit for a few seconds after.
Two agents that talk directly are joined under their cards. When the cards do
not fit one row they wrap, and the line from the chief runs down the left edge
to each row, as an org chart's does, instead of through the cards above; a
direct line between rows is written out rather than drawn across them. `←→`
moves between cards and `↵` goes to one. The board is watched, so a message
appears as it is written rather than on the next tick.

The flow views read the `events` table, which every state change and every
`fleet board msg` writes. **A message sent with `SendMessage` and never logged
does not appear there** — the board is the record, so an agent that does not
report is invisible to it by construction.

`fleet board msg` is both halves at once: it writes the event *and* puts the
message in front of the recipient, through herdr, so the interrupt and the
record cannot come apart. Delivery is best-effort and never costs the write —
an agent that has died still said what it said — and the CLI reports which
happened:

```
accounts-svc -> chief: the parameter is yours
      delivered to chief in herdr:acme-chief
```

It arrives as one line, marked, so the agent does not read a peer as the
person at the keyboard: `[fleet · accounts-svc · ENG-2553-2] the parameter is
yours`. A body is folded onto the same line, because a newline typed at a
prompt would submit half a message; past ~1500 characters it is a document
and the delivery says so rather than pasting a page into somebody's prompt.

## Picking up where you left off

A reboot, or herdr restarting: the agents' processes go, and their
conversations do not. Claude Code keeps each one, and `claude --resume`
continues it under the same id. What was missing was a record of which
conversations belonged together.

That record is a **run**: one stretch of work in one workspace — the chief and
every agent it started, each with the session it had. Every agent joins the run
it was started in; the chief's own environment carries `FLEET_RUN`, so an agent
it starts with `fleet spawn` lands in its run rather than whichever is newest.

Start fleet in a workspace with nothing running and a run to come back to, and
it asks before starting a fresh chief:

```
 pick up where you left off

 ▌ 28m ago · the chief + 2 agents
 ▌   ENG-2155-fixes, ENG-2155-review, ENG-2155, staging-fix
 ▌   chief, eng-2155, eng-2155-review
   start fresh — a new chief, nothing brought back
```

Choosing one relaunches each of its agents with `claude --resume` in its own
repository and its own tab, back on the graph as itself. The role goes back
into the system prompt and the chief's editing tools stay withheld, because
both are set at launch and not kept with the conversation. `r` opens the same
list at any time.

Two things are deliberately left out of a resume. An agent taken off the board
with `x` stays off — that was a choice, where a process dying is not, and
the two are recorded differently now. And an agent whose conversation is gone
from disk is named rather than resumed, since `--resume` on a missing
conversation opens an empty one that looks, at a glance, like the old agent.

A board from before runs existed has one made for it, once, from the sessions
the agents table still held — which is the last crew anyone ran.

## Working on the layout

Restarting a real fleet to look at a margin is the wrong loop: the agents are
the expensive part and the spacing has nothing to do with them. So there is a
fixture fleet.

```bash
./dev.sh                 # rebuild and redraw on every save
./dev.sh 140x40 log      # a size and a view: graph, log
fleet preview --plain    # the same frame as text, for a diff
```

`fleet preview` invents the whole thing — an in-memory board carrying every
task state and every agent presence, and no herdr behind it. It draws one
frame where your prompt was, in colour, and gives the shell back. Nothing it
does can reach a fleet's board, your registry, or herdr, so it is safe to run
beside a fleet that is up.

`dev.sh` polls for changes rather than needing `cargo-watch` or `fswatch`
installed, and builds debug — the release binary stays as it was, because
`~/.local/bin/fleet` is a symlink to it and somebody may have it open.

Worth knowing either way: **quitting fleet does not stop the agents.** herdr
owns them and the board is a file, so opening it again picks up everything
that is still running.

## Packaging plan

**Rust + ratatui, shipped as a Homebrew tap**, and linked into herdr as a
plugin.

Why Rust rather than Bun, given the rest of the stack is TypeScript:

- **Startup.** This is a tool you open dozens of times a day. A Rust binary is
  ready in single-digit milliseconds; a `bun build --compile` binary is ~60MB
  and takes tens of milliseconds before the first frame. That gap is the whole
  difference between a window you keep open and one you keep closed.
- **The workload is a steady parse.** Watching ~25 registry files, following
  herdr's event stream, and querying SQLite — all while rendering. No GC
  pauses mid-frame.
- **The graph pane needs sub-cell drawing.** ratatui's `Canvas` widget renders
  braille, so the live topology is actually drawn rather than approximated with
  box characters.
- **Precedent.** [zoetrope](https://github.com/furkankly/zoetrope) is this
  exact shape — a 3.8MB arm64 binary distributed through a personal tap — and
  it already parses Claude Code transcripts, MIT-licensed.

Bun is the faster route to a prototype if staying in TypeScript matters more
than the above; OpenTUI exists and `bun build --compile` does produce a single
binary. The cost is startup, size, and hand-rolling the braille drawing.

### Install story once it is a binary

```bash
brew tap me/tap
brew install fleet
fleet install-skill
```

Dev builds stay `cargo install --path .`. The tap is a second repo,
`homebrew-tap`, holding `Formula/fleet.rb` — the same arrangement zoetrope
uses.

## Design

Layout mockups live in the canvas at
the project's design canvas.

The frame: the flow — the crew as a graph, or the log — with the board beside
it (tasks above, background processes below), or under it in a narrow pane.
The agents themselves are herdr's tabs, not something fleet draws.
