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
// And in the chief's session it draws the fleet. In herdr the fleet tab sits
// beside the chief; outside herdr there is no tab, so `/fleet` opens a pane
// with the crew, the open tasks and the latest messages, a band above the
// prompt says who needs something, and toasts say what herdr would have
// notified. It reads `fleet board snapshot` every few seconds; a worker's
// session stops reading once the board says it is one.

import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register } from 'claude-code'

import type { SnapAgent, Snapshot } from '../types'

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

/** The board as `session` sees it; undefined when it cannot be read. */
async function look($: EngineInterface, session: string): Promise<Snapshot | undefined> {
  try {
    const out = await $.process.run(['fleet', 'board', 'snapshot', '--session', session], { timeoutMs: 10_000 })
    return out.exitCode === 0 ? (JSON.parse(out.stdout) as Snapshot) : undefined
  } catch {
    return undefined
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

/** What an agent is doing, in a few words, and the colour to say it in. */
function state(a: SnapAgent): [string, string | undefined] {
  if (a.waiting_for) return [`! ${a.waiting_for}`, 'red']
  if (a.awaiting_go && a.presence === 'waiting') return ['◇ needs a go', 'red']
  if (a.awaiting_go && a.presence === 'working') return ['● planning', 'yellow']
  switch (a.presence) {
    case 'working':
      return [`● ${a.tool ? toolName(a.tool) : 'working'}`, 'yellow']
    case 'waiting':
      return ['○ waiting', 'green']
    case 'gone':
      return ['× ended', undefined]
    default:
      return ['· not started', undefined]
  }
}

/** `mcp__claude_ai_Slack__slack_send_message` as `slack_send_message`. */
function toolName(tool: string): string {
  if (!tool.startsWith('mcp__')) return tool
  const rest = tool.slice('mcp__'.length)
  const at = rest.indexOf('__')
  return at < 0 ? rest : rest.slice(at + 2)
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

function pad(text: string, width: number): string {
  return text.length >= width ? text.slice(0, width) : text + ' '.repeat(width - text.length)
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
  // Set once the session has started and turns out to be one of fleet's.
  let session: string | undefined

  // The view's own: see the header.
  // Set once the board says this session is a worker: it draws nothing.
  let isWorker = false
  let looking = false
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
    // Only a session fleet started has a board. The plugin is handed to no
    // other, but a mod that checks costs nothing.
    if (!e.isInteractive || !(await $.env.get('FLEET_DB'))) return next(e)
    const id = await $.session.id()
    session = id

    const check = async () => {
      if (checking) return
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

    // The chief's view. Registered in every fleet session, since which one
    // is the chief is only known once the board has linked it.
    try {
      await $.command.register({ name: 'fleet', description: 'Show the fleet: the crew, the open tasks, the latest messages' })
    } catch {
      // A command of that name already: the band and toasts still work.
    }
    const watch = $.clock.every(EVERY, async () => {
      if (isWorker || looking) return
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
      session && typed && !text.startsWith('[fleet ·') && !text.startsWith('/') &&
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
      const edit = EDITS.has(e.tool)
      const command = e.tool === 'Bash' && typeof e.command === 'string' ? e.command : undefined
      if (!session || (!edit && command === undefined)) return await next(e)
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

  on('command.run', { command: 'fleet' }, async $ => {
    if (isWorker) return { text: "The fleet is drawn in the chief's session." }
    const opened = await $.ui.open({ id: PANE, title: 'fleet' })
    return opened.isPlaced ? {} : { text: 'Widen the terminal to see the fleet pane.' }
  })

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const { Box, Button, Text } = $.ui.resolve(e)
    const s = await read($, snapshot)
    if (!s) return <Text dimColor>Reading the board…</Text>
    const width = Math.max(40, e.props.bodyColumns ?? 80)
    const nameWidth = Math.min(18, Math.max(6, ...s.agents.map(a => a.name.length)) + 1)
    const messages = s.events.filter(ev => ev.kind === 'message').slice(0, 6)

    return (
      <Box flexDirection="column" width={width}>
        <Text bold>Crew</Text>
        {s.agents.length === 0 && <Text dimColor>No agents yet: the chief starts them with fleet spawn.</Text>}
        {s.agents.map(a => {
          const [said, colour] = state(a)
          const full = a.context !== null && a.context >= HIGH
          return (
            <Box key={`agent-${a.name}`} flexDirection="row" gap={1}>
              <Text bold={a.role === 'chief'}>{pad(a.name, nameWidth)}</Text>
              <Text color={colour}>{pad(said, 22)}</Text>
              <Text color={full ? 'red' : undefined} dimColor={!full}>
                {a.context === null ? '    ' : pad(`${a.context}%`, 4)}
              </Text>
              <Text dimColor wrap="truncate-end">{a.task ?? ''}</Text>
              {a.awaiting_go && a.presence === 'waiting' && (
                <Button key={`go-${a.name}`} label={`go ${a.awaiting_go}`} onPress={() => give($, a.awaiting_go ?? '')} />
              )}
            </Box>
          )
        })}
        {s.agents.some(a => a.target?.startsWith('bg:')) && (
          <Text dimColor>`claude agents` opens any of them; `claude attach &lt;id&gt;` one.</Text>
        )}
        <Text> </Text>
        <Text bold>Open tasks</Text>
        {s.tasks.length === 0 && <Text dimColor>Nothing open.</Text>}
        {s.tasks.map(t => (
          <Box key={`task-${t.key}`} flexDirection="row" gap={1}>
            <Text>{pad(t.key, 12)}</Text>
            <Text color={t.state === 'blocked' ? 'red' : t.state === 'running' ? 'yellow' : undefined}>{pad(t.state, 8)}</Text>
            <Text dimColor>{pad(t.agent ?? '', nameWidth)}</Text>
            <Text wrap="truncate-end">{t.note ? `${t.title} — ${t.note}` : t.title}</Text>
          </Box>
        ))}
        <Text> </Text>
        <Text bold>Latest messages</Text>
        {messages.length === 0 && <Text dimColor>None yet.</Text>}
        {messages.map(m => (
          <Text key={`msg-${m.key}`} wrap="truncate-end">
            <Text dimColor>{`${m.from ?? '?'} → ${m.to ?? '?'}: `}</Text>
            {m.summary}
          </Text>
        ))}
      </Box>
    )
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
