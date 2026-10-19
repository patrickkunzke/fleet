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

Updated after the probe (see "Probe results" below).

Build (b) as a host that the user turns on, `FLEET_HOST=desktop`. Keep
`background` as the default outside herdr. The chief runs in auto mode, and
in auto mode only the model's own call to `start_session` gets through. So
build `/fleet spawn` as a prompt command (a markdown command in the plugin)
that the model carries out:

1. The user types `/fleet spawn <name>` (or `/fleet spawn` for every pending
   agent) in the desktop chief. The command expands to a prompt that tells
   the model to call `start_session` with `initiation: "user_asked"`, `cwd`
   (the worker's repo, which can be any repo, not the chief's), `title`,
   `name`, `prompt` and `background`. The arguments come from `fleet board
   spawn --host desktop`, which registers the agent and prints them. The
   model only copies them. The probe did this in auto mode, with no dialog
   and no change of mode.
2. The worker's brief cannot start with a slash command: the app wraps the
   prompt. So the mod registers a tool, `mcp__fleet__join`, and the brief
   tells the model to call it first with the run and the agent's name. The
   tool does what `/fleet join` was meant to do: it records the worker's CLI
   session id (`$.session.id()`), sets `FLEET_DB` and `FLEET_RUN`, and starts
   the check-in. A `prompt.submit` hook that finds the join line in the
   wrapped brief could do the same without the model. That is not tested.
3. Store two ids on the board: the desktop id from the `start_session`
   result (`local_…`, for `list_sessions`, `stop_session` and
   `archive_session`) and the CLI session id from the join (for
   `$.session.send`). They are different ids.
4. Chief-to-worker messages go through `$.session.send({ to: { sessionId:
   <CLI id> } })`. The model's SendMessage to the `local_…` id also works.
5. A start that the chief wants on a turn the user did not start (a worker
   reported "done") is not possible, not even as a card, because a card
   cannot carry `cwd`. In that case the chief tells the user to run
   `/fleet spawn <name>`. Add this to the chief's brief.

Two things to settle while building:

- **Auto mode.** A direct call from the mod (`$.mcp.call` or `$.tool.call`)
  is refused in auto mode, because the classifier has no request to judge.
  It works only in a chief that is not in auto mode. Do not make the mod
  approve its own call in a `tool.check` hook to get past the classifier.
  That is a bypass of auto mode, and the classifier refused even to write
  that probe. A `/fleet spawn` that is a hooks command cannot hand the work
  to the model either: `$.prompt.submit` from a `command.run` hook is
  refused.
- **Where the mod comes from.** A worker loads fleet's mod only from an
  installed plugin or from the repo's project folder. The `start_session`
  session gets a new worktree, and untracked files in the repo do not go into
  it, but the probe mod in `<repo>/.claude/skills/` still loaded there. The
  installed plugin (PR #25) is the safe source.

Until then, add the Import line to the README, so that the user can at least
read retired workers' conversations in the sidebar.

## Probe results

2026-10-19, Claude Code 2.1.299, Claude desktop app (Code tab). A desktop
session in this repo was the chief. A throwaway mod, `fleet-probe`, ran in it
from the session's dev-mods folder (hot reload). A copy of the mod was in
`~/fleet-probe/a/.claude/skills/` and `~/fleet-probe/b/.claude/skills/`, two
scratch git repos. The mod registered `/probe-join`, `/probe-spawn`,
`/probe-send` and a tool `mcp__fleet-probe__join`, and logged every event to
a file. The real fleet plugin was not used.

| Question | Result |
| --- | --- |
| 1. Does `/probe-join w1` as the `start_session` prompt run as a command? | **No. It arrives as plain text, and this is not a race.** The mod loaded in the new session 250 ms before the first prompt, and its tool was there on the first turn. But the app wraps the prompt: the first user message is `<system-reminder>`s (linked sessions, worktree), then `Background (from the session that started this one):`, the background text, `Task:`, the prompt, and a line about the shared notes folder. `prompt.submit` sees `origin: sdk` and no `command`. A slash command can never be first. The fallback works: the brief told the model to call `mcp__fleet-probe__join` with `w1`, and the model did. |
| 1. Is the session in the sidebar under the chief? | **Yes.** `get_session` gives `parentSessionId` = the chief, `detached: false`. The app gave it its own worktree, `<repo>/.claude/worktrees/<name>` on branch `claude/<name>`, cut from `master`, because the chief itself runs in a worktree. Its `local_` id is not its CLI session id (`local_ceea9ccc…` against `1e069bd0…`), unlike imported sessions. |
| 1. What does the user have to do? | **Trust each repo once.** The first start in an untrusted repo returns `Nothing was started. Before this can work, the user needs to trust the repository …`, and a Trust repository button shows in the chief. After the click, the same call starts the session. |
| 2. Can the mod call `start_session`? | **Yes, both ways, from a command the user types, outside auto mode.** `$.mcp.call('ccd_session', 'start_session', args)` returns `{ content: [{ type: 'text', text: 'Started session "…" (session_id: local_…, name: …) …' }], isError: false }`. `$.tool.call({ tool: 'mcp__ccd_session__start_session', ...args })` returns `{ result: [...], text: 'Started session …' }`. No permission dialog showed for either. The new session is nested under the chief, gets a worktree, loads the mod and joins. It inherits the chief's permission mode. |
| 2. And in auto mode? | **Refused, both ways.** `$.mcp.call` throws and `$.tool.call` returns `{ deny }`, each with: `The server-side auto mode classifier gave no verdict for mcp__ccd_session__start_session: the request that produced this action did not ask for one. …`. The call does not reach the app. |
| 2. `initiation` from the mod | `user_asked` with `cwd`: **started** (after trust). `own_initiative` without `cwd`: **card** (`Proposed "…" (task_id: task_…). The user sees a card and nothing runs until they start it …`). When the user clicked it, the session started in the chief's project, not in a worker's repo. `offer` (title only): **the card asks the user, and the call waits**. "Do it here" returns `The user chose "Do it here" …`. "New session" returns `The user chose "New session" — the session is already opening … call start_session once more with exactly proposal_id …, prompt … and background …`. The mod has to make that second call, or the opened session fails without a brief. |
| 3. A start on a turn that another session's message started | **Refused, and no card is possible for a worker's repo.** `user_asked` with `cwd`: `Start sessions only on the user's own words to you here; this turn runs an automated event, another session's message, or the task another session gave you. Propose the session instead (initiation "own_initiative"), or start it once the user asks you here.` `own_initiative` with `cwd`: `A proposal card cannot carry a host, folder, environment or preset yet: propose it for this project (drop those arguments), or start it there when the user asks you to (initiation "user_asked").` `offer`: `No card was shown: this turn's request did not come from someone at this app's composer, so there is nobody to ask.` So the chief has to tell the user to start the worker, for example "w3 is ready to start in `<repo>`: run `/fleet spawn w3`." |
| 4. `$.session.send` from the chief's mod | **Only by the worker's CLI session id.** With `{ sessionId: 'local_…' }`: `{ isDelivered: false, reason: 'no live session on this machine has id local_… (a session that exited, or another machine's; …)' }`. With the CLI id: `{ isDelivered: true }`, and it starts a turn in the idle worker. The worker sees `Another Claude session sent a message:` and `<cross-session-message from="uds:…sock" from-session="local_<chief>" from-name="<chief title>" from-mode="prompting" from-plugin="fleet-probe">`, with `origin.kind: peer` and `origin.plugin: fleet-probe` in the transcript, then a note to treat it as a teammate's request. For comparison, the model's SendMessage to the `local_` id goes through "Claude Desktop's session messaging" and arrives as `<cross-session-message from="local_<chief>" name="<chief title>">`. |

### Auto mode follow-up

The user's chief always runs in auto mode, so a second probe looked for a way
to start workers without leaving it. The chief session was in auto mode.

| Variant | Result |
| --- | --- |
| B. A markdown prompt command in the mod (`commands/probe-ask.md`, typed as `/fleet-probe:probe-ask`). It tells the model to call `start_session` with `user_asked` and `cwd` = repo b. | **Works.** `prompt.submit` shows `origin: composer` and the command. The model's call goes through the classifier with no dialog. The worker starts nested under the chief, in its own worktree of b, loads the mod and joins six seconds later. |
| C. A hooks command (`/probe-submit`) that calls `$.prompt.submit` with the same request. | **Refused by the host:** `prompt.submit: called from a command.run hook, it would wait on the turn this hook is holding; submit from a later event (turn.complete) (host check)`. A submit from a later event was not tried. Its origin would be `plugin`, not the user, and the app may then refuse the start. |
| A. The mod calls `start_session` itself, and its own `tool.check` hook answers `allow`. | **Not built.** The auto mode classifier refused to write it, as an "Auto-Mode Bypass". That judgment is right: this design should not get past the classifier. |

Not tested: a `prompt.submit`
hook that runs the join from the wrapped brief; `$.session.send` to a worker
that is not idle.
