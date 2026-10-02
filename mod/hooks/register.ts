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

import type { Register } from 'claude-code'

/** How often the mailbox is checked. Well inside the board's fifteen. */
const EVERY = 3_000

/** What `fleet board inbox` prints. */
type Inbox = {
  agent: string | null
  text: string | null
  waiting: number
}

export const register: Register = on => {
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
    const session = await $.session.id()

    const check = async () => {
      if (checking) return
      checking = true
      try {
        const take = !busy && !pending
        const argv = ['fleet', 'board', 'inbox', '--session', session]
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
