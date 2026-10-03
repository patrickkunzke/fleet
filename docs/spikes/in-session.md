# Spike: starting fleet from inside an ordinary Claude Code session

2026-10-03, Claude Code 2.1.288, fleet 0.13.0, on a Team organization.

The goal: start `claude` as always, then `/fleet start` turns that session
into the chief, with fleet installed as an ordinary Claude Code plugin and
dormant until then. A probe plugin did mid-session what `/fleet start` would,
in an interactive Haiku session in manual permission mode.

## Answers

| Question | Result |
| --- | --- |
| Can a mod register a tool mid-session, from a command? | Yes. Registered from `/probe-start`, called on the next turn (Claude loaded it through ToolSearch). |
| Can a mod approve its own tool, so it needs no prompt? | Yes. A `tool.check` hook answering `{ decision: 'allow' }` for `mcp__probe__probe_tool` let it run with no prompt in manual mode. |
| Does `$.env.set` reach the session's processes mid-session? | Yes, both. A `$.process.run` child and the Bash tool printed `PROBE=yes-from-mod`. |
| Can a mod add the chief's role to the system prompt mid-session? | **No, not on a Team organization.** The debug log says `probe: prompt.compose bypassed by cc-plugin-sec-default (tier user)`, and the same for `prompt.context`. The built-in guard keeps user-installed mods out of the system prompt and context. |
| Installed `fleet` plugin, and a worker started with `--plugin-dir` for its own copy? | Fine. `Plugin "dupe" from --plugin-dir overrides installed version`. Workers keep getting the copy that matches the `fleet` binary. |

## What this changes in the design

- **The chief's role arrives as a prompt, not a system prompt.** `/fleet
  start` submits the chief's brief with `$.prompt.submit`, as fleet already
  delivers messages. The rule that matters most, that the chief does not
  edit, does not depend on it: the gate enforces it in code once the board
  says the session is the chief.
- **Compaction is the risk to the role.** A submitted brief is conversation,
  and compaction summarises conversation. `session.compact` lets a mod rewrite
  the compaction instructions, and it is not among the events the guard
  intercepts, so the mod can ask for the brief to be kept. Not yet tested.
- **Workers are unchanged:** started by fleet with `--plugin-dir` and
  `--append-system-prompt` as today, since they are new sessions.

## Suggested build

1. `fleet chief --adopt <session> --root <dir>`: record an existing session
   as the chief, in a new run, and print the board path. No exec.
2. In the mod, `/fleet start`: run that, `$.env.set` `FLEET_DB`, `FLEET_RUN`
   and fleet's folder on `PATH`, register the board tools, start the
   check-in and the view, and submit the chief's brief. A `tool.check` hook
   allows `mcp__fleet__*` and `fleet board`, in fleet sessions only.
3. The `session.compact` hook keeping the chief's brief through compaction.
4. The repo as a Claude Code marketplace, with `mod/` as the whole plugin,
   skill included, and the mod fetching the `fleet` release binary into its
   plugin folder (`$.plugin.root`) the first time it is needed.
