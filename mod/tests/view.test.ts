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
  const handed: string[][] = []
  const sent: string[][] = []
  const followed: string[][] = []
  on('process.spawn', async function* ($, e) {
    followed.push([...e.argv])
    const frame = { lines: [
      [],
      [{ t: '  fleet  /w/acme' }],
      [{ t: '│ ' }, { t: '◆ chief', fg: '#d97757', bold: true }, { t: ' │  TASKS' }],
    ] }
    yield { stream: 'stdout' as const, text: JSON.stringify(frame) + '\n' }
    // A followed view runs until it is quit: this one, until the test ends.
    await new Promise(() => {})
    return { code: 0, signal: null }
  })
  on('process.run', ($, e) => {
    const argv = [...e.argv]
    let stdout = JSON.stringify({ agent: 'chief', text: null, waiting: 0, awaiting_go: null })
    if (argv[2] === 'snapshot') stdout = JSON.stringify(seen.now)
    if (argv[2] === 'go') gone.push(argv)
    if (argv[1] === 'handoff') handed.push(argv)
    if (argv[2] === 'view-send') sent.push(argv.slice(5))
    return { value: { exitCode: 0, stdout, stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  on('session.id', () => ({ value: 'sid-chief' }))
  on('session.start', ($, e) => ({ cwd: e.cwd }))
  on('session.root', () => ({ value: '/w/acme' }))
  on('session.usage', () => ({ value: { startedAt: 0, context: { window: 200_000, percent: 31 }, rateLimits: [] } }))
  on('command.register', () => ({ value: undefined }))
  on('ui.status', () => ({ value: undefined }))
  const opened: string[] = []
  on('ui.open', ($, e) => {
    opened.push(e.id)
    return { value: { isPlaced: true as const } }
  })
  const toasts: string[] = []
  on('ui.toast', ($, e) => {
    toasts.push(e.text)
    return { value: undefined }
  })
  return { gone, handed, toasts, sent, followed, opened }
}

test("the chief's pane is the fleet tab, live: frames in, keys and clicks out", async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db', HOME: '/home/me' })
  const clock = mock.clock(on)
  const seen = { now: chiefSees() }
  const { gone, sent, followed } = board(on, seen)

  await $.session.start(start)
  await clock.advance(3_000)
  const ui = await $.ui.mount({ plugin: 'fleet', surface: 'terminal', component: 'Pane', requestId: 'fleet', props: { bodyColumns: 120 } as never })
  expect(followed).toEqual([], 'nothing runs before the pane asks for a frame')

  // The view asks for a frame with its size: fleet's view starts, followed.
  await ui.post({ t: 'tick', w: 118, h: 28 })
  expect(followed[0]).toEqual(['fleet', 'board', 'view', '--root', '/w/acme', '--width', '118', '--height', '28', '--follow', '--control', '/home/me/.claude-fleet/views/sid-chie.sock'])
  await ui.post({ t: 'tick', w: 118, h: 28 })
  expect((await ui.find({ text: /◆ chief/, in: 'fleet-view' }))?.text).toContain('◆ chief')
  expect(await ui.find({ text: /TASKS/, in: 'fleet-view' })).toBeDefined()

  // Its keys and clicks go to the view, as the tab's do.
  await ui.post({ t: 'key', key: 'l', ctrl: false, shift: false, meta: false })
  await ui.post({ t: 'key', key: 'return', ctrl: false, shift: false, meta: false })
  await ui.post({ t: 'click', x: 30, y: 7 })
  await ui.post({ t: 'tick', w: 140, h: 30 })
  expect(sent).toEqual([['key', 'l'], ['key', 'return'], ['click', '30', '7'], ['resize', '140', '30']])

  await ui.press({ key: 'go-billing' })
  expect(gone).toEqual([['fleet', 'board', 'go', 'ENG-1-1']])
})

test("the band above the chief's prompt is one quiet line, and a second for the most urgent ask", async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  const clock = mock.clock(on)
  const { gone } = board(on, { now: chiefSees() })

  await $.session.start(start)
  await clock.advance(3_000)

  for (const surface of ['terminal', 'desktop'] as const) {
    const band = await $.ui.mount({ plugin: 'fleet', surface, component: 'AbovePrompt', props: { hasSurvey: false, bodyColumns: 120 } as never })
    const said = (await band.findAll({ type: 'Text' })).map(t => t.text).join('|')
    // A dot per agent: billing waits for a go, accounts works.
    expect(said).toContain('◆|●')
    expect(said).toContain('1 working')
    expect(said).toContain('0 idle')
    expect(said).toContain('2 open')
    // The go first, in billing's own words, and the handoff counted after it.
    expect(said).toContain('▲ billing')
    expect(said).toContain('plan: add the column')
    expect(said).toContain('+1 more')
    expect(said).not.toContain('84%')
    expect(await band.find({ key: 'handoff-accounts' })).toBeUndefined()
    await band.press({ key: 'go-billing' })
  }
  expect(gone).toEqual([['fleet', 'board', 'go', 'ENG-1-1'], ['fleet', 'board', 'go', 'ENG-1-1']])
})

test('with nothing waiting the band is the one line', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  const clock = mock.clock(on)
  const calm = chiefSees()
  calm.agents[1]!.awaiting_go = null
  calm.agents[2]!.context = 40
  board(on, { now: calm })

  await $.session.start(start)
  await clock.advance(3_000)
  const band = await $.ui.mount({ plugin: 'fleet', surface: 'desktop', component: 'AbovePrompt', props: { hasSurvey: false, bodyColumns: 120 } as never })
  const said = (await band.findAll({ type: 'Text' })).map(t => t.text).join('|')
  expect(said).not.toContain('▲')
  expect(said).toContain('1 idle')
  expect(await band.find({ key: 'fleet-open' })).toBeDefined()
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

for (const surface of ['desktop', 'mobile'] as const) {
  test(`on ${surface} the pane is drawn natively from the board, with its buttons`, async ($, on) => {
    mock.env(on, { FLEET_DB: '/tmp/fleet.db', HOME: '/home/me' })
    const clock = mock.clock(on)
    const { gone, handed, followed } = board(on, { now: chiefSees() })

    await $.session.start({ ...start, surface })
    await clock.advance(3_000)
    const ui = await $.ui.mount({ plugin: 'fleet', surface, component: 'Pane', requestId: 'fleet', props: { bodyColumns: 90 } as never })

    // The crew as a graph, described for a reader that cannot see it: on
    // the desktop by default, off on a phone.
    const svg = (await ui.findAll({ type: 'Svg' })).find(e => String(e.props?.alt).startsWith('The crew'))
    if (surface === 'desktop') {
      expect(String(svg?.props?.alt)).toContain('billing ◇ needs a go')
      expect(String(svg?.props?.source)).toContain('<svg')
    } else {
      expect(svg).toBeUndefined()
    }
    // A context bar per agent.
    expect((await ui.findAll({ type: 'Svg' })).some(e => e.props?.alt === '84% of its context used')).toBe(true)

    const said = (await ui.findAll({ type: 'Text' })).map(t => t.text).join(' | ')
    expect(said).toContain('2 agents')
    expect(said).toContain('◇ needs a go')
    expect(said).toContain('● slack_send_message')
    expect(said).toContain('84% context')
    expect(said).toContain('claude attach 5cba7f8f')
    expect(said).toContain('needs the flag')
    expect(said).toContain('plan: add the column')
    expect(said).toContain('NEEDS YOU')
    expect(said).toContain('is at 84% context and will compact soon.')
    expect(said).toContain('blocked · 1')
    expect(said.indexOf('NEEDS YOU')).toBeLessThan(said.indexOf('CREW'))
    expect(said.indexOf('CREW')).toBeLessThan(said.indexOf('TASKS'))
    expect(said.indexOf('blocked · 1')).toBeLessThan(said.indexOf('queued · 1'))
    expect(said.indexOf('TASKS')).toBeLessThan(said.indexOf('LATEST'))
    expect((await ui.find({ type: 'Markdown' }))?.props?.text).toBe('### Fleet')

    // Nothing of the terminal's: no fleet view behind it.
    expect(await ui.find({ type: 'Client' })).toBeUndefined()
    await clock.advance(3_000)
    expect(followed).toEqual([])

    await ui.press({ key: 'go-billing' })
    expect(gone).toEqual([['fleet', 'board', 'go', 'ENG-1-1']])
    await ui.press({ key: 'handoff-accounts' })
    expect(handed).toEqual([['fleet', 'handoff', 'accounts']])
  })
}

test("the band opens the fleet pane while it is closed, and says /fleet once it is open", async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  const clock = mock.clock(on)
  const { opened } = board(on, { now: chiefSees() })

  await $.session.start(start)
  await clock.advance(3_000)
  const band = await $.ui.mount({ plugin: 'fleet', surface: 'desktop', component: 'AbovePrompt', props: { hasSurvey: false, bodyColumns: 120 } as never })
  await band.press({ key: 'fleet-open' })
  expect(opened).toEqual(['fleet'])

  const after = await $.ui.mount({ plugin: 'fleet', surface: 'desktop', component: 'AbovePrompt', props: { hasSurvey: false, bodyColumns: 120 } as never })
  expect(await after.find({ key: 'fleet-open' })).toBeUndefined()
  expect(await after.find({ text: '/fleet' })).toBeDefined()
})

test('the band offers the pane again when the desktop put it away without closing it', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  const clock = mock.clock(on)
  board(on, { now: chiefSees() })
  let panes = [{ id: 'fleet', title: 'fleet', isShown: true, isFocused: false, isPlaced: true }]
  on('ui.panes', () => ({ value: panes }))

  await $.session.start(start)
  await clock.advance(3_000)
  const band = await $.ui.mount({ plugin: 'fleet', surface: 'desktop', component: 'AbovePrompt', props: { hasSurvey: false, bodyColumns: 120 } as never })
  expect(await band.find({ key: 'fleet-open' })).toBeUndefined()

  // No ui.close: the panel was put away, and the engine lists no pane.
  panes = []
  await clock.advance(3_000)
  const later = await $.ui.mount({ plugin: 'fleet', surface: 'desktop', component: 'AbovePrompt', props: { hasSurvey: false, bodyColumns: 120 } as never })
  expect(await later.find({ key: 'fleet-open' })).toBeDefined()
})

/** The live graph in a mounted pane, if it is drawn. */
async function crewGraph(ui: { findAll: (q: { type: 'Svg' }) => Promise<{ props?: Record<string, unknown> }[]> }) {
  return (await ui.findAll({ type: 'Svg' })).find(e => String(e.props?.alt).startsWith('The crew'))
}

test('the Graph button shows and hides the graph, remembered for the workspace', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db', HOME: '/home/me' })
  mock.store(on)
  const clock = mock.clock(on)
  board(on, { now: chiefSees() })

  await $.session.start({ ...start, surface: 'desktop' })
  await clock.advance(3_000)
  const pane = { plugin: 'fleet', component: 'Pane', requestId: 'fleet', props: { bodyColumns: 90 } as never } as const
  const ui = await $.ui.mount({ ...pane, surface: 'desktop' })
  expect(await crewGraph(ui)).toBeDefined()
  await ui.press({ key: 'fleet-graph' })
  await clock.advance(10)
  expect(await crewGraph(ui)).toBeUndefined()
  await ui.unmount()
  // Drawn again later, it is still off.
  const again = await $.ui.mount({ ...pane, surface: 'desktop' })
  expect(await crewGraph(again)).toBeUndefined()

  // The phone keeps its own: off until pressed there.
  const phone = await $.ui.mount({ ...pane, surface: 'mobile' })
  expect(await crewGraph(phone)).toBeUndefined()
  await phone.press({ key: 'fleet-graph' })
  await clock.advance(10)
  expect(await crewGraph(phone)).toBeDefined()
  expect(await crewGraph(again)).toBeUndefined()
})

test('a new message travels the graph as a dot for a minute; what was there before does not', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db', HOME: '/home/me' })
  mock.store(on)
  const clock = mock.clock(on)
  const seen = { now: chiefSees() }
  board(on, seen)

  await $.session.start({ ...start, surface: 'desktop' })
  await clock.advance(3_000)
  const pane = { plugin: 'fleet', surface: 'desktop', component: 'Pane', requestId: 'fleet', props: { bodyColumns: 90 } as never } as const
  const ui = await $.ui.mount(pane)
  const before = String((await crewGraph(ui))?.props?.source)
  expect(before).not.toContain('class="dot')
  expect(before).toContain('quiet · no messages')

  const next = chiefSees()
  next.events.unshift(
    { key: 'e2', kind: 'message', from: 'chief', to: 'accounts', task: 'ENG-1-2', summary: 'the flag is on', notice: null },
    { key: 'e3', kind: 'message', from: 'accounts', to: 'billing', task: null, summary: 'column shared', notice: null },
    { key: 'e4', kind: 'message', from: 'billing', to: 'chief', task: 'ENG-1-1', summary: 'plan updated', notice: null },
  )
  seen.now = next
  await clock.advance(3_000)
  const g = await crewGraph(ui)
  const live = String(g?.props?.source)
  expect(live).toContain('class="dot down"')
  expect(live).toContain('class="dot up"')
  expect(live).toContain('@keyframes')
  expect(live).toContain('live · 3 messages in the last minute')
  expect(String(g?.props?.alt)).toContain('chief to accounts, accounts to billing, billing to chief')
  // billing needs a go: its node pulses.
  expect(live).toContain('class="pulse"')

  await clock.advance(61_000)
  const later = String((await crewGraph(ui))?.props?.source)
  expect(later).not.toContain('class="dot')
})
