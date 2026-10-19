// The fleet on a surface that draws natively: the desktop app and the phone.
// The terminal shows the fleet tab itself, drawn by fleet as text
// (view.tsx); a desktop window would show that as a grid of characters that
// wants the keyboard, and the phone draws no surface module at all. So here
// the pane is built from the board's snapshot instead, out of the surface's
// own elements, as a roster: what needs the user, the crew, the open tasks
// by state and the latest messages, under an optional live graph. The band
// above the prompt reads the same asks.

import type { ElementTable } from 'claude-code'

import type { SnapAgent, SnapEvent, SnapTask, Snapshot } from '../types'

/** What a press in the pane does: the same as the chief's commands. */
export type CrewActions = {
  go: (task: string) => void
  handoff: (agent: string) => void
}

/** From here an agent is close to compacting: the board's CONTEXT_HIGH. */
export const HIGH = 80

/** How many of the latest messages the pane lists. */
const RECENT = 6

/** A message this young is still in flight on the graph. */
export const LIVE_MS = 60_000

/** This many messages along one line within STEADY_MS is steady traffic. */
const STEADY = 3
const STEADY_MS = 180_000

/** The tool as the fleet view names it: an MCP tool without its server. */
export function toolName(tool: string): string {
  return tool.startsWith('mcp__') ? tool.split('__').at(-1) ?? tool : tool
}

export type Tone = 'working' | 'needs' | 'idle' | 'gone'

/** What an agent is doing, in the fleet view's words. */
export function doing(a: SnapAgent): { text: string; tone: Tone } {
  if (a.waiting_for) return { text: `! needs you · ${a.waiting_for}`, tone: 'needs' }
  if (a.presence === 'working') return { text: `● ${a.tool ? toolName(a.tool) : 'working'}`, tone: 'working' }
  if (a.presence === 'waiting' && a.awaiting_go) return { text: '◇ needs a go', tone: 'needs' }
  if (a.presence === 'waiting') return { text: '○ idle', tone: 'idle' }
  if (a.presence === 'unlinked') return { text: '○ starting', tone: 'idle' }
  return { text: 'gone', tone: 'gone' }
}

/** The colours the band uses too, which read on light and dark alike. */
export const TONE = { working: 'yellow', needs: 'yellow', idle: 'green', gone: 'gray' } as const

/** An agent as one glyph in the band's row of dots. */
export function dot(a: SnapAgent): string {
  const d = doing(a)
  if (d.tone === 'needs') return a.waiting_for ? '!' : '◆'
  return d.tone === 'working' ? '●' : '○'
}

const STATE: Record<string, string> = {
  running: 'yellow',
  blocked: 'red',
  review: 'cyan',
  done: 'green',
  queued: 'gray',
  dropped: 'gray',
}

/** Something an agent waits on the user for, most urgent first. */
export type Ask = {
  agent: string
  task: string | null
  kind: 'prompt' | 'go' | 'full'
  /** What it is, as a sentence after the agent's name. */
  text: string
}

/** The newest message `agent` sent, which says what a go is for. */
function said(s: Snapshot, agent: string): string | undefined {
  return s.events.find(e => e.kind === 'message' && e.from === agent)?.summary
}

/**
 * What the crew waits on the user for: an agent stopped at a prompt in its
 * own session, then a plan waiting for a go, then an agent close to
 * compacting.
 */
export function asks(s: Snapshot): Ask[] {
  const crew = s.agents.filter(a => a.role === 'worker')
  const out: Ask[] = []
  for (const a of crew) {
    if (a.waiting_for) out.push({ agent: a.name, task: a.task, kind: 'prompt', text: `is waiting at a ${a.waiting_for} in its session.` })
  }
  for (const a of crew) {
    if (a.awaiting_go && a.presence === 'waiting') {
      const what = said(s, a.name)
      out.push({ agent: a.name, task: a.awaiting_go, kind: 'go', text: what ?? `needs a go on ${a.awaiting_go}.` })
    }
  }
  for (const a of crew) {
    if (a.context !== null && a.context >= HIGH) {
      out.push({ agent: a.name, task: a.task, kind: 'full', text: `is at ${a.context}% context and will compact soon.` })
    }
  }
  return out
}

/** The press that answers `ask`, if there is one: a prompt is answered in the agent's own session. */
export function askButton(el: Pick<ElementTable<'mobile'>, 'Button'>, ask: Ask, act: CrewActions, long: boolean) {
  const { Button } = el
  if (ask.kind === 'go') {
    return <Button key={`go-${ask.agent}`} variant="primary" label={`Go on ${ask.task}`} onPress={() => act.go(ask.task ?? '')} />
  }
  if (ask.kind === 'full') {
    return <Button key={`handoff-${ask.agent}`} label={long ? 'Hand off to a fresh session' : 'Hand off'} onPress={() => act.handoff(ask.agent)} />
  }
  return null
}

/** How to reach a background agent from a terminal: there is no tab here. */
function attach(a: SnapAgent): string | undefined {
  return a.target?.startsWith('bg:') ? `claude attach ${a.target.slice(3)}` : undefined
}

function esc(text: string): string {
  return text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;')
}

function clip(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, Math.max(1, max - 1))}…` : text
}

/**
 * When the pane first saw each event, by key: the snapshot carries no times,
 * so a message's age is how long ago it turned up. What was on the board at
 * the first look is old news, never in flight.
 */
export class Seen {
  private at = new Map<string, number>()
  private started = false

  track(events: SnapEvent[], now: number): Map<string, number> {
    for (const e of events) {
      if (!this.at.has(e.key)) this.at.set(e.key, this.started ? now : -Infinity)
    }
    this.started = true
    // Keep only what the board still lists.
    const listed = new Set(events.map(e => e.key))
    for (const key of this.at.keys()) if (!listed.has(key)) this.at.delete(key)
    return this.at
  }

  /** When the youngest message stops being in flight; undefined when none is. */
  nextExpiry(now: number): number | undefined {
    let soonest: number | undefined
    for (const t of this.at.values()) {
      const end = t + LIVE_MS
      if (end > now && (soonest === undefined || end < soonest)) soonest = end
    }
    return soonest
  }
}

// The graph's own colours: drawn as an image, it cannot read the theme, so it
// sits on its own dark plate and reads the same on light and dark.
const PLATE = '#2B2D42'
const FILL: Record<Tone, string> = { working: '#E0A526', needs: '#E76F51', idle: '#2A9D8F', gone: '#8A8A8A' }
const INK: Record<Tone, string> = { working: '#1F1A10', needs: '#FFFFFF', idle: '#FFFFFF', gone: '#FFFFFF' }

const GRAPH_STYLE =
  'text{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}' +
  '.dot{fill:none;stroke-width:7;stroke-linecap:round;stroke-dasharray:0.01 200}' +
  '.down{animation:fdown 2.4s linear infinite}' +
  '.up{animation:fup 2.4s linear infinite}' +
  '.march{fill:none;stroke-dasharray:7 7;animation:march .8s linear infinite}' +
  '.pulse{transform-box:fill-box;transform-origin:center;animation:pulse 1.6s ease-out infinite}' +
  '@keyframes fdown{from{stroke-dashoffset:0}to{stroke-dashoffset:-100}}' +
  '@keyframes fup{from{stroke-dashoffset:-100}to{stroke-dashoffset:0}}' +
  '@keyframes march{to{stroke-dashoffset:-14}}' +
  '@keyframes pulse{from{opacity:.9;transform:scale(1)}to{opacity:0;transform:scale(1.5)}}' +
  '@media (prefers-reduced-motion:reduce){.down,.up,.march,.pulse{animation:none}.dot{display:none}}'

type Node = { a: SnapAgent; x: number; y: number }

/**
 * The crew as a live graph: the chief's card over a round node per agent,
 * its ring the agent's context, pulsing while it needs the user. A line from
 * the chief into each agent, heavier the more has gone along it, dashed and
 * still where nothing has, with dashes moving along it while the traffic is
 * steady; agents that talk to each other are joined by a curve under them.
 * Each message from the last minute is a dot travelling from its sender to
 * its receiver, drawn with CSS animation since the image runs no script.
 */
export function graph(s: Snapshot, seen: Map<string, number>, now: number): { source: string; height: number; alt: string } {
  const W = 400
  const PER_ROW = 5
  const R = 20
  const RING = 26
  const CIRC = 2 * Math.PI * RING
  const chief = s.agents.find(a => a.role === 'chief')
  const crew = s.agents.filter(a => a.role === 'worker')
  const cx = W / 2
  const chiefTop = 30
  const chiefBottom = chiefTop + 42
  const parts: string[] = []

  if (crew.length === 0) {
    const height = 190
    parts.push(
      `<rect width="${W}" height="${height}" rx="12" fill="${PLATE}"/>`,
      `<path d="M${cx} ${chiefBottom} L${cx} ${chiefBottom + 36}" fill="none" stroke="#6C6F93" stroke-width="1.5" stroke-dasharray="4 5"/>`,
      ...[-74, 0, 74].map(dx => `<circle cx="${cx + dx}" cy="${chiefBottom + 58}" r="${R}" fill="none" stroke="#6C6F93" stroke-width="1.5" stroke-dasharray="4 4"/>`),
      `<text x="${cx}" y="${height - 14}" font-size="11" fill="#D6D7E6" text-anchor="middle">agents join here when you approve a plan</text>`,
    )
    chiefCard()
    return { source: wrap(height), height, alt: `The crew: ${chief ? `${chief.name}, the chief, and ` : ''}no agents yet` }
  }

  // Where each agent sits: rows of up to PER_ROW, centred.
  const rows = Math.ceil(crew.length / PER_ROW)
  const step = W / Math.min(PER_ROW, crew.length)
  const rowH = 110
  const nodes = new Map<string, Node>()
  crew.forEach((a, i) => {
    const row = Math.floor(i / PER_ROW)
    const inRow = Math.min(PER_ROW, crew.length - row * PER_ROW)
    const x = W / 2 + (i % PER_ROW - (inRow - 1) / 2) * step
    nodes.set(a.name, { a, x, y: chiefBottom + 78 + row * rowH })
  })

  // Messages by the pair they went between, either way, with their ages.
  const chiefName = chief?.name
  const pair = (a: string, b: string) => (a < b ? `${a}\n${b}` : `${b}\n${a}`)
  const total = new Map<string, number>()
  const steady = new Map<string, number>()
  const live: SnapEvent[] = []
  for (const e of s.events) {
    if (e.kind !== 'message' || !e.from || !e.to) continue
    const known = (n: string) => n === chiefName || nodes.has(n)
    if (!known(e.from) || !known(e.to) || e.from === e.to) continue
    const k = pair(e.from, e.to)
    total.set(k, (total.get(k) ?? 0) + 1)
    const age = now - (seen.get(e.key) ?? -Infinity)
    if (age <= STEADY_MS) steady.set(k, (steady.get(k) ?? 0) + 1)
    if (age <= LIVE_MS) live.push(e)
  }

  // The lines: chief to agent, and a curve under the nodes between agents.
  const lines = new Map<string, { d: string; from: string }>()
  for (const n of nodes.values()) {
    if (!chiefName) break
    const mid = (chiefBottom + n.y - RING) / 2
    lines.set(pair(chiefName, n.a.name), { d: `M${cx} ${chiefBottom} C${cx} ${mid}, ${n.x} ${mid}, ${n.x} ${n.y - RING}`, from: chiefName })
  }
  let dip = 0
  for (const k of total.keys()) {
    if (lines.has(k)) continue
    const [p, q] = k.split('\n') as [string, string]
    const a = nodes.get(p)
    const b = nodes.get(q)
    if (!a || !b) continue
    const sag = 44 + Math.abs(a.x - b.x) / 8
    dip = Math.max(dip, sag)
    // From under each name, so the curve never crosses one.
    const ay = a.y + RING + 26
    const by = b.y + RING + 26
    lines.set(k, { d: `M${a.x} ${ay} C${a.x} ${ay + sag}, ${b.x} ${by + sag}, ${b.x} ${by}`, from: p })
  }

  const height = Math.round(chiefBottom + 78 + (rows - 1) * rowH + RING + 30 + (dip > 0 ? dip * 0.75 + 4 : 0))
  parts.push(`<rect width="${W}" height="${height}" rx="12" fill="${PLATE}"/>`)
  const inFlight = live.length
  parts.push(
    `<text x="12" y="20" font-size="11" fill="#B9BBD0">${inFlight > 0 ? `live · ${inFlight} message${inFlight === 1 ? '' : 's'} in the last minute` : 'quiet · no messages in the last minute'}</text>`,
  )

  for (const [k, line] of lines) {
    const n = total.get(k) ?? 0
    const [p, q] = k.split('\n') as [string, string]
    const toChief = p === chiefName || q === chiefName
    const agent = nodes.get(p === chiefName ? q : p)?.a
    const stroke = toChief && agent ? FILL[doing(agent).tone] : '#8A8DB0'
    if (n === 0) {
      parts.push(`<path d="${line.d}" fill="none" stroke="${stroke}" stroke-width="1.5" stroke-dasharray="5 5" opacity="0.8"/>`)
      continue
    }
    const width = (1.5 + Math.min(3, n * 0.5)).toFixed(1)
    const isSteady = (steady.get(k) ?? 0) >= STEADY
    parts.push(`<path d="${line.d}" fill="none" stroke="${stroke}" stroke-width="${isSteady ? Math.max(4, Number(width)) : width}" opacity="${isSteady ? 0.55 : 0.9}"/>`)
    if (isSteady) parts.push(`<path class="march" d="${line.d}" stroke="#F5D488" stroke-width="2"/>`)
  }

  // A dot for each message in flight, from its sender to its receiver; two
  // along one line set off one after the other.
  const along = new Map<string, number>()
  for (const e of live) {
    const k = pair(e.from ?? '', e.to ?? '')
    const line = lines.get(k)
    if (!line) continue
    const i = along.get(k) ?? 0
    along.set(k, i + 1)
    const way = line.from === e.from ? 'down' : 'up'
    const delay = i > 0 ? ` style="animation-delay:${(i * 0.8).toFixed(1)}s"` : ''
    parts.push(`<path class="dot ${way}" pathLength="100" d="${line.d}" stroke="${way === 'down' ? '#FFE7A3' : '#FFFFFF'}"${delay}/>`)
  }

  for (const { a, x, y } of nodes.values()) {
    const d = doing(a)
    const full = a.context !== null && a.context >= HIGH
    if (d.tone === 'needs') parts.push(`<circle class="pulse" cx="${x}" cy="${y}" r="${RING}" fill="none" stroke="${FILL.needs}" stroke-width="3"/>`)
    parts.push(`<circle cx="${x}" cy="${y}" r="${RING}" fill="none" stroke="#4A4D68" stroke-width="4"/>`)
    if (a.context !== null) {
      const arc = ((CIRC * Math.min(100, Math.max(0, a.context))) / 100).toFixed(1)
      parts.push(
        `<circle cx="${x}" cy="${y}" r="${RING}" fill="none" stroke="${full ? FILL.needs : '#D6D7E6'}" stroke-width="4" stroke-dasharray="${arc} ${CIRC.toFixed(1)}" transform="rotate(-90 ${x} ${y})"/>`,
      )
    }
    parts.push(`<circle cx="${x}" cy="${y}" r="${R}" fill="${FILL[d.tone]}"/>`)
    const mark = d.tone === 'needs' ? '!' : a.context !== null ? `${a.context}%` : '–'
    parts.push(
      `<text x="${x}" y="${y + 5}" font-size="${mark === '!' ? 14 : 12}" font-weight="${mark === '!' ? 800 : 700}" fill="${INK[d.tone]}" text-anchor="middle">${mark}</text>`,
      `<text x="${x}" y="${y + RING + 20}" font-size="12" font-weight="600" fill="#FFFFFF" text-anchor="middle">${esc(clip(a.name, Math.floor(step / 7.5)))}</text>`,
    )
  }
  chiefCard()

  const alt =
    `The crew: ${chief ? `${chief.name}, the chief, ` : ''}and ${crew.length} ${crew.length === 1 ? 'agent' : 'agents'}: ` +
    crew.map(a => `${a.name} ${doing(a).text}${a.context !== null ? ` at ${a.context}% context` : ''}`).join('; ') +
    (inFlight > 0 ? `. In the last minute: ${live.map(e => `${e.from} to ${e.to}`).join(', ')}` : '. No messages in the last minute')
  return { source: wrap(height), height, alt }

  function chiefCard() {
    if (!chief) return
    const d = doing(chief)
    const what = d.tone === 'working' ? d.text.replace(/^● /, '') : d.tone === 'idle' ? 'idle' : d.tone === 'needs' ? 'needs you' : 'gone'
    parts.push(
      `<rect x="${cx - 70}" y="${chiefTop}" width="140" height="42" rx="10" fill="#3D405B" stroke="#6C6F93"/>`,
      `<text x="${cx}" y="${chiefTop + 18}" font-size="13" font-weight="600" fill="#FFFFFF" text-anchor="middle">◆ ${esc(clip(chief.name, 16))}</text>`,
      `<text x="${cx}" y="${chiefTop + 34}" font-size="11" fill="#D6D7E6" text-anchor="middle">${esc(clip(what, 14))}${chief.context !== null ? ` · ${chief.context}%` : ''}</text>`,
    )
  }

  function wrap(h: number): string {
    return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${W} ${h}" width="${W}" height="${h}"><style>${GRAPH_STYLE}</style>${parts.join('')}</svg>`
  }
}

/** An agent's context as a small bar, red once it is close to compacting. */
function contextBar(context: number): string {
  const w = Math.round((140 * Math.min(100, Math.max(0, context))) / 100)
  const fill = context >= HIGH ? FILL.needs : '#9A9A9A'
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 140 6" width="140" height="6"><rect width="140" height="6" rx="3" fill="#8A8A8A" opacity="0.35"/><rect width="${w}" height="6" rx="3" fill="${fill}"/></svg>`
}

/** The order the board lists open tasks in. */
const TASK_ORDER = ['blocked', 'review', 'running', 'queued']

/** Open tasks grouped by state, in the board's order; any other state after. */
function grouped(tasks: SnapTask[]): [string, SnapTask[]][] {
  const by = new Map<string, SnapTask[]>()
  for (const t of tasks) by.set(t.state, [...(by.get(t.state) ?? []), t])
  const states = [...TASK_ORDER.filter(st => by.has(st)), ...[...by.keys()].filter(st => !TASK_ORDER.includes(st))]
  return states.map(st => [st, by.get(st) ?? []])
}

export type PaneOptions = {
  columns: number
  /** Whether the live graph is shown, and the press that flips it. */
  showGraph: boolean
  toggleGraph: () => void
  /** When each event was first seen, and the time now, for the graph. */
  seen: Map<string, number>
  now: number
}

/** The pane, built from `s` with the elements of the surface it is drawn on. */
export function crewPane(el: ElementTable<'mobile'>, s: Snapshot, act: CrewActions, o: PaneOptions) {
  const { Box, Text, Button, Svg, Markdown } = el
  const crew = s.agents.filter(a => a.role === 'worker')
  const working = crew.filter(a => a.presence === 'working').length
  const want = asks(s)
  const needing = new Set(want.map(w => w.agent)).size
  const messages = s.events.filter(e => e.kind === 'message')

  return (
    <Box flexDirection="column" gap={1} paddingX={1}>
      <Box flexDirection="column">
        <Box flexDirection="row" alignItems="center" columnGap={1}>
          <Box flexGrow={1}>
            <Markdown text="### Fleet" />
          </Box>
          <Button key="fleet-graph" variant={o.showGraph ? 'primary' : 'secondary'} label="Graph" onPress={() => o.toggleGraph()} />
        </Box>
        {crew.length === 0 && s.tasks.length === 0 ? (
          <Text dimColor>no agents · no open tasks</Text>
        ) : (
          <Box flexDirection="row" columnGap={2} flexWrap="wrap" alignItems="center">
            <Text dimColor>{`${crew.length} ${crew.length === 1 ? 'agent' : 'agents'}`}</Text>
            {working > 0 ? <Text color="yellow">{`● ${working} working`}</Text> : <Text dimColor>0 working</Text>}
            <Text dimColor>{`${s.tasks.length} open`}</Text>
            {needing > 0 && <Text color="yellow" bold>{`▲ ${needing} need${needing === 1 ? 's' : ''} you`}</Text>}
          </Box>
        )}
      </Box>

      {o.showGraph && graphBlock()}

      {crew.length === 0 && (
        <Text>Tell the chief what you want done. It plans the work across your repositories. When you approve the plan, it starts one agent per repository.</Text>
      )}

      {want.length > 0 && (
        <Box flexDirection="column" gap={1}>
          <Text bold dimColor>NEEDS YOU</Text>
          {want.map(w => (
            <Box key={`ask-${w.kind}-${w.agent}`} flexDirection="column" borderStyle="round" borderColor="yellow" paddingX={1}>
              <Text>
                <Text bold>{w.agent}</Text>
                {w.task ? <Text dimColor>{` · ${w.task}`}</Text> : null}
              </Text>
              <Text>{w.text}</Text>
              {askButton(el, w, act, true)}
            </Box>
          ))}
        </Box>
      )}

      {crew.length > 0 && (
        <Box flexDirection="column" gap={1}>
          <Text bold dimColor>CREW</Text>
          {crew.map(a => {
            const d = doing(a)
            const full = a.context !== null && a.context >= HIGH
            const how = attach(a)
            return (
              <Box key={`agent-${a.name}`} flexDirection="column">
                <Box flexDirection="row" columnGap={1} flexWrap="wrap">
                  <Text color={TONE[d.tone]}>{dot(a)}</Text>
                  <Text bold>{a.name}</Text>
                  {a.task && <Text dimColor>{a.task}</Text>}
                  <Text color={TONE[d.tone]}>{d.text}</Text>
                </Box>
                {(a.context !== null || how) && (
                  <Box flexDirection="row" columnGap={1} alignItems="center" paddingLeft={2} flexWrap="wrap">
                    {a.context !== null && <Svg source={contextBar(a.context)} alt={`${a.context}% of its context used`} width={140} height={6} />}
                    {a.context !== null && <Text color={full ? 'yellow' : undefined} dimColor={!full} bold={full}>{`${a.context}% context`}</Text>}
                    {how && <Text dimColor>{how}</Text>}
                  </Box>
                )}
              </Box>
            )
          })}
        </Box>
      )}

      {s.tasks.length > 0 && (
        <Box flexDirection="column" gap={1}>
          <Text bold dimColor>TASKS</Text>
          {grouped(s.tasks).map(([state, tasks]) => (
            <Box key={`state-${state}`} flexDirection="column">
              <Text color={STATE[state] ?? 'gray'}>{`${state} · ${tasks.length}`}</Text>
              {tasks.map(t => (
                <Box key={`task-${t.key}`} flexDirection="row" columnGap={2} flexWrap="wrap" paddingLeft={2}>
                  <Text>{t.key}</Text>
                  <Text>{t.title}</Text>
                  {t.agent && <Text dimColor>{t.agent}</Text>}
                  {t.note && <Text dimColor italic>{t.note}</Text>}
                </Box>
              ))}
            </Box>
          ))}
        </Box>
      )}

      {messages.length > 0 && (
        <Box flexDirection="column">
          <Text bold dimColor>LATEST</Text>
          {messages.slice(0, RECENT).map(e => (
            <Text key={`event-${e.key}`} wrap="truncate-end">
              <Text dimColor>{`${e.from ?? '?'} → ${e.to ?? '?'}  `}</Text>
              {e.summary}
            </Text>
          ))}
        </Box>
      )}
    </Box>
  )

  function graphBlock() {
    const g = graph(s, o.seen, o.now)
    const width = Math.min(560, Math.max(280, o.columns * 8))
    return (
      <Box flexDirection="column">
        <Svg source={g.source} alt={g.alt} width={width} />
        {crew.length > 0 && <Text dimColor>● message in flight · ⋯ steady traffic · - - quiet · ring = context</Text>}
      </Box>
    )
  }
}
