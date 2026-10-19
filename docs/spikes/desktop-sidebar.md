# Spike: fleet's workers in the Claude desktop app's sidebar

2026-10-17, Claude Code 2.1.299, Claude desktop app (Code tab), fleet 0.17.2.

The goal: each worker the chief starts shows as a session in the desktop
sidebar, like a session started from another session, so the user can open
it and talk to it with a click. Today the user can open a worker that runs
outside herdr only with `claude attach <id>`, because fleet starts it as a
`claude --bg` background session (`src/background.rs`).

This spike only reads. It changes nothing in fleet. It looks at the desktop
app's session store and log, the jobshop board, and the mod API types that
2.1.299 writes for `plugin-authoring`.

## Answers

| Question | Result |
| --- | --- |
| How did the jobshop workers of 2026-10-08 get into the sidebar? | **The desktop app imported them on 2026-10-15, after they had ended.** They were never desktop sessions. On the board, every jobshop agent has a `herdr:` target (`ncf-cut` is `herdr:jobshop-ncf-cut`, and the chief is `herdr:jobshop-chief`). The desktop's record for each one, under `~/Library/Application Support/Claude/claude-code-sessions/<account>/<org>/local_<cli id>.json`, has `"adoptedFromOtherSurface": true`. Its id is `local_` plus the CLI session id, which is the same id as `agents.session_id`. All 159 records with that flag have the same `indexedAt`: 2026-10-15 13:20. |
| What imported them? | **A manual Import in the desktop app** (`CCD-SessionRecovery`). The log says `Invoked` at 13:19:55, then `Recovered CLI session <id> as local_<id>` 159 times. The app scans `~/.claude/projects` regularly (`Probe: 161 importable CLI transcript(s)`) and offers the import. It does not run the import by itself. |
| Does the import pick up live workers? | **No.** It skipped the two transcripts that were in use: `Skipping candidate 363f661e…: a running Claude Code process owns this transcript; re-run Import after it exits`. `363f661e` is the tripgear worker `product-link`, which ran in herdr at that time. That is why tripgear's workers are not in the sidebar and jobshop's are. |
| Do `claude --bg` workers get into the sidebar? | Not by themselves. `get_session local_b9ff8e37…` (fleet-try `api`, `bg:b9ff8e37`) says `not found`. Its transcript has the same `entrypoint: cli` as the imported ones, so an Import after `claude stop` will probably pick it up. Not tested. |
| Is an imported worker live? | **No. It is history.** The desktop record holds the title, cwd and model, and points at the CLI transcript. If the user opens it and writes, the desktop resumes it with its own `claude` process. That process has no `--append-system-prompt`, no `--plugin-dir` and no `FLEET_DB`, so it is not a fleet worker anymore. |
| Can a mod start a session, or add one to the sidebar? | **Not directly.** The mod API (`$`) has `plugin, ui, model, audio, mcp, session, turn, prompt, tool, command, config, telemetry, agent, fs, store, state, clock, http, process, settings, env`. `$.agent.spawn` starts a subagent in the same process. `$.process.spawn` runs a program. `$.desktop` is only the desktop's table of elements (`Box`, `Text`, … `Client`). No noun creates a session, lists the sidebar or writes to it. |
| Can a mod call the desktop's own session tools? | **Maybe. This needs a test.** `$.mcp.call(server, tool, args)` calls a tool on any MCP server that is connected, "with the engine's own connection", and asks for no permission. `$.tool.call` runs any tool the model can call. In a desktop session, `start_session` is the tool `mcp__ccd_session__start_session`. It is not known if the desktop's `ccd_*` servers answer `$.mcp.call`. It is also not known if the app applies its rules about who started a session (see below) to a call that comes from a plugin. |
| Can a mod message a desktop session? | Yes, by the types. `$.session.send({ to: { sessionId }, text })` is the SendMessage tool's own delivery. The receiver sees it as a message from a peer, with `origin.plugin` set to fleet. |

## The options

### (a) Import the workers' transcripts

The Import is what put the jobshop workers in the sidebar. It is a manual
click. It skips sessions that are still running, and the imported session
stops being a fleet worker as soon as the user writes to it. So it gives the
user a browsable history of retired workers, not workers to talk to. fleet
cannot start the Import. The only other way is for fleet to write records into
the desktop's private store under `Application Support`, which is not a
documented interface. The app can overwrite or ignore those records, and they
would still not be live. **Rejected as a host.** It is worth one line in the
README: "after `fleet retire`, Import in the desktop app shows the workers'
conversations in the sidebar."

### (b) `FLEET_HOST=desktop`: the chief starts workers with `start_session`

A desktop chief already has `mcp__ccd_session__start_session`. A session it
starts gets what the user asked for. It shows in the sidebar, nested under the
chief. The app keeps a "Linked sessions now" note in the chief with each
worker's name, id and state. When a worker ends a turn, the app sends the
chief a note that quotes the first 400 characters of the worker's report. The
chief and the user can reach a worker with `SendMessage` by its `local_…` id.
`list_sessions {"linked": true}` gives `isRunning`, and `stop_session` and
`archive_session` give `retire`.

What `start_session` does not carry, compared with `claude --bg`:

- **No `--append-system-prompt`.** The worker's role has to come in the
  `prompt`. This is the same limit that `/fleet start` has in
  [in-session.md](in-session.md), with the same risk from compaction.
- **No `--plugin-dir`.** The worker gets fleet's mod only if fleet is
  installed as a plugin. PR #25 makes it one.
- **No environment.** `FLEET_DB` and `FLEET_RUN` do not reach the worker. A
  `preset` brings "its saved environment variables", but one preset for each
  run is too heavy. The mod has to set them with `$.env.set`, which works
  mid-session (in-session.md).
- **No `--allowed-tools`.** `permission_mode` is "this session's own or a
  lower one". The mod's `tool.check` hook already allows `fleet board` and
  `mcp__fleet__*` in fleet sessions.
- **The app decides who may start a session.** `initiation: "user_asked"`
  starts a session at once, but only when the user asked for it in their own
  words. `cwd` (the worker's repo) is allowed only with `user_asked`, and the
  app can ask the user to trust the folder. On a turn that another session's
  message or an automated event started, the app refuses a start and allows
  only an `own_initiative` card, which the user has to click. A chief that
  spawns because a worker reported "done" is on such a turn.
- **Only the model can call it.** The tool belongs to the chief's model, so
  the `fleet` binary (Rust) cannot call it. `fleet spawn` would have to
  register the agent on the board and print what the chief must pass to the
  tool. Option (c) could remove this limit.

With these limits, the worker joins the board itself. A new `/fleet join
<run> <name>` command in the mod does for a worker what `/fleet start` does
for the chief. It reads the pending agent row and records the session id
(`$.session.id()`). It sets `FLEET_DB`, `FLEET_RUN` and fleet's folder on
`PATH`. It starts the check-in and submits the worker's brief. The prompt that
the chief gives `start_session` is then only that command, or a short brief
that starts with it. Not tested: whether a `prompt` that starts with `/fleet`
runs as a slash command in a session that `start_session` started.

The board target is `desktop:local_<id>`. Status comes from the mod's
check-ins and `list_sessions`, not from `claude agents`. Messages go through
`$.session.send` or the mailbox, as they do today.

### (c) The mod API

There is no session or sidebar noun in 2.1.299. Two calls could turn (b) from
a request to the model into code:

- `$.mcp.call("ccd_session", "start_session", {...})` (or `$.tool.call` with
  `mcp__ccd_session__start_session`) from a `/fleet spawn` command or the board
  tool's hook. Then the chief's model does not have to copy arguments. If the
  app treats the call as `user_asked` because the user typed the command, the
  click for each worker also goes away.
- `$.session.send({ to: { sessionId } })` for chief-to-worker messages,
  instead of the mailbox and `$.prompt.submit`.

Neither one is tested. If the app refuses `ccd_*` calls from a plugin, (b) still
works through the model.

## Recommendation

Build (b) as a host that the user turns on, `FLEET_HOST=desktop`. Keep
`background` as the default outside herdr. Before building, run a small probe
in a scratch fleet from a desktop chief. It needs about one hour:

1. The chief calls `start_session` with `cwd` set to a repo and the prompt
   `/fleet join <run> <name>`. Check that the slash command runs, that the
   installed mod loads and checks in, and that the worker is in the sidebar
   under the chief.
2. Call the same tool from the mod with `$.mcp.call`, and then with
   `$.tool.call`, from a `/fleet spawn` command. Note what the app does with
   `initiation`. Does it start the session, show a card, or refuse?
3. Start a worker on a turn that a worker's message started. Check that it
   becomes a card, and note what the chief has to tell the user.
4. `$.session.send` from the chief's mod to the worker's `local_` id.

If step 2 works, `fleet spawn` in a desktop chief becomes a mod command, and
the model does not have to do anything. If step 2 does not work, the chief's
brief tells it to call `start_session` itself after `fleet board spawn
--host desktop` registers the agent.

Until then, add the Import line to the README, so that the user can at least
read retired workers' conversations in the sidebar.
