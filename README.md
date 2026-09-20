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
| `cli/board.sh` — the `fleet board` contract | done, shell implementation |
| `skills/board` — the `/board` skill | done |
| `src/registry.rs` — session discovery and watching | done, 6 tests |
| `src/transcript.rs` — transcript reading and tailing | done, 8 tests |
| `src/db.rs` — fleet.db reads and writes | done, 11 tests |
| `fleet board` reads in the binary | done; writes still cli/board.sh |
| `src/tmux.rs` — spawning and pane control | done, 8 tests |
| `src/ui/` — frame, fleet rail, session pane | done, 34 tests; the right rail is still a stub |
| `src/ui/mirror.rs` — the live tmux pane | done, 5 tests |
| TUI panes | not started |
| `board` subcommand in the binary | not started |

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

Symlinks `cli/board.sh` to `~/.local/bin/fleet`, symlinks `skills/board` into
`~/.claude/skills/`, and creates the database. Both symlinks point back here,
so edits take effect immediately.

## Use

```bash
fleet help
fleet epic ENG-2553 "shared settings flag"
fleet add ENG-2553-1 ~/Code/acme/service/accounts-service "shared column" --epic ENG-2553
fleet add ENG-2553-2 ~/Code/acme/service/billing-service "consume param" --epic ENG-2553 --dep ENG-2553-1
fleet ready          # only ENG-2553-1 — the other one is waiting
fleet ls
fleet log
```

`FLEET_JSON=1` before any read gives JSON. `FLEET_DB` overrides the database
path, which is how the tests run against a scratch copy.

The Rust binary carries the same reads plus the view itself:

```bash
cargo run -- tui --root ~/Code/acme      # the fleet view
cargo run -- spawn billing-svc --repo ~/Code/acme/service/billing-service
cargo run -- sessions --watch
cargo run -- tui --snapshot 104x20          # one frame to stdout
```

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
src/ui/…             fleet rail [done], session pane [done], flow, board rail
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
   this program and `↵` hands over the unmodified terminal.
2. **The transcript**, re-rendered from the jsonl. The only thing that can show
   a session which is not in our tmux, or one that has ended, and the
   structured source the flow pane is built on. A reading of the session
   rather than the session, so it is the fallback.
