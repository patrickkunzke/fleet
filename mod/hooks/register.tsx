// fleet's mod: messages from the board, delivered by the session itself.
//
// Without it, `fleet board msg` types a message into the recipient's herdr
// pane: one line, cut at 1500 characters, and typed whether or not the agent
// is in the middle of a turn. With it, this session checks its mailbox on the
// board every few seconds and, once it is idle, takes what is waiting and
// submits it as a prompt of its own, whole.
//
// The check is also how the board knows the mod is here: a mailbox that
// stops checking in goes quiet after fifteen seconds, and fleet goes back to
// typing. So a session without the mod, or one whose mod has died, still
// gets its messages.
//
// It also holds the go-ahead. A worker does not edit until its task has one:
// `fleet board go` from the chief, or a prompt the person types in the
// worker's own pane. The chief does not edit at all, Bash included. Both are
// a guard against an agent drifting into work, not a sandbox: a determined
// one can write a file in ways no pattern here knows. When the board cannot
// be asked, the edit goes ahead, as it would without the mod.
//
// It gives the session the board as tools: `mcp__fleet__board_start`,
// `..._msg`, `..._go` and the rest, each running the `fleet board` command of
// the same name. A tool has a schema to fill in where a shell line has
// quoting to get wrong, and nothing to mistake for a skill.
//
// And in the chief's session it draws the fleet. In herdr the fleet tab sits
// beside the chief; outside herdr there is no tab, so `/fleet` opens a pane
// with the crew, the open tasks and the latest messages, a band above the
// prompt says who needs something, and toasts say what herdr would have
// notified. It reads `fleet board snapshot` every few seconds; a worker's
// session stops reading once the board says it is one.

import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register } from 'claude-code'

import type { GraphSpan, Snapshot } from '../types'

/** How often the mailbox is checked. Well inside the board's fifteen. */
const EVERY = 3_000

/** What `fleet board inbox` prints. */
type Inbox = {
  agent: string | null
  text: string | null
  waiting: number
  /** The task this worker cannot change files for until it has a go. */
  awaiting_go?: string | null
}

/** What `fleet board gate` prints. */
type Gate = {
  agent: string | null
  role: string | null
  tasks: string[]
  approved: boolean
}

/** The tools that change files. */
const EDITS = new Set(['Edit', 'Write', 'NotebookEdit', 'MultiEdit'])

/** Where writing is fine even for an agent that may not edit. */
const SCRATCH = String.raw`(?:/dev/|/tmp/|/private/tmp/|\$TMPDIR|"\$TMPDIR)`

/** A command at the start of a pipeline, a list or a substitution. */
const AT = String.raw`(?:^|[;&|(]\s*|\$\(\s*|\bxargs\s+)`

/**
 * Whether a shell command writes files outside scratch space, as far as a
 * pattern can tell. Quoted text is blanked first, so a message that says
 * "a -> b" is not a redirect.
 */
export function writes(command: string): boolean {
  const bare = command.replace(/'[^']*'/g, "''").replace(/"(?:[^"\\]|\\.)*"/g, '""')
  const rules = [
    // > and >> into a file, not 2>&1 and not into /dev/null or /tmp
    new RegExp(String.raw`(?<![0-9&])>>?\s*(?!&|\s*${SCRATCH})[^\s&|;)]`),
    new RegExp(String.raw`[0-9]>>?\s*(?!&|\s*${SCRATCH})[^\s&|;)]`),
    new RegExp(String.raw`${AT}(?:sed|gsed)\s+(?:[^|;&]*\s)?(?:-[a-zA-Z]*i|--in-place)`),
    new RegExp(String.raw`${AT}perl\s+(?:-[a-zA-Z]*\s+)*-[a-zA-Z]*i`),
    new RegExp(String.raw`${AT}tee\s+(?!(?:-a\s+)?${SCRATCH})`),
    new RegExp(String.raw`${AT}(?:cp|mv|rm|touch|mkdir|rmdir|ln|install|truncate|patch|dd)\b(?![^;&|]*\s${SCRATCH}\S*\s*$)`),
    new RegExp(String.raw`${AT}git\s+(?:-C\s+\S+\s+)?(?:apply|am|checkout|switch|restore|reset|stash|commit|merge|rebase|cherry-pick|revert|mv|rm|clean|pull)\b`),
  ]
  return rules.some(rule => rule.test(bare))
}

/** One of the board's commands, as a tool the model calls. */
type BoardTool = {
  name: string
  description: string
  properties: Record<string, unknown>
  required: string[]
  /** Only the chief plans, dispatches and gives the go. */
  isChiefs?: true
  /** The `fleet` command line, from the tool's input and who calls it. */
  argv: (input: Record<string, unknown>, me: string) => string[]
}

const str = (description: string) => ({ type: 'string', description })
const TASK = str('The task key, such as ENG-2553-1')

/** `--flag value` when the value was given. */
function opt(flag: string, value: unknown): string[] {
  return typeof value === 'string' && value !== '' ? [flag, value] : []
}

const BOARD_TOOLS: BoardTool[] = [
  {
    name: 'board_ls',
    description: 'Every task on the board, with its state, its agent and what it waits on.',
    properties: { state: str('Only tasks in this state: queued, running, blocked, review, done, dropped'), epic: str('Only this epic'), repo: str('Only tasks whose repository path contains this') },
    required: [],
    argv: i => ['board', 'ls', ...opt('--state', i.state), ...opt('--epic', i.epic), ...opt('--repo', i.repo)],
  },
  {
    name: 'board_show',
    description: 'One task: its brief, and everything that happened to it.',
    properties: { task: TASK },
    required: ['task'],
    argv: i => ['board', 'show', String(i.task)],
  },
  {
    name: 'board_ready',
    description: 'Queued tasks whose dependencies are all done: what can be dispatched now.',
    properties: {},
    required: [],
    argv: () => ['board', 'ready'],
  },
  {
    name: 'board_start',
    description: 'Mark a task running: you have picked it up.',
    properties: { task: TASK },
    required: ['task'],
    argv: i => ['board', 'start', String(i.task)],
  },
  {
    name: 'board_block',
    description: 'Mark a task blocked, with why. Tell the chief as well.',
    properties: { task: TASK, reason: str('What it is waiting for') },
    required: ['task', 'reason'],
    argv: i => ['board', 'block', String(i.task), String(i.reason)],
  },
  {
    name: 'board_unblock',
    description: 'Carry on with a blocked task.',
    properties: { task: TASK },
    required: ['task'],
    argv: i => ['board', 'unblock', String(i.task)],
  },
  {
    name: 'board_review',
    description: 'Open a task for review rather than finished.',
    properties: { task: TASK, mr: str('The merge or pull request URL') },
    required: ['task'],
    argv: i => ['board', 'review', String(i.task), ...opt('--mr', i.mr)],
  },
  {
    name: 'board_done',
    description: 'Finish a task. Says which tasks that freed.',
    properties: { task: TASK, mr: str('The merge or pull request URL') },
    required: ['task'],
    argv: i => ['board', 'done', String(i.task), ...opt('--mr', i.mr)],
  },
  {
    name: 'board_msg',
    description: 'Send another agent a message, recorded on the board and delivered to them. You are the sender.',
    properties: { to: str('The agent, by its board name: chief, or a worker'), summary: str('One line'), body: str('The rest, if there is more'), task: TASK },
    required: ['to', 'summary'],
    argv: (i, me) => ['board', 'msg', me, String(i.to), String(i.summary), ...opt('--body', i.body), ...opt('--task', i.task)],
  },
  {
    name: 'board_note',
    description: 'Record something on the board that is not a message to anyone.',
    properties: { summary: str('One line'), body: str('The rest'), task: TASK },
    required: ['summary'],
    argv: i => ['board', 'note', String(i.summary), ...opt('--body', i.body), ...opt('--task', i.task)],
  },
  {
    name: 'board_add',
    description: 'Queue a task for a repository.',
    properties: {
      task: TASK,
      repo: str('The repository, as an absolute path'),
      title: str('A short title'),
      body: str('What a fresh agent in that repository needs in order to start'),
      epic: str('The epic it belongs to'),
      deps: { type: 'array', items: { type: 'string' }, description: 'Tasks it waits on' },
    },
    required: ['task', 'repo', 'title'],
    isChiefs: true,
    argv: i => [
      'board', 'add', String(i.task), String(i.repo), String(i.title),
      ...opt('--epic', i.epic), ...opt('--body', i.body),
      ...(Array.isArray(i.deps) ? i.deps.flatMap(d => ['--dep', String(d)]) : []),
    ],
  },
  {
    name: 'board_dep',
    description: 'Record that one task waits on another.',
    properties: { task: TASK, depends_on: str('The task it waits on') },
    required: ['task', 'depends_on'],
    isChiefs: true,
    argv: i => ['board', 'dep', String(i.task), String(i.depends_on)],
  },
  {
    name: 'board_epic',
    description: 'Create or retitle an epic.',
    properties: { key: str('The epic key, such as ENG-2553'), title: str('Its title') },
    required: ['key', 'title'],
    isChiefs: true,
    argv: i => ['board', 'epic', String(i.key), String(i.title)],
  },
  {
    name: 'board_claim',
    description: 'Assign a task to an agent.',
    properties: { task: TASK, agent: str('The agent, by its board name') },
    required: ['task', 'agent'],
    isChiefs: true,
    argv: i => ['board', 'claim', String(i.task), String(i.agent)],
  },
  {
    name: 'board_go',
    description: "Give a worker the go-ahead on its task, and tell it. Until a task has one, its worker cannot change files. Anything short of a go is board_msg.",
    properties: { task: TASK, message: str('What to send with it; "go" when not given'), body: str('More, if there is more') },
    required: ['task'],
    isChiefs: true,
    argv: i => ['board', 'go', String(i.task), ...(typeof i.message === 'string' && i.message ? [i.message] : []), ...opt('--body', i.body)],
  },
  {
    name: 'board_drop',
    description: 'Abandon a task.',
    properties: { task: TASK, reason: str('Why') },
    required: ['task'],
    isChiefs: true,
    argv: i => ['board', 'drop', String(i.task), ...(typeof i.reason === 'string' && i.reason ? [i.reason] : [])],
  },
  {
    name: 'spawn',
    description: 'Start an agent in a repository, briefed on its task, and claim the task for it.',
    properties: { name: str('What to call it, usually after the repository'), repo: str('The repository, as an absolute path'), task: TASK },
    required: ['name', 'repo'],
    isChiefs: true,
    argv: i => ['spawn', String(i.name), '--repo', String(i.repo), ...opt('--task', i.task)],
  },
  {
    name: 'handoff',
    description: "Hand an agent's task to a fresh session of it, for one nearly out of context.",
    properties: { agent: str('The agent'), note: str('Anything the new session should know that the board does not say'), now: { type: 'boolean', description: 'Close the old session even mid-turn' } },
    required: ['agent'],
    isChiefs: true,
    argv: i => ['handoff', String(i.agent), ...opt('--note', i.note), ...(i.now === true ? ['--now'] : [])],
  },
  {
    name: 'retire',
    description: "Take an agent off the board once its work is done, and close its tab or background session.",
    properties: { name: str('The agent') },
    required: ['name'],
    isChiefs: true,
    argv: i => ['board', 'retire', String(i.name)],
  },
]

/** What Claude Code calls a tool of this plugin's. */
const TOOL_PREFIX = 'mcp__fleet__'

/** Run a board tool as the agent `me`, and answer with what fleet said. */
async function runBoardTool($: EngineInterface, tool: BoardTool, input: Record<string, unknown>, me: string) {
  try {
    // spawn and handoff wait for the new session to report.
    const out = await $.process.run(['fleet', ...tool.argv(input, me)], { timeoutMs: 120_000 })
    const said = (out.stdout + out.stderr).trim() || 'done'
    return out.exitCode === 0 ? { result: said } : { isError: true as const, result: said }
  } catch (err) {
    return { isError: true as const, result: `fleet could not be run: ${String(err)}` }
  }
}

/** Register the board's tools in this session. */
async function registerTools($: EngineInterface) {
  for (const t of BOARD_TOOLS) {
    try {
      await $.tool.register({ name: t.name, description: t.description, inputSchema: { type: 'object', properties: t.properties, required: t.required } })
    } catch {
      // A name already taken: the `fleet board` command still is.
    }
  }
}

/**
 * Where the `fleet` to start with is, or why there is none. The one that
 * matches this plugin first, so the mod and the board agree on what they
 * say to each other: FLEET_BIN when set, for working on fleet itself; then
 * the copy in this plugin's folder, fetched now if it is not there yet; and
 * only then whatever fleet is on PATH.
 */
async function findFleet($: EngineInterface): Promise<{ path: string } | { problem: string }> {
  const given = await $.env.get('FLEET_BIN')
  if (given) return { path: given }
  const own = `${$.plugin.root}/bin/fleet`
  if ((await $.process.run(['test', '-x', own])).exitCode === 0) return { path: own }
  const fetched = await $.process.run(['sh', `${$.plugin.root}/scripts/fetch-fleet.sh`], { timeoutMs: 120_000 })
  if (fetched.exitCode === 0) return { path: own }
  const onPath = await $.process.run(['sh', '-c', 'command -v fleet'])
  if (onPath.exitCode === 0 && onPath.stdout.trim()) return { path: onPath.stdout.trim() }
  return { problem: (fetched.stderr || fetched.stdout).trim() || 'fleet could not be downloaded' }
}

/** What `fleet chief --adopt` prints. */
type Adopted = { board: string; run: number; bin: string | null; brief: string }

/**
 * A plain `fleet board`, `fleet spawn` or `fleet handoff` command: what a
 * fleet session runs without being asked. Anything that chains, pipes,
 * redirects or substitutes is left to the usual prompt.
 */
const FLEET_COMMAND = /^\s*fleet\s+(?:board|spawn|handoff)\b[^;&|`$<>\n]*$/

/** A worker reaching for the go-ahead it is waiting for. */
const SELF_APPROVAL = /\bfleet\s+board\s+(?:go|gate)\b/

/**
 * Ask the board whether `session` may edit, and with `approve`, give its
 * task the user's go first. Undefined when the board cannot be asked.
 */
async function gate($: EngineInterface, session: string, approve = false): Promise<Gate | undefined> {
  const argv = ['fleet', 'board', 'gate', '--session', session]
  if (approve) argv.push('--user-approves')
  try {
    const out = await $.process.run(argv, { timeoutMs: 10_000 })
    return out.exitCode === 0 ? (JSON.parse(out.stdout) as Gate) : undefined
  } catch {
    return undefined
  }
}

const PANE = 'fleet'
/** From here an agent is close to compacting: the board's CONTEXT_HIGH. */
const HIGH = 80

const snapshot = atom({ plugin: 'fleet', key: 'snapshot' } as const, null)
const brief = atom({ plugin: 'fleet', key: 'brief' } as const, null)

/** The board as `session` sees it; undefined when it cannot be read. */
async function look($: EngineInterface, session: string): Promise<Snapshot | undefined> {
  try {
    const out = await $.process.run(['fleet', 'board', 'snapshot', '--session', session], { timeoutMs: 10_000 })
    return out.exitCode === 0 ? (JSON.parse(out.stdout) as Snapshot) : undefined
  } catch {
    return undefined
  }
}

/** A key as `fleet board view-send` names it, with its modifiers. */
function keyWords(e: { key: string; ctrl: boolean; shift: boolean; meta: boolean }): string[] | undefined {
  const name = e.key === ' ' ? 'space' : e.key
  if (!name || /\s/.test(name)) return undefined
  return ['key', name, ...(e.ctrl ? ['ctrl'] : []), ...(e.shift ? ['shift'] : []), ...(e.meta ? ['alt'] : [])]
}

/** Send the followed view one message: a key, a click or its size. */
async function sendView($: EngineInterface, control: string, words: string[]) {
  try {
    await $.process.run(['fleet', 'board', 'view-send', '--control', control, ...words], { timeoutMs: 5_000 })
  } catch {
    // The view has ended: the next tick starts another.
  }
}

/** Give a waiting agent its go, as the chief would with `fleet board go`. */
async function give($: EngineInterface, task: string) {
  try {
    const out = await $.process.run(['fleet', 'board', 'go', task], { timeoutMs: 10_000 })
    $.ui.toast(out.exitCode === 0 ? `${task}: go given` : `${task}: ${out.stderr.trim() || 'the go did not go through'}`)
  } catch {
    $.ui.toast(`${task}: fleet could not be run`)
  }
}

/** What needs the user, for the band and its count. */
function needs(s: Snapshot): string[] {
  const out: string[] = []
  for (const a of s.agents) {
    if (a.waiting_for) out.push(`${a.name} at a ${a.waiting_for}`)
    else if (a.awaiting_go && a.presence === 'waiting') out.push(`${a.name} needs a go`)
    if (a.context !== null && a.context >= HIGH) out.push(`${a.name} at ${a.context}%`)
  }
  return out
}

/**
 * Toast what is new since `seen`, and return what holds now. `seen` is
 * undefined until the first read, which marks everything already there as
 * seen: opening the chief must not replay yesterday's blockers.
 */
function notify($: EngineInterface, s: Snapshot, seen: Set<string> | undefined): Set<string> {
  const now = new Set<string>()
  const said: string[] = []
  for (const e of s.events) {
    if (!e.notice) continue
    now.add(`event:${e.key}`)
    if (seen && !seen.has(`event:${e.key}`)) said.push(`${e.notice.title} — ${e.notice.body}`)
  }
  for (const a of s.agents) {
    const marks: [string, string][] = []
    if (a.waiting_for) marks.push([`ask:${a.name}:${a.waiting_for}`, `fleet · ${a.name} is waiting at a ${a.waiting_for}`])
    if (a.awaiting_go && a.presence === 'waiting') {
      marks.push([`go:${a.name}:${a.awaiting_go}`, `fleet · ${a.name} needs a go on ${a.awaiting_go}`])
    }
    if (a.context !== null && a.context >= HIGH) {
      marks.push([`full:${a.name}`, `fleet · ${a.name} is at ${a.context}% context: \`fleet handoff ${a.name}\``])
    }
    for (const [mark, text] of marks) {
      now.add(mark)
      if (seen && !seen.has(mark)) said.push(text)
    }
  }
  for (const text of said) $.ui.toast(text)
  // Only what still holds, so a later wait or climb is news again.
  return now
}

export const register: Register = on => {
  // The session's id, once it has started.
  let session: string | undefined
  // Whether it is part of a fleet: started by one (FLEET_DB is set), or made
  // its chief with /fleet start. Until then the plugin does nothing at all.
  let active = false

  // The view's own: see the header.
  // Set once the board says this session is a worker: it draws nothing.
  let isWorker = false
  let looking = false
  // The fleet view behind the pane: `fleet board view --follow`, started
  // when the pane first asks for a frame and stopped when it closes. Its
  // newest frame, numbered, and the number the pane last got.
  let view: { control: string; size: { w: number; h: number } } | undefined
  let frame: GraphSpan[][] = []
  let frameNumber = 0
  let shownNumber = -1
  // What has been toasted already: see notify.
  let seen: Set<string> | undefined


  // A turn is running. Messages wait on the board until it ends, rather than
  // queueing up as prompts behind it.
  let busy = false
  // A prompt was submitted and its turn has not started yet. Taking more now
  // would stack a second prompt behind the first.
  let pending = false
  // A check is still running. The timer does not wait for one to finish.
  let checking = false
  // The main loop's tools running now, newest last: what the fleet view
  // shows the agent doing. Several run at once when Claude calls them in
  // parallel.
  const running: string[] = []

  on('session.start', async ($, e, next) => {
    if (!e.isInteractive) return next(e)
    const id = await $.session.id()
    session = id
    // /fleet, in every session with the plugin: in one fleet started it
    // shows the fleet, in any other `/fleet start` makes it the chief.
    try {
      await $.command.register({ name: 'fleet', description: 'Show the fleet, or `/fleet start` to make this session its chief', argumentHint: '[start]' })
    } catch {
      // A command of that name already: the band and toasts still work.
    }

    // A session fleet started has a board from the start. Any other waits
    // for /fleet start, and the timers below do nothing until then.
    active = Boolean(await $.env.get('FLEET_DB'))
    // The board as tools. Which ones the chief alone may use is settled when
    // they are called: a session is put on the board, and so has a role,
    // only after it starts.
    if (active) await registerTools($)

    const check = async () => {
      if (!active || checking) return
      checking = true
      try {
        const take = !busy && !pending
        const argv = ['fleet', 'board', 'inbox', '--session', id]
        if (take) argv.push('--take')
        const tool = running.at(-1)
        if (tool) argv.push('--tool', tool)
        try {
          const { context } = await $.session.usage()
          if (context.percent !== undefined) argv.push('--context', String(Math.round(context.percent)))
        } catch {
          // No reading yet, before the first answer: nothing to say.
        }
        const out = await $.process.run(argv, { timeoutMs: 10_000 })
        if (out.exitCode !== 0) return
        const inbox = JSON.parse(out.stdout) as Inbox
        if (inbox.text) {
          pending = true
          // Not awaited: it resolves when the turn starts, and the mailbox
          // has to keep checking in meanwhile or the board stops trusting it.
          $.prompt.submit({ text: inbox.text }).then(
            () => { pending = false },
            () => { pending = false },
          )
        }
        const parts: string[] = []
        if (inbox.awaiting_go) parts.push(`waiting for a go on ${inbox.awaiting_go}`)
        if (inbox.waiting > 0) {
          parts.push(`${inbox.waiting} ${inbox.waiting === 1 ? 'message' : 'messages'} waiting for this turn to end`)
        }
        $.ui.status(parts.length ? parts.join(' · ') : undefined)
      } catch {
        // fleet not on PATH, a board locked past the timeout, output that is
        // not JSON: the next check tries again, and while none succeeds the
        // board falls back to typing.
      } finally {
        checking = false
      }
    }

    $.clock.every(EVERY, () => { void check() })
    void check()

    // The chief's view, in every fleet session, since which one is the chief
    // is only known once the board has linked it.
    const watch = $.clock.every(EVERY, async () => {
      if (!active || isWorker || looking) return
      looking = true
      try {
        const s = await look($, id)
        if (!s) return
        if (s.me?.role === 'worker') {
          isWorker = true
          watch.cancel()
          return
        }
        // Not linked yet: a session is put on the board a few seconds after
        // it starts.
        if (s.me?.role !== 'chief') return
        await update($, snapshot, () => s)
        seen = notify($, s, seen)

      } finally {
        looking = false
      }
    })
    return next(e)
  })

  // The person typing in this pane is the user's own go-ahead. Three things
  // arrive the same way and are not: the brief fleet starts an agent with,
  // given on the command line, which is always a new session's first prompt
  // (a resumed one has turns already); a message fleet typed here because
  // the mod was not checking in; and a slash command.
  on('prompt.submit', async ($, e, next) => {
    const typed = e.origin?.kind === 'composer' || e.origin?.kind === 'bridge'
    const text = e.text.trimStart()
    if (
      active && session && typed && !text.startsWith('[fleet ·') && !text.startsWith('/') &&
      (await $.session.turns()) > 0
    ) {
      await gate($, session, true)
    }
    return next(e)
  })

  on('tool.call', async ($, e, next) => {
    // The main loop's tool, not a subagent's: those run under the main
    // loop's Agent call, which is what the fleet view says it is doing.
    const main = session !== undefined && e.agentId === undefined
    if (main) running.push(e.tool)
    try {
      const boardTool = active && session && e.tool.startsWith(TOOL_PREFIX)
        ? BOARD_TOOLS.find(t => TOOL_PREFIX + t.name === e.tool)
        : undefined
      if (boardTool && session) {
        const g = await gate($, session)
        if (!g?.agent) return { isError: true as const, result: 'fleet: this session is not on the board yet; try again in a moment' }
        if (g.role === 'retired') return { deny: `fleet: this session of ${g.agent} was retired, and changes nothing more.` }
        if (boardTool.isChiefs && g.role !== 'chief') {
          return { deny: `fleet: ${boardTool.name} is the chief's. Ask the chief with board_msg.` }
        }
        return await runBoardTool($, boardTool, e as unknown as Record<string, unknown>, g.agent)
      }
      const edit = EDITS.has(e.tool)
      const command = e.tool === 'Bash' && typeof e.command === 'string' ? e.command : undefined
      if (!active || !session || (!edit && command === undefined)) return await next(e)
      if (command !== undefined && !writes(command) && !SELF_APPROVAL.test(command)) return await next(e)

      const g = await gate($, session)
      if (!g?.agent) return await next(e)

      // Retired, or handed off to a fresh session that carries on: what
      // this one changes now, the other does not know about.
      if (g.role === 'retired') {
        return {
          deny:
            `fleet: this session of ${g.agent} was retired, and changes nothing more. ` +
            'If its task was handed off, a fresh session is carrying on with it.',
        }
      }

      if (g.role === 'chief') {
        if (edit || (command !== undefined && writes(command))) {
          return {
            deny:
              'fleet: the chief of staff does not change files, through Bash either. ' +
              'Put the work on the board and give it to the agent for that repository.',
          }
        }
        return await next(e)
      }

      if (command !== undefined && SELF_APPROVAL.test(command)) {
        return { deny: `fleet: a go-ahead comes from the chief or the user, not from ${g.agent}.` }
      }
      if (g.approved) return await next(e)
      const task = g.tasks.join(', ')
      return {
        deny:
          `fleet: ${task} has no go-ahead yet, so ${g.agent} does not change files. ` +
          `Send the chief your plan with \`fleet board msg ${g.agent} chief '...'\` and wait for its go, ` +
          'or for the user to answer you here.',
      }
    } finally {
      if (main) running.splice(running.lastIndexOf(e.tool), 1)
    }
  })

  // A chief made with /fleet start has its brief as a turn, and compaction
  // summarises turns: put the brief back at the head of what is kept. A
  // chief fleet started has it in its system prompt, which compaction
  // leaves alone, so it has nothing here to put back.
  on('session.compact', async ($, e, next) => {
    const compacted = await next(e)
    const kept = await read($, brief)
    if (!active || !kept || e.agentId !== undefined || !('messages' in compacted) || !compacted.messages) {
      return compacted
    }
    const reminder = {
      role: 'user' as const,
      text: `[fleet] You are this workspace's chief of staff. Your brief, kept through compaction:\n\n${kept}`,
      toolUses: [],
    }
    return { ...compacted, messages: [reminder, ...compacted.messages] }
  })

  // fleet's own tools and plain fleet commands need no prompt in a fleet
  // session: an agent started by fleet has them allowed on its command
  // line, and a session made the chief with /fleet start has this.
  on('tool.check', ($, e, next) => {
    if (!active) return next(e)
    if (e.tool.startsWith(TOOL_PREFIX)) return { decision: 'allow' as const }
    const command = (e.input as { command?: unknown } | null)?.command
    if (e.tool === 'Bash' && typeof command === 'string' && FLEET_COMMAND.test(command)) {
      return { decision: 'allow' as const }
    }
    return next(e)
  })

  on('turn.start', ($, e, next) => {
    busy = true
    pending = false
    return next(e)
  })

  on('turn.complete', async ($, e, next) => {
    const done = await next(e)
    // A subagent's run ends with a turn.complete of its own, in the middle
    // of the main loop's turn; only the main loop's says the session is idle.
    if (e.agentId === undefined) busy = false
    return done
  })

  on('command.run', { command: 'fleet' }, async ($, e) => {
    if (e.args.trim() === 'start') {
      if (active) return { text: 'This session is part of a fleet already. /fleet shows it.' }
      if (!session) return { text: 'This session has not started yet.' }
      const root = await $.session.root()
      let adopted: Adopted
      try {
        const found = await findFleet($)
        if ('problem' in found) return { text: `${found.problem}\nInstall fleet another way (see its README), then run /fleet start again.` }
        const out = await $.process.run([found.path, 'chief', '--adopt', session, '--root', root], { timeoutMs: 20_000 })
        if (out.exitCode !== 0) return { text: `fleet: ${(out.stderr || out.stdout).trim()}` }
        adopted = JSON.parse(out.stdout) as Adopted
      } catch (err) {
        return { text: `fleet could not be started: ${String(err)}` }
      }
      // What a session fleet starts is given on its command line: the board,
      // the run, and fleet itself on PATH, for this session's processes and
      // the agents it starts.
      await $.env.set('FLEET_DB', adopted.board)
      await $.env.set('FLEET_RUN', String(adopted.run))
      if (adopted.bin) await $.env.set('PATH', `${adopted.bin}:${(await $.env.get('PATH')) ?? ''}`)
      await registerTools($)
      active = true
      // The chief's brief, as a turn of its own: Claude Code keeps a mod
      // installed by a user out of the system prompt. From a timer, since a
      // command cannot wait on a turn while it holds this one.
      $.clock.after(1, () => { void $.prompt.submit({ text: adopted.brief }) })
      // Kept for compaction, which summarises a turn like any other.
      await update($, brief, () => adopted.brief)
      return { text: `This session is the chief of ${root} now. Its brief follows; /fleet shows the crew.` }
    }
    if (!active) return { text: 'This session is not part of a fleet. `/fleet start` makes it the chief of this workspace.' }
    if (isWorker) return { text: "The fleet is drawn in the chief's session." }
    const opened = await $.ui.open({ id: PANE, title: 'fleet' })
    return opened.isPlaced ? {} : { text: 'Widen the terminal to see the fleet pane.' }
  })

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const { Box, Button, Client, Text } = $.ui.resolve(e)
    const s = await read($, snapshot)
    if (!s) return <Text dimColor>Reading the board…</Text>
    const waiting = s.agents.filter(a => a.awaiting_go && a.presence === 'waiting')
    // The fleet tab, live: fleet draws it, the Client shows it and takes its
    // keys. Beneath it, while a worker waits, the go the tab gives by keys.
    // The rows the pane shows, not the terminal's: the prompt and the
    // status take the bottom of the screen.
    const shown = e.props.scroll?.bodyRows ?? (e.viewport?.rows ?? 30) - 8
    const rows = Math.max(10, shown - (waiting.length > 0 ? 1 : 0))
    return (
      <Box flexDirection="column" width={Math.max(40, e.props.bodyColumns ?? 80)}>
        <Client key="fleet-view" module="./view.tsx" props={{ lines: frame }} width="100%" height={rows} />
        {waiting.length > 0 && (
          <Box flexDirection="row" gap={1}>
            <Text>  Waiting for a go:</Text>
            {waiting.map(a => (
              <Button key={`go-${a.name}`} label={`go ${a.awaiting_go}`} onPress={() => give($, a.awaiting_go ?? '')} />
            ))}
          </Box>
        )}
      </Box>
    )
  })

  // What the pane's view posts: a tick asking for a newer frame, with its
  // size; a key; a click.
  on('ui.message', async ($, e, next) => {
    const data = e.data as { t?: string; w?: number; h?: number; key?: string; ctrl?: boolean; shift?: boolean; meta?: boolean; x?: number; y?: number }
    if (!active || !session) return next(e)
    if (data.t === 'tick' && typeof data.w === 'number' && typeof data.h === 'number' && data.w > 0 && data.h > 0) {
      const size = { w: data.w, h: data.h }
      if (!view) {
        const home = (await $.env.get('HOME')) ?? '/tmp'
        // Short: a socket's path has a hundred characters or so to fit in.
        const control = `${home}/.claude-fleet/views/${session.slice(0, 8)}.sock`
        const root = await $.session.root()
        view = { control, size }
        const argv = ['fleet', 'board', 'view', '--root', root, '--width', String(size.w), '--height', String(size.h), '--follow', '--control', control]
        void (async () => {
          let pending = ''
          try {
            for await (const piece of $.process.spawn({ argv })) {
              if (piece.stream !== 'stdout') continue
              pending += piece.text
              let end = pending.indexOf('\n')
              while (end >= 0) {
                const line = pending.slice(0, end)
                pending = pending.slice(end + 1)
                try {
                  frame = (JSON.parse(line) as { lines: GraphSpan[][] }).lines
                  frameNumber += 1
                } catch {
                  // Not a frame: nothing to draw.
                }
                end = pending.indexOf('\n')
              }
            }
            // It ended by itself: the view was quit, as `q` quits the fleet
            // tab. The pane goes with it.
            view = undefined
            await $.ui.close({ id: PANE })
          } catch {
            // It could not start, or it died: the next tick starts another.
          } finally {
            view = undefined
          }
        })()
      } else if (view.size.w !== size.w || view.size.h !== size.h) {
        view.size = size
        await sendView($, view.control, ['resize', String(size.w), String(size.h)])
      }
      if (frameNumber !== shownNumber) {
        shownNumber = frameNumber
        return { props: { lines: frame } }
      }
      return {}
    }
    if (!view) return {}
    if (data.t === 'key' && typeof data.key === 'string') {
      const words = keyWords({ key: data.key, ctrl: data.ctrl === true, shift: data.shift === true, meta: data.meta === true })
      if (words) await sendView($, view.control, words)
      return {}
    }
    if (data.t === 'click' && typeof data.x === 'number' && typeof data.y === 'number') {
      await sendView($, view.control, ['click', String(data.x), String(data.y)])
      return {}
    }
    return next(e)
  })

  // The pane closed: the view behind it has no one to draw for.
  on('ui.close', async ($, e, next) => {
    const closed = await next(e)
    if (view && (e as { id?: string }).id === PANE) {
      await sendView($, view.control, ['quit'])
      frame = []
      shownNumber = -1
    }
    return closed
  })

  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    if (isWorker || e.props.hasSurvey) return next(e)
    const s = await read($, snapshot)
    const crew = s?.agents.filter(a => a.role === 'worker') ?? []
    if (!s || crew.length === 0) return next(e)
    const { Box, Text } = $.ui.resolve(e)
    const want = needs(s)
    const working = crew.filter(a => a.presence === 'working').length
    const open = s.tasks.length

    return (
      <Box flexDirection="row">
        <Text dimColor>{`fleet · ${crew.length} agent${crew.length === 1 ? '' : 's'} · ${working} working · ${open} open`}</Text>
        {want.length > 0 && <Text color="red">{` · ${want.join(' · ')}`}</Text>}
        <Text dimColor> · /fleet</Text>
      </Box>
    )
  })
}
