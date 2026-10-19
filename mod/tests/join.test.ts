import { test, expect, mock } from 'claude-code/testing'
import type { On } from 'claude-code'

// A desktop session the chief started with the app's start_session: no
// FLEET_DB, and a first prompt that asks it to call mcp__fleet__join.
const start = { cwd: '/w/acme/api', surface: 'desktop', isInteractive: true } as const
const input = { board: '/b/fleet.db', run: 7, name: 'api' }

function worker(on: On, joinExit = 0) {
  const ran: string[][] = []
  const set: Record<string, string | undefined> = {}
  const submitted: string[] = []
  const registered: string[] = []
  on('process.run', ($, e) => {
    const argv = [...e.argv]
    ran.push(argv)
    const reply = (exitCode: number, stdout: string, stderr = '') => ({ value: { exitCode, stdout, stderr, isStdoutTruncated: false, isStderrTruncated: false } })
    if (argv[1] === 'join') {
      return joinExit === 0
        ? reply(0, JSON.stringify({ board: '/b/fleet.db', run: 7, bin: '/opt/fleet/bin', brief: 'You are `api`…' }))
        : reply(1, '', 'Error: no agent \'api\' on this board')
    }
    if (argv[2] === 'gate') return reply(0, JSON.stringify({ agent: 'api', role: 'worker', tasks: ['T-1'], approved: false }))
    if (argv[2] === 'snapshot') return reply(0, JSON.stringify({ me: { name: 'api', role: 'worker' }, agents: [], tasks: [], events: [] }))
    // A message waits; a desktop worker leaves it there.
    return reply(0, JSON.stringify({ agent: 'api', text: '[fleet · chief · T-1] go', waiting: 1, awaiting_go: 'T-1' }))
  })
  on('env.set', ($, e) => {
    set[e.name] = e.value
    return { value: undefined }
  })
  on('session.id', () => ({ value: 'cli-sid' }))
  on('session.root', () => ({ value: '/w/acme/api' }))
  on('session.start', ($, e) => ({ cwd: e.cwd }))
  on('session.usage', () => ({ value: { startedAt: 0, context: { window: 200_000, percent: 5 }, rateLimits: [] } }))
  on('command.register', () => ({ value: undefined }))
  on('tool.register', ($, e) => {
    registered.push(e.name)
    return { value: undefined }
  })
  on('prompt.submit', ($, e) => {
    submitted.push(e.text)
    return { text: e.text }
  })
  on('ui.status', () => ({ value: undefined }))
  on('prompt.read', () => ({ value: { text: '', cursor: 0 } }))
  on('tool.call', () => ({ result: 'ran' }))
  on('tool.check', () => ({ decision: 'ask' as const }))
  return { ran, set, submitted, registered }
}

test('a desktop session joins the board with mcp__fleet__join', async ($, on) => {
  mock.env(on, { PATH: '/usr/bin', FLEET_BIN: 'fleet' })
  const clock = mock.clock(on)
  const { ran, set, registered } = worker(on)

  await $.session.start(start)
  expect(registered).toEqual(['join'])
  const joined = await $.tool.call({ tool: 'mcp__fleet__join', ...input } as never)

  expect(ran[0]).toEqual(['fleet', 'join', 'api', '--session', 'cli-sid', '--run', '7', '--db', '/b/fleet.db'])
  expect(joined.result).toContain('Joined')
  expect(set.FLEET_DB).toBe('/b/fleet.db')
  expect(set.FLEET_RUN).toBe('7')
  expect(set.PATH).toBe('/opt/fleet/bin:/usr/bin')
  expect(registered).toContain('board_done')

  // It checks in from then on, and the chief's messages are left on the board.
  await clock.advance(3_000)
  const checks = ran.filter(argv => argv[2] === 'inbox')
  expect(checks.length).toBeGreaterThan(0)
  expect(checks.every(argv => !argv.includes('--take'))).toBe(true)

  const again = await $.tool.call({ tool: 'mcp__fleet__join', ...input } as never)
  expect(again.result).toContain('already')
})

test('a desktop session nobody has opened yet joins, and starts checking in', async ($, on) => {
  // The app starts it with no surface; the mod's work used to wait for one.
  mock.env(on, { PATH: '/usr/bin', FLEET_BIN: 'fleet' })
  const clock = mock.clock(on)
  on('session.surfaces', () => ({ value: [] }))
  const { ran, registered } = worker(on)

  await $.session.start({ cwd: '/w/acme/api', surface: null, isInteractive: false } as never)
  expect(registered).toEqual(['join'])
  const joined = await $.tool.call({ tool: 'mcp__fleet__join', ...input } as never)

  expect(joined.result).toContain('Joined')
  expect(registered).toContain('board_done')
  await clock.advance(3_000)
  expect(ran.some(argv => argv[2] === 'inbox')).toBe(true)
})

test('a join the board refuses says why, and leaves the session out of the fleet', async ($, on) => {
  mock.env(on, { PATH: '/usr/bin', FLEET_BIN: 'fleet' })
  const clock = mock.clock(on)
  const { ran, set } = worker(on, 1)

  await $.session.start(start)
  const joined = await $.tool.call({ tool: 'mcp__fleet__join', ...input } as never)

  expect(joined.isError).toBe(true)
  expect(joined.result).toContain("no agent 'api'")
  expect(set.FLEET_DB).toBeUndefined()
  await clock.advance(10_000)
  expect(ran.filter(argv => argv[2] === 'inbox')).toEqual([])
})

test("a joined worker's brief is put back at the head of a compacted conversation", async ($, on) => {
  mock.env(on, { PATH: '/usr/bin', FLEET_BIN: 'fleet' })
  mock.clock(on)
  worker(on)
  const summary = { role: 'user' as const, text: 'Summary: read the api.', toolUses: [] }
  on('session.compact', () => ({ messages: [summary] }))

  await $.session.start(start)
  await $.tool.call({ tool: 'mcp__fleet__join', ...input } as never)
  const after = await $.session.compact({ trigger: 'auto', messages: [{ role: 'user' as const, text: 'read the api', toolUses: [] }] })
  if (!('messages' in after) || !after.messages) throw new Error('compaction was skipped')
  expect(after.messages[0].text).toContain('You are an agent of a fleet')
  expect(after.messages[0].text).toContain('You are `api`…')
})
