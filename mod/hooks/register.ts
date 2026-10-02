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

import type { EngineInterface, Register } from 'claude-code'

/** How often the mailbox is checked. Well inside the board's fifteen. */
const EVERY = 3_000

/** What `fleet board inbox` prints. */
type Inbox = {
  agent: string | null
  text: string | null
  waiting: number
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

export const register: Register = on => {
  // Set once the session has started and turns out to be one of fleet's.
  let session: string | undefined

  // A turn is running. Messages wait on the board until it ends, rather than
  // queueing up as prompts behind it.
  let busy = false
  // A prompt was submitted and its turn has not started yet. Taking more now
  // would stack a second prompt behind the first.
  let pending = false
  // A check is still running. The timer does not wait for one to finish.
  let checking = false

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
        $.ui.status(
          inbox.waiting > 0
            ? `${inbox.waiting} ${inbox.waiting === 1 ? 'message' : 'messages'} waiting for this turn to end`
            : undefined,
        )
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
    const edit = EDITS.has(e.tool)
    const command = e.tool === 'Bash' && typeof e.command === 'string' ? e.command : undefined
    if (!session || (!edit && command === undefined)) return next(e)
    if (command !== undefined && !writes(command) && !SELF_APPROVAL.test(command)) return next(e)

    const g = await gate($, session)
    if (!g?.agent) return next(e)

    if (g.role === 'chief') {
      if (edit || (command !== undefined && writes(command))) {
        return {
          deny:
            'fleet: the chief of staff does not change files, through Bash either. ' +
            'Put the work on the board and give it to the agent for that repository.',
        }
      }
      return next(e)
    }

    if (command !== undefined && SELF_APPROVAL.test(command)) {
      return { deny: `fleet: a go-ahead comes from the chief or the user, not from ${g.agent}.` }
    }
    if (g.approved) return next(e)
    const task = g.tasks.join(', ')
    return {
      deny:
        `fleet: ${task} has no go-ahead yet, so ${g.agent} does not change files. ` +
        `Send the chief your plan with \`fleet board msg ${g.agent} chief '...'\` and wait for its go, ` +
        'or for the user to answer you here.',
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
}
