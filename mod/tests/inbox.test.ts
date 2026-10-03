import { test, expect, mock } from 'claude-code/testing'
import type { On } from 'claude-code'

/** A board that answers `fleet board inbox` with what it holds. */
function board(on: On, held: string[], awaiting_go: string | null = null) {
  const runs: string[][] = []
  on('process.run', ($, e) => {
    // The view's read: this session is a worker, so it stops reading.
    if (e.argv[2] === 'snapshot') {
      const stdout = JSON.stringify({ me: { name: 'accounts-svc', role: 'worker' }, agents: [], tasks: [], events: [] })
      return { value: { exitCode: 0, stdout, stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
    }
    runs.push([...e.argv])
    const take = e.argv.includes('--take')
    const text = take && held.length ? held.splice(0).join('\n\n') : null
    const stdout = JSON.stringify({ agent: 'accounts-svc', text, waiting: take ? 0 : held.length, awaiting_go })
    return { value: { exitCode: 0, stdout, stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  return runs
}

function session(on: On, percent?: number) {
  on('session.id', () => ({ value: 'sid-1' }))
  on('session.usage', () => ({ value: { startedAt: 0, context: { window: 200_000, percent }, rateLimits: [] } }))
  on('session.start', ($, e) => ({ cwd: e.cwd }))
  const submitted: string[] = []
  on('prompt.submit', ($, e) => {
    submitted.push(e.text)
    return { turnId: 't-1' }
  })
  const status: (string | undefined)[] = []
  on('ui.status', ($, e) => {
    status.push(e.text)
    return { value: undefined }
  })
  return { submitted, status }
}

const start = { cwd: '/repo', surface: 'terminal', isInteractive: true } as const

test('an idle session takes what is waiting and submits it', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  const clock = mock.clock(on)
  const runs = board(on, ['[fleet · chief · ENG-1-1] go\n\nthe whole body'])
  const { submitted } = session(on)

  await $.session.start(start)
  await clock.advance(3_000)

  expect(runs[0]).toEqual(['fleet', 'board', 'inbox', '--session', 'sid-1', '--take'])
  expect(submitted).toEqual(['[fleet · chief · ENG-1-1] go\n\nthe whole body'])
})

test('a busy session checks in without taking, and says what waits', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  const clock = mock.clock(on)
  const held: string[] = []
  const runs = board(on, held)
  const { submitted, status } = session(on)
  on('turn.start', ($, e) => ({ turnId: e.turnId }))

  await $.session.start(start)
  await $.turn.start({ text: 'work', turnId: 't-0' })
  runs.length = 0
  held.push('[fleet · chief] hold off')
  await clock.advance(3_000)

  expect(runs.length).toBe(1)
  expect(runs[0]).not.toContain('--take')
  expect(submitted).toEqual([])
  expect(status.at(-1)).toBe('1 message waiting for this turn to end')
})

test('what waited through a turn is taken when the turn ends, not when a subagent does', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  const clock = mock.clock(on)
  const held: string[] = []
  board(on, held)
  const { submitted } = session(on)
  on('turn.start', ($, e) => ({ turnId: e.turnId }))
  on('turn.complete', () => ({ text: '' }))
  const done = { answer: '', durationMs: 1, isAborted: false, turnId: 't-0', reason: 'answer' } as const

  await $.session.start(start)
  await $.turn.start({ text: 'work', turnId: 't-0' })
  held.push('[fleet · chief] go')
  await clock.advance(3_000)
  await $.turn.complete({ ...done, agentId: 'sub-1' })
  await clock.advance(3_000)
  expect(submitted).toEqual([])

  await $.turn.complete(done)
  await clock.advance(3_000)
  expect(submitted).toEqual(['[fleet · chief] go'])
})

test('a worker held back from editing says what it waits for under its prompt', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  const clock = mock.clock(on)
  const held: string[] = []
  board(on, held, 'ENG-1-1')
  const { status } = session(on)
  on('turn.start', ($, e) => ({ turnId: e.turnId }))

  await $.session.start(start)
  await clock.advance(3_000)
  expect(status.at(-1)).toBe('waiting for a go on ENG-1-1')

  await $.turn.start({ text: 'work', turnId: 't-0' })
  held.push('[fleet · chief] what is the plan?')
  await clock.advance(3_000)
  expect(status.at(-1)).toBe('waiting for a go on ENG-1-1 · 1 message waiting for this turn to end')
})

test('the check-in says which tool is running and how full the context is', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  const clock = mock.clock(on)
  const runs = board(on, [])
  session(on, 71.4)
  // A tool that runs until the test lets it finish.
  let finish = () => {}
  on('tool.call', () => new Promise(resolve => { finish = () => resolve({ result: 'ok' }) }))

  await $.session.start(start)
  const call = $.tool.call({ tool: 'Bash', command: 'cargo test' })
  await clock.advance(3_000)
  expect(runs.at(-1)).toEqual(['fleet', 'board', 'inbox', '--session', 'sid-1', '--take', '--tool', 'Bash', '--context', '71'])

  finish()
  await call
  await clock.advance(3_000)
  expect(runs.at(-1)).toEqual(['fleet', 'board', 'inbox', '--session', 'sid-1', '--take', '--context', '71'])
})

test('a session fleet did not start leaves the board alone', async ($, on) => {
  mock.env(on, {})
  const clock = mock.clock(on)
  const runs = board(on, ['[fleet · chief] go'])
  session(on)

  await $.session.start(start)
  await clock.advance(10_000)

  expect(runs).toEqual([])
})
