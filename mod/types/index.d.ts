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

declare module 'claude-code' {
  interface PluginState {
    fleet: { snapshot: Snapshot | null }
  }
}
