import { test, expect, mock } from 'claude-code/testing'
import type { On } from 'claude-code'

import type { Snapshot } from '../types'

const start = { cwd: '/repo', surface: 'terminal', isInteractive: true } as const

function chiefSees(): Snapshot {
  return {
    me: { name: 'chief', role: 'chief' },
    agents: [
      { name: 'chief', role: 'chief', presence: 'waiting', waiting_for: null, tool: null, context: 31, task: null, awaiting_go: null, target: 'bg:aaaa1111' },
      { name: 'billing', role: 'worker', presence: 'waiting', waiting_for: null, tool: null, context: 22, task: 'ENG-1-1', awaiting_go: 'ENG-1-1', target: 'bg:5cba7f8f' },
      { name: 'accounts', role: 'worker', presence: 'working', waiting_for: null, tool: 'mcp__claude_ai_Slack__slack_send_message', context: 84, task: 'ENG-1-2', awaiting_go: null, target: 'bg:6d1e2f3a' },
    ],
    tasks: [
      { key: 'ENG-1-1', title: 'write notes', state: 'queued', agent: 'billing', note: null },
      { key: 'ENG-1-2', title: 'share the column', state: 'blocked', agent: 'accounts', note: 'needs the flag' },
    ],
    events: [
      { key: 'e1', kind: 'message', from: 'billing', to: 'chief', task: 'ENG-1-1', summary: 'plan: add the column', notice: null },
    ],
  }
}

/** A board that answers the view's reads with `seen`, and records a go. */
function board(on: On, seen: { now: Snapshot }) {
  const gone: string[][] = []
  const drawn: string[][] = []
  on('process.run', ($, e) => {
    const argv = [...e.argv]
    let stdout = JSON.stringify({ agent: 'chief', text: null, waiting: 0, awaiting_go: null })
    if (argv[2] === 'snapshot') stdout = JSON.stringify(seen.now)
    if (argv[2] === 'view') {
      drawn.push(argv)
      stdout = JSON.stringify({ lines: [
        [],
        [{ t: '  fleet  /w/acme' }, { t: '1 needs a go', fg: '#d97757' }],
        [{ t: '───────────┬──' }],
        [{ t: '│ ' }, { t: '◆ chief', fg: '#d97757', bold: true }, { t: ' │  TASKS' }],
        [],
        [{ t: '│ ○ billing   ' }, { t: '◇ needs a go', fg: '#d97757' }, { t: '│   · write notes' }],
      ] })
    }
    if (argv[2] === 'go') gone.push(argv)
    return { value: { exitCode: 0, stdout, stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  on('session.id', () => ({ value: 'sid-chief' }))
  on('session.start', ($, e) => ({ cwd: e.cwd }))
  on('session.root', () => ({ value: '/w/acme' }))
  on('session.usage', () => ({ value: { startedAt: 0, context: { window: 200_000, percent: 31 }, rateLimits: [] } }))
  on('command.register', () => ({ value: undefined }))
  on('ui.status', () => ({ value: undefined }))
  const toasts: string[] = []
  on('ui.toast', ($, e) => {
    toasts.push(e.text)
    return { value: undefined }
  })
  return { gone, toasts, drawn }
}

test("the chief's pane is the fleet tab's view at its size, and a waiting agent gets its go from it", async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  const clock = mock.clock(on)
  const seen = { now: chiefSees() }
  const { gone, drawn } = board(on, seen)

  await $.session.start(start)
  await clock.advance(3_000)
  const ui = await $.ui.mount({ plugin: 'fleet', surface: 'terminal', component: 'Pane', requestId: 'fleet', props: { bodyColumns: 120 } as never })
  expect(drawn).toEqual([], 'nothing is drawn for a pane nobody opened')

  // The next read draws the whole view, for this workspace, at the pane's size.
  await clock.advance(3_000)
  expect(drawn.at(-1)?.slice(0, 7)).toEqual(['fleet', 'board', 'view', '--root', '/w/acme', '--width', '120'])
  expect(await ui.find({ text: /fleet {2}\/w\/acme/ })).toBeDefined()
  expect((await ui.find({ text: /◆ chief/ }))?.text).toContain('◆ chief')
  expect(await ui.find({ text: /TASKS/ })).toBeDefined()

  await ui.press({ key: 'go-billing' })
  expect(gone).toEqual([['fleet', 'board', 'go', 'ENG-1-1']])
})

test("the band above the chief's prompt says who needs something", async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  const clock = mock.clock(on)
  board(on, { now: chiefSees() })

  await $.session.start(start)
  await clock.advance(3_000)

  const band = await $.ui.mount({ plugin: 'fleet', surface: 'terminal', component: 'AbovePrompt', props: { hasSurvey: false, bodyColumns: 120 } as never })
  const said = (await band.findAll({ type: 'Text' })).map(t => t.text).join('')
  expect(said).toContain('2 agents · 1 working · 2 open')
  expect(said).toContain('billing needs a go')
  expect(said).toContain('accounts at 84%')
})

test('what was already on the board is not news; what comes after is, once', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  const clock = mock.clock(on)
  const seen = { now: chiefSees() }
  const { toasts } = board(on, seen)

  await $.session.start(start)
  await clock.advance(3_000)
  expect(toasts).toEqual([])

  const next = chiefSees()
  next.events.unshift({ key: 'e2', kind: 'task', from: 'accounts', to: null, task: 'ENG-1-2', summary: 'done', notice: { title: 'fleet · ENG-1-2 is done', body: 'accounts' } })
  next.agents[1].waiting_for = 'permission prompt'
  seen.now = next
  await clock.advance(3_000)
  expect(toasts).toEqual(['fleet · ENG-1-2 is done — accounts', 'fleet · billing is waiting at a permission prompt'])

  await clock.advance(3_000)
  expect(toasts.length).toBe(2)
})

test("a worker's session stops reading once the board says it is one", async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  const clock = mock.clock(on)
  const worker = { ...chiefSees(), me: { name: 'billing', role: 'worker' } }
  let reads = 0
  board(on, {
    get now() {
      reads += 1
      return worker
    },
  } as { now: Snapshot })

  await $.session.start(start)
  await clock.advance(9_000)
  expect(reads).toBe(1)
})
