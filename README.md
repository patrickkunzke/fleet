# fleet

A terminal orchestrator for Claude Code sessions working across several repos
at once — a chief-of-staff session that plans and dispatches, worker sessions
that do the work, and one TUI to watch them interact.

The equivalent of Claude Projects' coordinator/thread model, built out of what
already runs on the machine.

## Status

| Piece | State |
|---|---|
| `schema.sql` — the coordination database | done |
| `skills/board` — the `/board` skill | done |
| `src/registry.rs` — session discovery and watching | done, 6 tests |
| `src/transcript.rs` — transcript reading and tailing | done, 8 tests |
| `src/db.rs` — fleet.db reads and writes, and runs | done, 30 tests |
| `fleet board` — the whole board, in the binary | done, 13 tests |
| `src/tmux.rs` — spawning and pane control | done, 17 tests |
| `src/ui/` — frame, fleet rail, session pane, board rail | done, 57 tests |
| `src/agent.rs` — starting, resuming, repo discovery | done, 7 tests |
| `src/brief.rs` — what an agent is told when it starts | done, 23 tests |
| `src/ui/resume.rs` — picking a run to bring back | done, 5 tests |
| `src/msg.rs` — delivering a message to its recipient | done, 6 tests |
| `n` to spawn from the rail | done |
| `src/ui/flow.rs` — the flow pane and its log view | done, 10 tests |
| `src/ui/graph.rs` — the flow as a live graph | done, 16 tests |
| `src/ui/mirror.rs` — the live tmux pane | done, 8 tests |
| `src/ui/sender.rs` — input to panes, off the UI thread | done, 8 tests |
| `src/ui/selection.rs` — drag to copy from a pane | done, 6 tests |
| `src/ui/clipboard.rs` — pbcopy and OSC 52 | done, 3 tests |
| `src/ui/preview.rs` — the fixture fleet, for layout work | done, 4 tests |
| `src/herdr.rs` — typed calls to the herdr CLI | done, 7 tests |
| `src/host.rs` — tmux window or herdr tab, one interface | done, 2 tests |
| `src/plugin.rs` + `herdr-plugin.toml` — fleet as a herdr plugin, fleet mode | done, 7 tests |
| `src/ui/hosting.rs` — herdr's events, sidebar labels, notifications | done, 6 tests |

## What it is built on

Nothing here invents state that Claude Code already keeps. It joins four
sources, and owns only the first:

| Source | Holds | Access |
|---|---|---|
| `~/.claude-fleet/fleet.db` | tasks, agents, flow events, background processes | ours, read/write |
| `~/.claude/sessions/<pid>.json` | live sessions: name, cwd, status, socket | read, watched |
| `~/.claude/projects/<slug>/<id>.jsonl` | raw tool activity per session | read, tailed |
| `~/.claude-mem/claude-mem.db` | long-term recall across every project | read only |

The split that matters: **fleet.db is coordination, claude-mem is memory.**
claude-mem is observational and lags behind by a background worker, which makes
it excellent for "what did we decide about the shared flag" and useless for
"has !412 landed yet". Anything an agent must act on right now goes in
fleet.db; anything worth remembering next month goes to claude-mem.

## Install (development)

```bash
./install.sh
```

Builds the binary, links it to `~/.local/bin/fleet`, links `skills/board` into
`~/.claude/skills/`, and creates the database.

## Inside herdr

[herdr](https://herdr.dev) owns terminals properly — scrolling, selecting,
pasting, the mouse, reattaching after a reboot — which is everything fleet had
to rebuild by hand around tmux and never got to feel native. Run inside herdr,
fleet stops drawing terminals at all. herdr draws every agent in a tab of its
own; fleet is the tab that shows the crew, with the chief beside it.

```
herdr sidebar      tabs in the workspace
─────────────      ───────────────────────────────────────────────────
▾ acme          fleet · billing-service · storefront
    ● chief
    ○ content-…    ┌ fleet ──────────────────────────────┬ chief ─────┐
    ● renaissa…    │ agents │ graph, or log │ the board │ claude     │
                   └─────────────────────────────────────┴────────────┘
```

- **The fleet tab** has the rail, the graph and the board as before, and the
  chief in a split on the right, 40% of the width: it is the one you talk to,
  and the board is what you watch while you do. `↵` on an agent switches to
  its tab, or to the chief's pane. `l` flips between the graph and the log, `n`
  starts an agent, `r` brings a crew back, `x` retires one, `q` quits. None of
  them needs the `^a` prefix: no key in this tab belongs to an agent.
- **Each agent is a herdr tab** named after it, in the repository it works
  in. herdr's sidebar shows which ones are working and which are waiting on
  you — and, on each agent's second line where herdr would say `claude`, what
  it is on: `ENG-2553-2 · waits on ENG-2553-1`, `ENG-2553-2 · blocked: needs
  the flag`, `ENG-2553-2 · review`. The chief's line is the board at a
  glance: `3 open · 1 blocked · 1 in review`. The labels expire ten minutes
  after fleet stops, rather than going stale.
- **State is herdr's.** The fleet tab follows herdr's event stream, so an
  agent's state on the rail changes when herdr sees it change, and an agent
  stopped at a question or permission dialog shows as `! needs you` — which
  the session registry alone could not tell.
- **Notifications** when a task is blocked, ready for review, or done, through
  herdr's own (`[ui.toast] delivery`; herdr's default is off). Only new
  events: opening fleet does not replay yesterday's. An agent at a dialog is
  not announced twice — herdr signals that itself.
- **The chief** starts beside the fleet view the first time. It is briefed
  exactly as before and still runs without the editing tools, and
  `fleet spawn` run by the chief opens the new agent's tab in the same
  workspace. Quitting the view with `q` closes only its own pane while the
  chief is there; opening fleet again puts the view back on its left.
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
pane runs the view in that pane.

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

Start it in any terminal. tmux holds the agents; you never have to be inside
it. `↵` gives you the real pane and hands the screen back when you detach.

```bash
fleet                                       # the fleet view, scoped to $PWD
fleet spawn billing-svc --repo ~/Code/acme/service/billing-service
fleet sessions --watch
fleet repos --root ~/Code/acme           # what `n` offers
fleet tui --snapshot 104x22 --view graph    # one frame to stdout

fleet board epic ENG-2553 "shared settings flag"
fleet board add ENG-2553-1 ~/Code/acme/service/accounts-service "shared column" --epic ENG-2553
fleet board add ENG-2553-2 ~/Code/acme/service/billing-service "consume param" --epic ENG-2553 --dep ENG-2553-1
fleet board ready     # only ENG-2553-1 — the other one is waiting
fleet board ls
fleet board log
```

`FLEET_JSON=1` before any read gives JSON. `FLEET_DB` overrides the database
path, which is how the tests run against a scratch copy. `fleet board sql`
takes a SELECT and refuses anything else: every write goes through a verb that
records the matching flow event, and a bare UPDATE is the one way to break
that.

**Typing goes to the agent.** Click a pane to point the keyboard at it, and
everything you type reaches that session — arrows, Escape, Ctrl-C, its own
line editor. The wheel scrolls it. There is no mode to enter first.

**Nothing the UI thread does waits on tmux.** A tmux call is a process, about
11ms to start and hear back, and fleet used to make one per keystroke and four
per wheel notch on the thread that draws — then sleep up to 60ms after each key
waiting for the echo. A trackpad flick queued work faster than it drained; the
pane froze, then kept scrolling after the hand had stopped. Keystrokes, pastes
and the wheel now go to one sender thread, in order, which takes everything
waiting at once and merges what it can: a run of typed characters is one
`send-keys`, a flick is one call carrying every notch. The main loop drains
everything queued before drawing a single frame, and the mouse is asked for
clicks and drags only, not every movement of the pointer. Measured against a
real pane: fifty notches leave the UI thread in 0.2ms and arrive in 80ms;
forty-three keystrokes in 2.4ms and 48ms, in order.

**Keys keep their modifiers, and a paste stays a paste.** Every keystroke is
passed to tmux by name with every modifier on it — Option+Left is `M-Left`,
which tmux delivers as the same `ESC [1;3D` a terminal would — and Shift+Enter
goes as `M-Enter`, the `ESC CR` that Claude Code reads as a newline, because
tmux cannot hand a shifted Enter to a program that did not ask it for extended
keys. A paste arrives whole, over bracketed paste from the terminal and
`paste-buffer -p` into the pane, so its newlines stay newlines and Claude Code
shows it as a paste. Where the terminal speaks the kitty keyboard protocol,
fleet asks for it, which is the only way to tell Shift+Enter from Enter at all.

When a key does something in the terminal that it does not do in fleet,
`fleet keys` shows what the terminal sent for it and what fleet passes on.
The answer differs between terminals and between their settings, so it is
the first thing to run.

**The wheel goes where the program inside expects it.** Claude Code runs on
the alternate screen and captures the mouse itself, so there is no scrollback
for fleet to move through and the wheel is forwarded to it as a mouse report —
it scrolls its own history. A program on the alternate screen that did not ask
for the mouse gets arrow keys instead, which is what tmux sends in the same
situation. Only a plain pane, a shell or a log, is scrolled through fleet's own
view of it. Which case applies is asked of tmux rather than worked out from the
byte stream: the modes are set once at startup and fleet attaches to agents
that have been running for hours.

**Drag over an agent's output to copy it.** Capturing the mouse is what lets
a pane be clicked and scrolled, and it takes the terminal's own selection
away — there is no way to have both. So fleet does the selecting: drag, and
what was under it goes to the clipboard on release, by `pbcopy` and by OSC 52
so it also works over SSH. Anything that moves the text — a keystroke, a
scroll — drops the selection rather than leaving a highlight over a line that
has gone.

That covers the centre pane, which is an agent's own output. To select
anywhere else — the rails, the flow log — **`^a m`** hands the mouse back to
the terminal entirely; clicking and scrolling stop until you press it again,
and the key bar says so while it is off.

Fleet's own keys live behind **`Ctrl-A`**, the way a multiplexer's do: `^a n`
new agent, `^a x` take one off the rail, `^a r` bring back an earlier run,
`^a m` give the mouse back, `^a z`
fold the rails away, `^a g` graph, `^a l` log, `^a tab` move the keyboard,
`^a q` quit. `↵` hands you the real terminal until you
detach. `^a ^a` sends a literal Ctrl-A through. Where nothing is live to type
into — a transcript, the flow views — the keys act directly without it.

Starting fleet in a workspace starts a **chief of staff** there if one is not
already running. That is who you talk to; it plans the work and starts the
agents that do it. The rail lists only agents the fleet started, not every
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
pane opens. `--role chief` starts a chief instead — the other brief, the deny
list, and a row the rail draws as one. `--command` overrides the whole thing
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
moves between cards and `↵` opens one. The board is watched, so a message
appears as it is written rather than on the next tick.

The flow views read the `events` table, which every state change and every
`fleet board msg` writes. **A message sent with `SendMessage` and never logged
does not appear there** — the board is the record, so an agent that does not
report is invisible to it by construction.

`fleet board msg` is both halves at once: it writes the event *and* types the
message into the recipient's pane, so the interrupt and the record cannot come
apart. Delivery is best-effort and never costs the write — an agent that has
died still said what it said — and the CLI reports which happened:

```
accounts-svc -> chief: the parameter is yours
      delivered to chief in fleet:chief
```

It arrives as one line, marked, so the agent does not read a peer as the
person at the keyboard: `[fleet · accounts-svc · ENG-2553-2] the parameter is
yours`. A body is folded onto the same line, because `send-keys` types what it
is given and a newline would submit half a message; past ~1500 characters it
is a document and the delivery says so rather than pasting a page into
somebody's prompt.

## Picking up where you left off

A reboot, a closed terminal, `tmux kill-server`: the agents' processes go, and
their conversations do not. Claude Code keeps each one, and `claude --resume`
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
repository, back on the rail as itself. The role goes back into the system
prompt and the chief's editing tools stay withheld, because both are set at
launch and not kept with the conversation. `^a r` opens the same list at any
time; from a shell, `fleet resume` lists the runs and `fleet resume <id>`
brings one back.

Two things are deliberately left out of a resume. An agent taken off the rail
with `^a x` stays off — that was a choice, where a process dying is not, and
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
./dev.sh 140x40 graph    # a size and a view: session, graph, log
fleet preview --plain    # the same frame as text, for a diff
```

`fleet preview` invents the whole thing — an in-memory board carrying every
task state and every agent presence, and a canned Claude Code pane on a
**private tmux server** (`-L fleet-preview`). It draws one frame where your
prompt was, in colour, and gives the shell back. Nothing it does can reach
`~/.claude-fleet/fleet.db`, your registry, or your tmux server, so it is safe
to run beside a fleet that is up.

`dev.sh` polls for changes rather than needing `cargo-watch` or `fswatch`
installed, and builds debug — the release binary stays as it was, because
`~/.local/bin/fleet` is a symlink to it and somebody may have it open.

Worth knowing either way: **quitting fleet does not stop the agents.** tmux
owns them and the board is a file, so starting it again reattaches to
everything that is still running.

## Packaging plan

**Rust + ratatui, shipped as a Homebrew tap.** The shell CLI is a placeholder
for the same subcommands in the binary, so the skill never has to change.

Why Rust rather than Bun, given the rest of the stack is TypeScript:

- **Startup.** This is a tool you open dozens of times a day. A Rust binary is
  ready in single-digit milliseconds; a `bun build --compile` binary is ~60MB
  and takes tens of milliseconds before the first frame. That gap is the whole
  difference between a window you keep open and one you keep closed.
- **The workload is a steady parse.** Watching ~25 registry files, tailing
  several transcripts that are already hundreds of KB each, and querying
  SQLite — all while rendering. No GC pauses mid-frame.
- **The graph pane needs sub-cell drawing.** ratatui's `Canvas` widget renders
  braille, so the live topology is actually drawn rather than approximated with
  box characters.
- **Precedent.** [zoetrope](https://github.com/furkankly/zoetrope) is this
  exact shape — a 3.8MB arm64 binary distributed through a personal tap — and
  it already parses Claude Code transcripts, MIT-licensed.

Bun is the faster route to a prototype if staying in TypeScript matters more
than the above; OpenTUI exists and `bun build --compile` does produce a single
binary. The cost is startup, size, and hand-rolling the braille drawing.

### Intended layout

```
Cargo.toml
src/main.rs          clap — default subcommand is the TUI, `board` is the CLI
src/db.rs            fleet.db  [done]
src/registry.rs      ~/.claude/sessions watcher (notify / FSEvents)  [done]
src/transcript.rs    jsonl tail  [done]
src/tmux.rs          spawn a session into a pane, zoom to it  [done]
src/ui/…             fleet rail, session pane, board rail, flow [done]
schema.sql           embedded with include_str!
skills/board/        installed by `fleet install-skill`
```

Crates: `ratatui`, `crossterm`, `rusqlite` (bundled), `notify`, `serde_json`,
`tokio`, `clap`.

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

Layout mockups for the TUI live in the canvas at
the project's design canvas.

The frame: a left rail of **spawned agents only** (no idle repo list), a centre
pane showing whatever is selected — an agent's live session, or the flow — and
a right rail carrying the chief's tasks above and background processes below.

The centre pane shows two different things, and prefers the first:

1. **The agent's actual terminal**, mirrored out of tmux with `pipe-pane` and
   replayed through a vt100 parser. The real REPL — spinners, permission
   prompts, its own colours. tmux still owns the process, so an agent outlives
   this program and `↵` hands over the unmodified terminal. Typing goes
   straight to it; `Ctrl-A` is how you address fleet instead.
2. **The transcript**, re-rendered from the jsonl. The only thing that can show
   a session which is not in our tmux, or one that has ended, and the
   structured source the flow pane is built on. A reading of the session
   rather than the session, so it is the fallback.
