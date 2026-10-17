import { test, expect, mock } from 'claude-code/testing'
import type { On } from 'claude-code'

const start = { cwd: '/w/acme', surface: 'terminal', isInteractive: true } as const

/** An ordinary session, with fleet's plugin installed and no board yet. */
function session(on: On, onRun?: (argv: string[]) => Promise<void>) {
  const ran: string[][] = []
  const set: Record<string, string | undefined> = {}
  const submitted: string[] = []
  const registered: string[] = []
  const commands: string[] = []
  on('process.run', async ($, e) => {
    const argv = [...e.argv]
    ran.push(argv)
    if (onRun) await onRun(argv)
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
  on('command.register', ($, e) => {
    commands.push(e.name)
    return { value: undefined }
  })
  on('tool.register', ($, e) => {
    registered.push(e.name)
    return { value: undefined }
  })
  on('prompt.submit', ($, e) => {
    submitted.push(e.text)
    return { text: e.text }
  })
  on('ui.status', () => ({ value: undefined }))
  const opened: string[] = []
  on('ui.open', ($, e) => {
    opened.push(e.id)
    return { value: { isPlaced: true as const } }
  })
  on('prompt.read', () => ({ value: { text: '', cursor: 0 } }))
  on('tool.call', () => ({ result: 'ran' }))
  on('tool.check', () => ({ decision: 'ask' as const }))
  return { ran, set, submitted, registered, commands, opened }
}

test('an ordinary session with the plugin runs nothing until /fleet start', async ($, on) => {
  mock.env(on, { PATH: '/usr/bin', FLEET_BIN: 'fleet' })
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
  mock.env(on, { PATH: '/usr/bin', FLEET_BIN: 'fleet' })
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
  mock.env(on, { PATH: '/usr/bin', FLEET_BIN: 'fleet' })
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

test("an adopted chief's brief is put back at the head of a compacted conversation", async ($, on) => {
  mock.env(on, { PATH: '/usr/bin', FLEET_BIN: 'fleet' })
  mock.clock(on)
  session(on)
  const summary = { role: 'user' as const, text: 'Summary: planned LIME-1, citrus is on it.', toolUses: [] }
  const said = [{ role: 'user' as const, text: 'plan LIME-1', toolUses: [] }]
  on('session.compact', () => ({ messages: [summary] }))

  await $.session.start(start)
  const before = await $.session.compact({ trigger: 'manual', messages: said })
  expect(before).toEqual({ messages: [summary] })

  await $.command.run({ command: 'fleet', args: 'start' } as never)
  const after = await $.session.compact({ trigger: 'auto', messages: said })
  if (!('messages' in after) || !after.messages) throw new Error('compaction was skipped')
  expect(after.messages[0].text).toContain('You are the chief of staff…')
  expect(after.messages[1]).toEqual(summary)
})

test('/fleet start fetches the fleet that matches the plugin when it has none', async ($, on) => {
  mock.env(on, { PATH: '/usr/bin' })
  mock.clock(on)
  const ran: string[][] = []
  let fetched = false
  on('process.run', ($, e) => {
    const argv = [...e.argv]
    ran.push(argv)
    const reply = (exitCode: number, stdout = '') => ({ value: { exitCode, stdout, stderr: '', isStdoutTruncated: false, isStderrTruncated: false } })
    if (argv[0] === 'test') return reply(fetched ? 0 : 1)
    if (argv[0] === 'sh' && argv[1].endsWith('/scripts/fetch-fleet.sh')) {
      fetched = true
      return reply(0)
    }
    if (argv[1] === 'chief') return reply(0, JSON.stringify({ board: '/b/fleet.db', run: 1, bin: null, brief: 'brief' }))
    return reply(0, JSON.stringify({ agent: 'chief', text: null, waiting: 0, awaiting_go: null }))
  })
  for (const [event, value] of [['env.set', undefined], ['session.id', 'sid-mine'], ['session.root', '/w/acme'], ['command.register', undefined], ['tool.register', undefined], ['ui.status', undefined], ['ui.open', { isPlaced: true }]] as const) {
    on(event, () => ({ value }) as never)
  }
  on('session.start', ($, e) => ({ cwd: e.cwd }))
  on('prompt.submit', ($, e) => ({ text: e.text }))

  await $.session.start(start)
  const said = await $.command.run({ command: 'fleet', args: 'start' } as never)

  const adopt = ran.find(argv => argv[1] === 'chief')
  expect(adopt?.[0]).toMatch(/\/bin\/fleet$/)
  expect(fetched).toBe(true)
  expect(said.text).toContain('chief of /w/acme')
})

test('a session the desktop app hosts starts the mod once the app attaches; a -p run never does', async ($, on) => {
  mock.env(on, { PATH: '/usr/bin', FLEET_BIN: 'fleet', FLEET_DB: '/tmp/fleet.db' })
  const clock = mock.clock(on)
  const { ran, commands } = session(on)
  let surfaces: ('desktop')[] = []
  on('session.surfaces', () => ({ value: surfaces }))

  // Through the SDK, as the desktop app and `claude -p` both start it.
  await $.session.start({ cwd: '/w/acme', surface: null, isInteractive: false })
  await clock.advance(10_000)
  expect(commands).toEqual([])
  expect(ran).toEqual([])

  surfaces = ['desktop']
  await clock.advance(1_000)
  expect(commands).toEqual(['fleet'])
  await clock.advance(3_000)
  expect(ran.some(argv => argv[2] === 'inbox')).toBe(true)
})

for (const surface of ['terminal', 'desktop'] as const) {
  test(`on ${surface}, a workspace listed in auto-open offers to start a fleet, and one press does`, async ($, on) => {
    mock.env(on, { PATH: '/usr/bin', FLEET_BIN: 'fleet', HOME: '/w' })
    const clock = mock.clock(on)
    // fleet takes its time to answer: the band is looked at meanwhile.
    let answer = () => {}
    let asked = () => {}
    const isAsked = new Promise<void>(resolve => { asked = resolve })
    const { ran, submitted, opened } = session(on, async argv => {
      if (argv[1] !== 'chief') return
      asked()
      await new Promise<void>(resolve => { answer = resolve })
    })
    on('fs.read', ($, e) => ({ value: e.path === '/w/.claude-fleet/auto-open' ? '# fleets\n~/acme\n' : '' }) as never)

    // Beneath the plugin, the engine's own band: nothing.
    on('ui.render', { component: 'AbovePrompt' }, () => ({ type: 'Box', props: {}, children: [] }) as never)
    await $.session.start({ ...start, surface })
    const band = await $.ui.mount({ plugin: 'fleet', surface, component: 'AbovePrompt', props: { hasSurvey: false, bodyColumns: 100 } as never })
    expect(await band.find({ key: 'fleet-start' })).toBeDefined()
    expect(ran).toEqual([])

    const pressed = band.press({ key: 'fleet-start' })
    await isAsked
    // While it starts, the band says so and offers no second press.
    const meanwhile = await $.ui.mount({ plugin: 'fleet', surface, component: 'AbovePrompt', props: { hasSurvey: false, bodyColumns: 100 } as never })
    expect(await meanwhile.find({ text: /Starting the fleet/ })).toBeDefined()
    expect(await meanwhile.find({ key: 'fleet-start' })).toBeUndefined()
    answer()
    await pressed
    // The rest of the start runs on after the press: wait for its pane.
    for (let i = 0; i < 1_000 && opened.length === 0; i++) await Promise.resolve()
    expect(ran.some(argv => argv[1] === 'chief' && argv[2] === '--adopt')).toBe(true)
    expect(opened).toEqual(['fleet'])
    const again = await $.ui.mount({ plugin: 'fleet', surface, component: 'AbovePrompt', props: { hasSurvey: false, bodyColumns: 100 } as never })
    expect(await again.find({ key: 'fleet-start' })).toBeUndefined()
    // The chief's brief follows, as after /fleet start.
    await clock.advance(1_000)
    expect(submitted).toEqual(['You are the chief of staff…'])
  })
}

test('a workspace not listed in auto-open is offered nothing', async ($, on) => {
  mock.env(on, { PATH: '/usr/bin', FLEET_BIN: 'fleet', HOME: '/w' })
  mock.clock(on)
  const { ran } = session(on)
  on('fs.read', () => ({ value: '~/elsewhere\n' }) as never)
  on('ui.render', { component: 'AbovePrompt' }, () => ({ type: 'Box', props: {}, children: [] }) as never)

  await $.session.start(start)
  const band = await $.ui.mount({ plugin: 'fleet', surface: 'terminal', component: 'AbovePrompt', props: { hasSurvey: false, bodyColumns: 100 } as never })
  expect(await band.find({ key: 'fleet-start' })).toBeUndefined()
  // Only the check that the two are not the same directory spelled apart.
  expect(ran.map(argv => argv[0])).toEqual(['sh'])
})

test('/fleet start opens the fleet pane by itself', async ($, on) => {
  mock.env(on, { PATH: '/usr/bin', FLEET_BIN: 'fleet' })
  mock.clock(on)
  const { opened } = session(on)

  await $.session.start(start)
  const said = await $.command.run({ command: 'fleet', args: 'start' } as never)
  expect(said.text).toContain('the fleet pane shows the crew')
  expect(opened).toEqual(['fleet'])
})
