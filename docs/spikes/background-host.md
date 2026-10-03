# Spike: fleet without herdr, on Claude Code background sessions

2026-10-03, Claude Code 2.1.288, fleet 0.9.1.

Can fleet run its crew as Claude Code background sessions (`claude --bg`,
`claude agents`) instead of herdr tabs? Each question below was answered with
a real background worker on a scratch board, started with fleet's own launch
flags and plugin (`/board` skill and mod).

## Answers

| Question | Result |
| --- | --- |
| Does `--append-system-prompt` reach a background session? | Yes. The worker recited a codeword that only its role held, and still did after a stop and a wake. |
| Does `--allowed-tools 'Bash(fleet board:*)'` work? | Yes. `fleet board start` ran without a prompt; an `echo` outside the list stopped at one. |
| Does `--plugin-dir` load the skill and the mod? | Yes. `/plugin` shows `1 mod active · fleet`; the mod checked in, reported `Bash · 27%`, and showed "waiting for a go on ENG-1-1" under the prompt. |
| Do `FLEET_DB` and `PATH` reach it? | Yes, from the shell that runs `claude --bg`. |
| Does board message delivery work with no terminal attached? | Yes. `fleet board msg` queued, the mod submitted it once the session was idle, and the worker answered. |
| Can fleet read status, including "stuck at a dialog"? | Yes, and better than herdr: `claude agents --json` gives `status` (`busy`/`waiting`/`idle`), `state` (`working`/`blocked`/`done`/…) and `waitingFor` (`permission prompt`, `input needed`, …). |
| Can you talk to a worker? | `claude attach <id>` (or Enter in `claude agents`) opens the full session; Space in agent view peeks and replies without attaching. |
| Stop and resume? | `claude stop <id>`. `claude --resume <session> --bg` with **no flags** wakes it with its saved options: name, plugin, role, allowed tools, model, permission mode and folder. |

## Things that change the design

1. **`--session-id` is ignored with `--bg`.** The session picks its own id and
   prints `backgrounded · <short id> · <name>`. fleet has to parse that, or
   find the session by name in `claude agents --json`, to link it on the board.
2. **`--resume <id> --bg` with flags starts a copy** under a new id ("keeps its
   own saved options, so the flags you passed started a copy"). A crew resume
   must wake with no flags, which is simpler than today's resume line.
3. **A woken session takes its process environment from the shell that wakes
   it.** Woken from a shell without `FLEET_DB`, the mod stayed silent, while
   its Bash tool still had the variable from its saved shell snapshot. fleet
   must wake sessions with `FLEET_DB` and `PATH` set, as it starts them.
4. **Untrusted folders are refused, with exit code 0** ("Workspace not
   trusted. Run `claude` in … once"). fleet has to read the output, not the
   exit code. Trust carries down to subfolders, so trusting the workspace
   root once, which opening the chief there does, covers every repo in it.
5. **`claude logs` is raw terminal output**, not text. The transcript
   (`~/.claude/projects/*/<session>.jsonl`) stays the place to read what an
   agent said, as `fleet handoff` already does.

## What herdr still does that this does not

- Each agent's terminal side by side with the graph, in one layout. Agent view
  is a list you attach to.
- Agent view is a research preview. `claude agents --json` is its documented
  stable interface; the files under `~/.claude/jobs/` are not.

## Suggested build, in this order

1. **A background host** beside the herdr one: `Host::open` runs `claude --bg`
   in the repo and parses the id; status from `claude agents --json`; close is
   `claude stop`; focus prints `claude attach <id>`; resume wakes without
   flags. `spawn`, `handoff`, `retire` and resume then work in any terminal.
2. **The chief's view as a mod**: a `/fleet` pane with the crew and the board,
   a band above the chief's prompt, and toasts for what herdr notifies today.
   A simpler graph than the Rust one to start: cards, no animated lines.
3. **Packaging** as a Claude Code plugin in a marketplace, with the `fleet`
   binary alongside, since the board needs SQLite and mods have none.
