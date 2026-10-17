// The fleet pane on a surface that draws natively: the desktop app and the
// phone. The terminal shows the fleet tab itself, drawn by fleet as text
// (view.tsx); a desktop window would show that as a grid of characters that
// wants the keyboard, and the phone draws no surface module at all. So here
// the pane is built from the board's snapshot instead, out of the surface's
// own elements: the crew as a graph, a card for each agent with its buttons,
// the open tasks and the latest messages.

import type { ElementTable } from 'claude-code'

import type { SnapAgent, SnapTask, Snapshot } from '../types'

/** What a press in the pane does: the same as the chief's commands. */
export type CrewActions = {
  go: (task: string) => void
  handoff: (agent: string) => void
}

/** From here an agent is close to compacting: the board's CONTEXT_HIGH. */
export const HIGH = 80

/** How many of the latest messages the pane lists. */
const RECENT = 6

/** The tool as the fleet view names it: an MCP tool without its server. */
export function toolName(tool: string): string {
  return tool.startsWith('mcp__') ? tool.split('__').at(-1) ?? tool : tool
}

/** What an agent is doing, in the fleet view's words. */
export function doing(a: SnapAgent): { text: string; tone: 'working' | 'needs' | 'idle' | 'gone' } {
  if (a.waiting_for) return { text: `! needs you · ${a.waiting_for}`, tone: 'needs' }
  if (a.presence === 'working') return { text: `● ${a.tool ? toolName(a.tool) : 'working'}`, tone: 'working' }
  if (a.presence === 'waiting' && a.awaiting_go) return { text: '◇ needs a go', tone: 'needs' }
  if (a.presence === 'waiting') return { text: '○ idle', tone: 'idle' }
  if (a.presence === 'unlinked') return { text: '○ starting', tone: 'idle' }
  return { text: 'gone', tone: 'gone' }
}

/** The colours the band uses too, which read on light and dark alike. */
const TONE = { working: 'yellow', needs: 'yellow', idle: 'green', gone: 'gray' } as const

const STATE: Record<string, string> = {
  running: 'yellow',
  blocked: 'red',
  review: 'cyan',
  done: 'green',
  queued: 'gray',
  dropped: 'gray',
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
 * The crew as the fleet tab draws it, as an SVG: the chief's card over its
 * agents', a line from it into each, heavier the more has gone along it and
 * dashed where nothing has, coloured by what the agent at its end is doing.
 *
 * Drawn as an image, so it cannot read the theme: every card is filled, its
 * text on its own fill, and the lines are mid-tones that read on light and
 * dark alike.
 */
export function graph(s: Snapshot): { source: string; height: number; alt: string } {
  const W = 640
  const PER_ROW = 4
  const GAP = 14
  const CARD_H = 46
  const chief = s.agents.find(a => a.role === 'chief')
  const crew = s.agents.filter(a => a.role === 'worker')
  const fill = {
    chief: '#3d405b',
    working: '#e0a526',
    needs: '#e76f51',
    idle: '#2a9d8f',
    gone: '#8a8a8a',
  }
  const ink = { chief: '#ffffff', working: '#1f1a10', needs: '#ffffff', idle: '#ffffff', gone: '#ffffff' }

  // Traffic between the chief and each agent, either way.
  const traffic = new Map<string, number>()
  for (const e of s.events) {
    if (e.kind !== 'message') continue
    const other = e.from === chief?.name ? e.to : e.to === chief?.name ? e.from : null
    if (other) traffic.set(other, (traffic.get(other) ?? 0) + 1)
  }

  const rows = Math.max(1, Math.ceil(crew.length / PER_ROW))
  const height = 24 + CARD_H + 64 + rows * (CARD_H + 20)
  const parts: string[] = []
  const chiefW = 200
  const cx = W / 2
  const chiefY = 12
  const card = (x: number, y: number, w: number, tone: keyof typeof fill, name: string, line: string) => {
    parts.push(
      `<rect x="${x}" y="${y}" width="${w}" height="${CARD_H}" rx="9" fill="${fill[tone]}"/>`,
      `<text x="${x + 12}" y="${y + 19}" fill="${ink[tone]}" font-size="13" font-weight="600">${esc(clip(name, Math.floor(w / 8)))}</text>`,
      `<text x="${x + 12}" y="${y + 36}" fill="${ink[tone]}" font-size="11" opacity="0.85">${esc(clip(line, Math.floor(w / 6.5)))}</text>`,
    )
  }

  const each = Math.min(180, (W - GAP * (Math.min(crew.length, PER_ROW) + 1)) / Math.max(1, Math.min(crew.length, PER_ROW)))
  crew.forEach((a, i) => {
    const row = Math.floor(i / PER_ROW)
    const inRow = Math.min(PER_ROW, crew.length - row * PER_ROW)
    const span = inRow * each + (inRow - 1) * GAP
    const x = (W - span) / 2 + (i % PER_ROW) * (each + GAP)
    const y = chiefY + CARD_H + 64 + row * (CARD_H + 20)
    const d = doing(a)
    const n = traffic.get(a.name) ?? 0
    const stroke = d.tone === 'working' ? '#e0a526' : d.tone === 'needs' ? '#e76f51' : d.tone === 'idle' ? '#2a9d8f' : '#8a8a8a'
    const midY = (chiefY + CARD_H + y) / 2
    parts.unshift(
      `<path d="M ${cx} ${chiefY + CARD_H} C ${cx} ${midY}, ${x + each / 2} ${midY}, ${x + each / 2} ${y}" fill="none" stroke="${stroke}" stroke-width="${(1.5 + Math.min(4, n * 0.75)).toFixed(2)}"${n === 0 ? ' stroke-dasharray="5 5"' : ''} opacity="0.9"/>`,
    )
    const ctx = a.context !== null ? ` · ${a.context}%` : ''
    card(x, y, each, d.tone, a.name, `${d.text}${ctx}${a.task ? ` · ${a.task}` : ''}`)
  })
  if (chief) {
    const d = doing(chief)
    card(cx - chiefW / 2, chiefY, chiefW, 'chief', `◆ ${chief.name}`, `${d.text}${chief.context !== null ? ` · ${chief.context}%` : ''}`)
  }

  const source =
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${W} ${height}" width="${W}" height="${height}" ` +
    `font-family="-apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif">${parts.join('')}</svg>`
  const alt =
    `The crew: ${chief ? `${chief.name}, the chief, ` : ''}and ${crew.length} ${crew.length === 1 ? 'agent' : 'agents'}` +
    (crew.length ? `: ${crew.map(a => `${a.name} ${doing(a).text}`).join('; ')}` : '')
  return { source, height, alt }
}

/** Open tasks first by state, as the board lists them. */
function ordered(tasks: SnapTask[]): SnapTask[] {
  const rank = ['blocked', 'review', 'running', 'queued']
  const at = (t: SnapTask) => (rank.indexOf(t.state) + rank.length + 1) % (rank.length + 1)
  return [...tasks].sort((a, b) => at(a) - at(b))
}

/** The pane, built from `s` with the elements of the surface it is drawn on. */
export function crewPane(el: ElementTable<'mobile'>, s: Snapshot, act: CrewActions, columns: number) {
  const { Box, Text, Button, Svg } = el
  const crew = s.agents.filter(a => a.role === 'worker')
  const working = crew.filter(a => a.presence === 'working').length
  const needing = crew.filter(a => doing(a).tone === 'needs')
  const g = graph(s)

  return (
    <Box flexDirection="column" gap={1} paddingX={1}>
      <Box flexDirection="row" columnGap={2} flexWrap="wrap" alignItems="center">
        <Text dimColor>{`${crew.length} ${crew.length === 1 ? 'agent' : 'agents'}`}</Text>
        {working > 0 ? <Text color="yellow">{`● ${working} working`}</Text> : <Text dimColor>0 working</Text>}
        <Text dimColor>{`${s.tasks.length} open`}</Text>
        {needing.length > 0 && <Text color="yellow" bold>{`▲ ${needing.length} need${needing.length === 1 ? 's' : ''} you`}</Text>}
      </Box>

      {crew.length > 0 ? (
        <Svg source={g.source} alt={g.alt} width={Math.min(640, Math.max(280, columns * 8))} />
      ) : (
        <Text dimColor>No agents yet. Tell the chief what you want done.</Text>
      )}

      {crew.length > 0 && (
        <Box flexDirection="column" gap={1}>
          <Text bold dimColor>CREW</Text>
          {crew.map(a => {
            const d = doing(a)
            const full = a.context !== null && a.context >= HIGH
            const how = attach(a)
            return (
              <Box key={`agent-${a.name}`} flexDirection="column" borderStyle="round" borderColor={d.tone === 'needs' ? 'yellow' : 'gray'} paddingX={1}>
                <Box flexDirection="row" columnGap={2} flexWrap="wrap" alignItems="center">
                  <Text bold>{a.name}</Text>
                  {a.task && <Text dimColor>{a.task}</Text>}
                  <Text color={TONE[d.tone]}>{d.text}</Text>
                  {a.context !== null && <Text color={full ? 'yellow' : undefined} dimColor={!full}>{`${a.context}% context`}</Text>}
                </Box>
                {(a.awaiting_go && a.presence === 'waiting') || full ? (
                  <Box flexDirection="row" columnGap={1}>
                    {a.awaiting_go && a.presence === 'waiting' && (
                      <Button key={`go-${a.name}`} variant="primary" label={`Go on ${a.awaiting_go}`} onPress={() => act.go(a.awaiting_go ?? '')} />
                    )}
                    {full && <Button key={`handoff-${a.name}`} label="Hand off to a fresh session" onPress={() => act.handoff(a.name)} />}
                  </Box>
                ) : null}
                {how && <Text dimColor>{how}</Text>}
              </Box>
            )
          })}
        </Box>
      )}

      {s.tasks.length > 0 && (
        <Box flexDirection="column">
          <Text bold dimColor>TASKS</Text>
          {ordered(s.tasks).map(t => (
            <Box key={`task-${t.key}`} flexDirection="row" columnGap={2} flexWrap="wrap">
              <Text>{t.key}</Text>
              <Text color={STATE[t.state] ?? 'gray'}>{t.state}</Text>
              <Text>{t.title}</Text>
              {t.agent && <Text dimColor>{t.agent}</Text>}
              {t.note && <Text dimColor italic>{t.note}</Text>}
            </Box>
          ))}
        </Box>
      )}

      {s.events.some(e => e.kind === 'message') && (
        <Box flexDirection="column">
          <Text bold dimColor>LATEST</Text>
          {s.events.filter(e => e.kind === 'message').slice(0, RECENT).map(e => (
            <Box key={`event-${e.key}`} flexDirection="row" columnGap={1}>
              <Text dimColor>{`${e.from ?? '?'} → ${e.to ?? '?'}`}</Text>
              <Text wrap="truncate-end">{e.summary}</Text>
            </Box>
          ))}
        </Box>
      )}
    </Box>
  )
}
