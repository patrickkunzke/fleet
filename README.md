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
| `src/db.rs` — fleet.db reads and writes | done, 11 tests |
| `fleet board` — the whole board, in the binary | done, 13 tests |
| `src/tmux.rs` — spawning and pane control | done, 8 tests |
| `src/ui/` — frame, fleet rail, session pane, board rail | done, 57 tests |
| `src/agent.rs` — starting an agent, repo discovery | done, 4 tests |
| `n` to spawn from the rail | done |
| `src/ui/flow.rs` — graph and log views | done, 18 tests |
| `src/ui/mirror.rs` — the live tmux pane | done, 6 tests |
| `src/ui/selection.rs` — drag to copy from a pane | done, 6 tests |
| `src/ui/clipboard.rs` — pbcopy and OSC 52 | done, 3 tests |
| `src/ui/preview.rs` — the fixture fleet, for layout work | done, 4 tests |

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
new agent, `^a x` take one off the rail, `^a m` give the mouse back, `^a z`
fold the rails away, `^a g` graph, `^a l` log, `^a tab` move the keyboard,
`^a q` quit. `↵` hands you the real terminal until you
detach. `^a ^a` sends a literal Ctrl-A through. Where nothing is live to type
into — a transcript, the flow views — the keys act directly without it.

Starting fleet in a workspace starts a **chief of staff** there if one is not
already running — an ordinary Claude Code session with the board skill. That
is who you talk to; it plans the work and starts the agents that do it. The
rail lists only agents the fleet started, not every session on the machine.

The flow views read the `events` table, which every state change and every
`fleet msg` writes. **A message sent with `SendMessage` and never logged does
not appear there** — the board is the record, so an agent that does not report
is invisible to it by construction. That is why the `/board` skill asks for
both: the board is the state, the message is the interrupt.

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
