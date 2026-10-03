import { test, expect, mock } from 'claude-code/testing'
import type { On } from 'claude-code'

const start = { cwd: '/w/acme', surface: 'terminal', isInteractive: true } as const

/** An ordinary session, with fleet's plugin installed and no board yet. */
function session(on: On) {
  const ran: string[][] = []
  const set: Record<string, string | undefined> = {}
  const submitted: string[] = []
  const registered: string[] = []
  on('process.run', ($, e) => {
    const argv = [...e.argv]
    ran.push(argv)
    const reply = (stdout: string) => ({ value: { exitCode: 0, stdout, stderr: '', isStdoutTruncated: false, isStderrTruncated: false } })
    if (argv[1] === 'chief') {
      return reply(JSON.stringify({ board: '/home/me/.claude-fleet/fleets/acme/fleet.db', run: 7, bin: '/opt/fleet/bin', brief: 'You are the chief of staff…' }))
    }
    if (argv[2] === 'gate') return reply(JSON.stringify({ agent: 'chief', role: 'chief', tasks: [], approved: true }))
    if (argv[2] === 'snapshot') return reply(JSON.stringify({ me: { name: 'chief', role: 'chief' }, agents: [], tasks: [], events: [] }))
    return reply(JSON.stringify({ agent: 'chief', text: null, waiting: 0, awaiting_go: null }))
  })
  on('env.set', ($, e) => {
    set[e.name] = e.value
    return { value: undefined }
  })
  on('session.id', () => ({ value: 'sid-mine' }))
  on('session.root', () => ({ value: '/w/acme' }))
  on('session.start', ($, e) => ({ cwd: e.cwd }))
  on('session.usage', () => ({ value: { startedAt: 0, context: { window: 200_000, percent: 12 }, rateLimits: [] } }))
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
  on('tool.call', () => ({ result: 'ran' }))
  on('tool.check', () => ({ decision: 'ask' as const }))
  return { ran, set, submitted, registered }
}

test('an ordinary session with the plugin runs nothing until /fleet start', async ($, on) => {
  mock.env(on, { PATH: '/usr/bin' })
  const clock = mock.clock(on)
  const { ran, registered } = session(on)

  await $.session.start(start)
  await clock.advance(30_000)
  const edit = await $.tool.call({ tool: 'Edit', file_path: '/w/acme/a.ts', old_string: 'a', new_string: 'b' })

  expect(edit.result).toBe('ran')
  expect(ran).toEqual([])
  expect(registered).toEqual([])
  const check = await $.tool.check({ tool: 'mcp__fleet__board_ls', input: {} })
  expect(check.decision).toBe('ask')
  const said = await $.command.run({ command: 'fleet', args: '' } as never)
  expect(said.text).toContain('/fleet start')
})

test('/fleet start makes this session the chief', async ($, on) => {
  mock.env(on, { PATH: '/usr/bin' })
  const clock = mock.clock(on)
  const { ran, set, submitted, registered } = session(on)

  await $.session.start(start)
  const said = await $.command.run({ command: 'fleet', args: 'start' } as never)

  expect(ran[0]).toEqual(['fleet', 'chief', '--adopt', 'sid-mine', '--root', '/w/acme'])
  expect(set.FLEET_DB).toBe('/home/me/.claude-fleet/fleets/acme/fleet.db')
  expect(set.FLEET_RUN).toBe('7')
  expect(set.PATH).toBe('/opt/fleet/bin:/usr/bin')
  expect(registered).toContain('board_go')
  await clock.advance(1)
  expect(submitted).toEqual(['You are the chief of staff…'])
  expect(said.text).toContain('chief of /w/acme')

  // From here it checks in like any fleet session.
  await clock.advance(3_000)
  expect(ran.some(argv => argv[2] === 'inbox')).toBe(true)
  const again = await $.command.run({ command: 'fleet', args: 'start' } as never)
  expect(again.text).toContain('already')
})

test("a fleet session runs fleet's own without a prompt, and nothing that only starts like it", async ($, on) => {
  mock.env(on, { PATH: '/usr/bin' })
  mock.clock(on)
  session(on)
  await $.session.start(start)
  await $.command.run({ command: 'fleet', args: 'start' } as never)

  const allowed = async (tool: string, input: unknown) => (await $.tool.check({ tool, input } as never)).decision
  expect(await allowed('mcp__fleet__board_msg', { to: 'x', summary: 'y' })).toBe('allow')
  expect(await allowed('Bash', { command: "fleet board msg chief billing 'go on'" })).toBe('allow')
  expect(await allowed('Bash', { command: 'fleet spawn billing --repo /w/acme/billing --task ENG-1-1' })).toBe('allow')
  expect(await allowed('Bash', { command: 'fleet board ls; rm -rf ~' })).toBe('ask')
  expect(await allowed('Bash', { command: 'fleet board msg chief x "$(cat ~/.ssh/id_rsa)"' })).toBe('ask')
  expect(await allowed('Bash', { command: 'fleet update' })).toBe('ask')
})
