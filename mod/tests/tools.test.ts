import { test, expect, mock } from 'claude-code/testing'
import type { On } from 'claude-code'

const start = { cwd: '/repo', surface: 'terminal', isInteractive: true } as const

/** A board where this session is `me`, as `role`; `fails` makes a command fail. */
function board(on: On, me: string, role: string, fails = false) {
  const ran: string[][] = []
  const registered: string[] = []
  on('process.run', ($, e) => {
    const argv = [...e.argv]
    const reply = (stdout: string, exitCode = 0, stderr = '') => ({
      value: { exitCode, stdout, stderr, isStdoutTruncated: false, isStderrTruncated: false },
    })
    if (argv[2] === 'gate') return reply(JSON.stringify({ agent: me, role, tasks: [], approved: true }))
    if (argv[2] === 'inbox') return reply(JSON.stringify({ agent: me, text: null, waiting: 0, awaiting_go: null }))
    if (argv[2] === 'snapshot') return reply(JSON.stringify({ me: { name: me, role }, agents: [], tasks: [], events: [] }))
    ran.push(argv)
    return fails ? reply('', 1, 'unknown task ENG-404') : reply(`ok: ${argv.slice(1).join(' ')}`)
  })
  on('tool.register', ($, e) => {
    registered.push(e.name)
    return { value: undefined }
  })
  on('session.id', () => ({ value: 'sid-1' }))
  on('session.start', ($, e) => ({ cwd: e.cwd }))
  on('session.usage', () => ({ value: { startedAt: 0, context: { window: 200_000, percent: 10 }, rateLimits: [] } }))
  on('command.register', () => ({ value: undefined }))
  on('ui.status', () => ({ value: undefined }))
  return { ran, registered }
}

test('a fleet session gets the board as tools', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  mock.clock(on)
  const { registered } = board(on, 'billing', 'worker')
  await $.session.start(start)
  expect(registered).toContain('board_start')
  expect(registered).toContain('board_msg')
  expect(registered).toContain('board_go')
  expect(registered).toContain('spawn')
})

test("a worker's message goes out under its own name, whatever it writes", async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  mock.clock(on)
  const { ran } = board(on, 'billing', 'worker')
  await $.session.start(start)

  const sent = await $.tool.call({ tool: 'mcp__fleet__board_msg', to: 'chief', summary: 'plan: add the column', body: "it's 'quoted' and\nmulti-line", task: 'ENG-1-1' } as never)
  expect(ran.at(-1)).toEqual(['fleet', 'board', 'msg', 'billing', 'chief', 'plan: add the column', '--body', "it's 'quoted' and\nmulti-line", '--task', 'ENG-1-1'])
  expect(sent.result).toContain('ok: board msg billing chief')

  await $.tool.call({ tool: 'mcp__fleet__board_start', task: 'ENG-1-1' } as never)
  expect(ran.at(-1)).toEqual(['fleet', 'board', 'start', 'ENG-1-1'])
})

test("the chief's tools are the chief's", async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  mock.clock(on)
  const { ran } = board(on, 'billing', 'worker')
  await $.session.start(start)

  const go = await $.tool.call({ tool: 'mcp__fleet__board_go', task: 'ENG-1-1' } as never)
  expect(go.deny).toContain("board_go is the chief's")
  const spawn = await $.tool.call({ tool: 'mcp__fleet__spawn', name: 'x', repo: '/w/x' } as never)
  expect(spawn.deny).toBeDefined()
  expect(ran).toEqual([])
})

test('the chief gives a go and starts agents with them', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  mock.clock(on)
  const { ran } = board(on, 'chief', 'chief')
  await $.session.start(start)

  await $.tool.call({ tool: 'mcp__fleet__board_go', task: 'ENG-1-1', message: 'go, billing first' } as never)
  expect(ran.at(-1)).toEqual(['fleet', 'board', 'go', 'ENG-1-1', 'go, billing first'])
  await $.tool.call({ tool: 'mcp__fleet__board_add', task: 'ENG-1-2', repo: '/w/billing', title: 'consume it', deps: ['ENG-1-1'] } as never)
  expect(ran.at(-1)).toEqual(['fleet', 'board', 'add', 'ENG-1-2', '/w/billing', 'consume it', '--dep', 'ENG-1-1'])
  await $.tool.call({ tool: 'mcp__fleet__spawn', name: 'billing', repo: '/w/billing', task: 'ENG-1-2' } as never)
  expect(ran.at(-1)).toEqual(['fleet', 'spawn', 'billing', '--repo', '/w/billing', '--task', 'ENG-1-2'])
})

test('what fleet refuses comes back as an error the model reads', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  mock.clock(on)
  board(on, 'billing', 'worker', true)
  await $.session.start(start)

  const done = await $.tool.call({ tool: 'mcp__fleet__board_done', task: 'ENG-404' } as never)
  expect(done.isError).toBe(true)
  expect(String(done.result)).toContain('unknown task ENG-404')
})
