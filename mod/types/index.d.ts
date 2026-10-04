// What `fleet board snapshot` prints: the fleet as the chief's session draws it.

export type SnapAgent = {
  name: string
  role: 'chief' | 'worker'
  presence: 'working' | 'waiting' | 'gone' | 'unlinked'
  waiting_for: string | null
  tool: string | null
  context: number | null
  task: string | null
  awaiting_go: string | null
  target: string | null
}

export type SnapTask = {
  key: string
  title: string
  state: string
  agent: string | null
  note: string | null
}

export type SnapEvent = {
  key: string
  kind: string
  from: string | null
  to: string | null
  task: string | null
  summary: string
  notice: { title: string; body: string } | null
}

export type Snapshot = {
  me: { name: string; role: string } | null
  agents: SnapAgent[]
  tasks: SnapTask[]
  events: SnapEvent[]
}

/** One run of cells in one style, as `fleet board graph` draws them. */
export type GraphSpan = { t: string; fg?: string; bold?: boolean; dim?: boolean }

declare module 'claude-code' {
  interface PluginState {
    fleet: {
      snapshot: Snapshot | null
      /** The brief a session made the chief with /fleet start received as a
       *  turn, kept to put back after compaction. */
      brief: string | null
      /** The fleet view's graph, drawn by fleet at the pane's size. */
      graph: GraphSpan[][] | null
    }
  }
}
