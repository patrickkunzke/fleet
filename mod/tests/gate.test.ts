import { test, expect, mock } from 'claude-code/testing'
import type { On } from 'claude-code'
import { writes } from '../hooks/register.ts'

type Gate = { agent: string | null; role: string | null; tasks: string[]; approved: boolean }

/** A board whose gate answers with `state`, which `--user-approves` approves. */
/** How many prompts the session has had, as `$.session.turns()` answers. */
const turns = { n: 1 }

function board(on: On, state: Gate) {
  const asked: string[][] = []
  on('process.run', ($, e) => {
    const argv = [...e.argv]
    let stdout = JSON.stringify({ agent: state.agent, text: null, waiting: 0 })
    if (argv[2] === 'gate') {
      asked.push(argv)
      if (argv.includes('--user-approves') && state.role === 'worker') state.approved = true
      stdout = JSON.stringify(state)
    }
    return { value: { exitCode: 0, stdout, stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  on('session.id', () => ({ value: 'sid-1' }))
  on('session.turns', () => ({ value: turns.n }))
  on('session.start', ($, e) => ({ cwd: e.cwd }))
  on('ui.status', () => ({ value: undefined }))
  on('prompt.submit', ($, e) => ({ text: e.text }))
  on('tool.call', () => ({ result: 'ran' }))
  return asked
}

const start = { cwd: '/repo', surface: 'terminal', isInteractive: true } as const
const worker = (): Gate => ({ agent: 'accounts-svc', role: 'worker', tasks: ['ENG-1-1'], approved: false })

test('a worker without a go may read but not edit', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  mock.clock(on)
  board(on, worker())
  await $.session.start(start)

  const edit = await $.tool.call({ tool: 'Edit', file_path: '/repo/a.ts', old_string: 'a', new_string: 'b' })
  expect(edit.deny).toBeDefined()
  expect(edit.deny).toContain('ENG-1-1 has no go-ahead yet')

  const read = await $.tool.call({ tool: 'Bash', command: 'git status && cat src/a.ts' })
  expect(read.result).toBe('ran')

  const sneak = await $.tool.call({ tool: 'Bash', command: "sed -i '' 's/a/b/' src/a.ts" })
  expect(sneak.deny).toBeDefined()
})

test('with a go, the worker edits', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  mock.clock(on)
  board(on, { ...worker(), approved: true })
  await $.session.start(start)

  const edit = await $.tool.call({ tool: 'Write', file_path: '/repo/a.ts', content: 'x' })
  expect(edit.result).toBe('ran')
})

test('the user typing in the pane is a go; the brief and a message fleet typed there are not', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  mock.clock(on)
  const asked = board(on, worker())
  await $.session.start(start)

  // fleet's brief, given on the command line, arrives as typed: first.
  turns.n = 0
  await $.prompt.submit({ text: 'Your task is ENG-1-1', origin: { kind: 'composer' } } as never)
  turns.n = 1
  await $.prompt.submit({ text: '[fleet · billing-svc] go ahead', origin: { kind: 'composer' } } as never)
  await $.prompt.submit({ text: '/model', origin: { kind: 'composer' } } as never)
  expect(asked.filter(a => a.includes('--user-approves'))).toEqual([])

  await $.prompt.submit({ text: 'looks good, go', origin: { kind: 'composer' } } as never)
  expect(asked.filter(a => a.includes('--user-approves')).length).toBe(1)
  const edit = await $.tool.call({ tool: 'Edit', file_path: '/repo/a.ts', old_string: 'a', new_string: 'b' })
  expect(edit.result).toBe('ran')
})

test('a worker cannot give itself the go', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  mock.clock(on)
  board(on, { ...worker(), approved: true })
  await $.session.start(start)

  const self = await $.tool.call({ tool: 'Bash', command: 'fleet board go ENG-1-1' })
  expect(self.deny).toBeDefined()
  expect(self.deny).toContain('not from accounts-svc')
})

test('the chief changes no files, and may give a go', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  mock.clock(on)
  board(on, { agent: 'chief', role: 'chief', tasks: [], approved: true })
  await $.session.start(start)

  const bash = await $.tool.call({ tool: 'Bash', command: 'cat > src/a.ts <<EOF\nx\nEOF' })
  expect(bash.deny).toBeDefined()
  expect(bash.deny).toContain('chief of staff does not change files')

  const go = await $.tool.call({ tool: 'Bash', command: "fleet board go ENG-1-1 'go, billing first'" })
  expect(go.result).toBe('ran')
  const msg = await $.tool.call({ tool: 'Bash', command: `fleet board msg chief accounts-svc "a -> b, then c > d"` })
  expect(msg.result).toBe('ran')
})

test('a retired session changes nothing more', async ($, on) => {
  mock.env(on, { FLEET_DB: '/tmp/fleet.db' })
  mock.clock(on)
  board(on, { agent: 'accounts-svc', role: 'retired', tasks: [], approved: false })
  await $.session.start(start)

  const edit = await $.tool.call({ tool: 'Write', file_path: '/repo/a.ts', content: 'x' })
  expect(edit.deny).toContain('was retired')
  const read = await $.tool.call({ tool: 'Bash', command: 'git status' })
  expect(read.result).toBe('ran')
})

test('which commands write', () => {
  const yes = [
    'echo x > a.ts',
    'echo x >> notes.md',
    'cat <<EOF > src/a.ts',
    "sed -i '' 's/a/b/' a.ts",
    'sed -E -i.bak s/a/b/ a.ts',
    'perl -pi -e s/a/b/ a.ts',
    'echo x | tee a.ts',
    'rm src/a.ts',
    'mv a.ts b.ts',
    'git commit -m wip',
    'git -C ../billing checkout -b feat',
    'cd src && touch a.ts',
    'find . -name x | xargs rm',
  ]
  const no = [
    'git status',
    'git diff HEAD~1',
    'cargo test 2>&1 | tail',
    'ls > /dev/null',
    'echo x > /tmp/scratch.md',
    'echo x | tee /tmp/x',
    'grep -rn "a > b" src',
    `fleet board msg chief x 'a -> b'`,
    'sed -n 1,20p a.ts',
    'rm -rf /tmp/fleet-scratch',
    'npm run build 2> /dev/null',
  ]
  for (const c of yes) expect([c, writes(c)]).toEqual([c, true])
  for (const c of no) expect([c, writes(c)]).toEqual([c, false])
})
