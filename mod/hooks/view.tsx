// The fleet view in the chief's /fleet pane: the frames fleet draws, and the
// keys and clicks that go back to it.
//
// A surface module: it runs where the pane is drawn, with no `$`. It draws
// the newest frame its props carry, asks the hooks module for a newer one on
// its frame clock, and posts every key pressed while it has the focus and
// every click, with its own size, so the view behind it is drawn to fit.

import type { ClientModule } from 'claude-code'

import type { GraphSpan } from '../types'

type Props = { lines: GraphSpan[][] }
type State = { isStarted: true }

/** How often to ask for a newer frame: the view's own animation rate. */
const TICK_MS = 100

const FleetView: ClientModule<Props, State> = (props, surface) => {
  const { Box, Text } = surface.elements
  if (surface.state === undefined) {
    surface.every(TICK_MS, () => surface.post({ t: 'tick', w: surface.columns, h: surface.rows }))
    surface.onKey(e => surface.post({ t: 'key', key: e.key, ctrl: e.ctrl === true, shift: e.shift === true, meta: e.meta === true }))
    surface.onPointer(e => {
      if (e.type === 'down' && e.button === 'left') surface.post({ t: 'click', x: e.x, y: e.y })
    })
    surface.setState({ isStarted: true })
  }
  const lines = props.lines ?? []
  if (lines.length === 0) return <Text dimColor>Drawing the fleet…</Text>
  return (
    <Box flexDirection="column">
      {lines.map((line, i) => (
        <Text key={`l${i}`} wrap="truncate-end">
          {line.length === 0 ? ' ' : line.map(span => (
            <Text color={span.fg} bold={span.bold} dimColor={span.dim}>{span.t}</Text>
          ))}
        </Text>
      ))}
    </Box>
  )
}

export default FleetView
