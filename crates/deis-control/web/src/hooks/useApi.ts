import { useState, useEffect, useCallback, useRef } from 'react'
import { api } from '@/lib/api'
import type { Command } from '@/lib/api'

// Every request below goes through `api.request` rather than a bare `fetch`.
// These hooks used to call `fetch` directly, which meant they sent no
// `X-API-Key`: with `api_auth_enabled = true` the whole dashboard answered 401
// on every poll. One request layer, one place that carries the key and handles
// a 401.
//
// These hooks never mount without a credential the control plane has already
// accepted: App.tsx asks one question before it renders them and shows the key
// screen instead when the answer is 401 (see `Access` there). That is what
// stops the five intervals below from polling a control plane that is refusing
// all of them.

/**
 * Read a list out of a response that may be either `{ <key>: [...] }` or a
 * bare array. Both shapes are in use across these endpoints, and a mismatch
 * used to surface as an empty page rather than an error.
 */
function listFrom<T>(data: unknown, key: string): T[] {
  if (Array.isArray(data)) return data as T[]
  if (data && typeof data === 'object') {
    const value = (data as Record<string, unknown>)[key]
    if (Array.isArray(value)) return value as T[]
  }
  return []
}

/**
 * Same idea for an object-valued response: `{ <key>: {...} }` or the object
 * itself.
 */
function objectFrom(data: unknown, key: string): Record<string, unknown> {
  if (data && typeof data === 'object' && !Array.isArray(data)) {
    const record = data as Record<string, unknown>
    const value = record[key]
    if (value && typeof value === 'object' && !Array.isArray(value)) {
      return value as Record<string, unknown>
    }
    return record
  }
  return {}
}

/**
 * One node holding a control channel open to the control plane, as
 * `/api/sessions` reports it.
 *
 * A heartbeat on this channel says the node is there and nothing else, which
 * is why there is no load figure here: the wire's heartbeat message is a
 * status string.
 */
interface SessionSummary {
  id: string
  instance_id: string
  node_role: string
  host: string
  ports: number[]
  state: string
  connected_at: string
  last_heartbeat: string
}


/**
 * One deis-runtime node, as `/api/runtime` reports it.
 *
 * These are not the same objects as `SessionSummary` above: those are the
 * processes holding a deis-wire control channel open, while these are the
 * runtime nodes the control plane dials over gRPC to deploy versions, probe
 * health and fire cron. A node gets on this list either because it is named
 * in the control plane's configuration file or because it registered itself,
 * which is what `source` says.
 */
interface FleetNode {
  id: string
  addr: string
  source: 'configured' | 'registered'
  registered_at: string | null
  last_registered_at: string | null
  healthy: boolean
  version: string | null
  last_checked: string | null
  last_error: string | null
}

interface Event {
  event_type: string
  node_id?: string
  severity: string
  message: string
  metadata?: Record<string, unknown>
  timestamp: string
}

interface MetricsSummary {
  nodes: {
    total: number
    active: number
    connected: number
    registered: number
    disconnected: number
    by_type: Record<string, number>
  }
  system: {
    uptime_seconds: number
    total_events: number
    total_commands: number
    total_heartbeats: number
  }
}

export function useMetrics() {
  const [metrics, setMetrics] = useState<MetricsSummary | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const initialLoadRef = useRef(true)

  const fetchMetrics = useCallback(async () => {
    try {
      if (initialLoadRef.current) {
        setLoading(true)
      }
      const data = await api.request<unknown>('/api/metrics/summary')
      setMetrics(objectFrom(data, 'metrics') as unknown as MetricsSummary)
      setError(null)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch metrics')
    } finally {
      if (initialLoadRef.current) {
        setLoading(false)
        initialLoadRef.current = false
      }
    }
  }, [])

  useEffect(() => {
    const initialFetch = window.setTimeout(fetchMetrics, 0)
    const interval = window.setInterval(fetchMetrics, 2000)
    return () => {
      window.clearTimeout(initialFetch)
      window.clearInterval(interval)
    }
  }, [fetchMetrics])

  return { metrics, loading, error, refetch: fetchMetrics }
}

/**
 * The control channels open to the control plane: every node that
 * authenticated, registered and is heartbeating right now. Commands and
 * configuration pushes travel down these, and nothing else can receive one.
 */
export function useSessions() {
  const [sessions, setSessions] = useState<SessionSummary[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const initialLoadRef = useRef(true)

  const fetchSessions = useCallback(async () => {
    try {
      // Only show the loading state on the first fetch, so background refreshes
      // do not blank out data the user is already reading
      if (initialLoadRef.current) {
        setLoading(true)
      }
      const data = await api.request<unknown>('/api/sessions')
      setSessions(listFrom<SessionSummary>(data, 'sessions'))
      setError(null)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch control channels')
    } finally {
      if (initialLoadRef.current) {
        setLoading(false)
        initialLoadRef.current = false
      }
    }
  }, [])

  useEffect(() => {
    const initialFetch = window.setTimeout(fetchSessions, 0)
    const interval = window.setInterval(fetchSessions, 5000)
    return () => {
      window.clearTimeout(initialFetch)
      window.clearInterval(interval)
    }
  }, [fetchSessions])

  return { sessions, loading, error, refetch: fetchSessions }
}

/**
 * The runtime fleet: every node that can be handed a deployment or a cron
 * occurrence, whether it came from the configuration file or registered
 * itself.
 */
export function useFleet() {
  const [nodes, setNodes] = useState<FleetNode[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const initialLoadRef = useRef(true)

  const fetchNodes = useCallback(async () => {
    try {
      if (initialLoadRef.current) {
        setLoading(true)
      }
      const data = await api.request<unknown>('/api/runtime')
      setNodes(listFrom<FleetNode>(data, 'nodes'))
      setError(null)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch the runtime fleet')
    } finally {
      if (initialLoadRef.current) {
        setLoading(false)
        initialLoadRef.current = false
      }
    }
  }, [])

  useEffect(() => {
    const initialFetch = window.setTimeout(fetchNodes, 0)
    const interval = window.setInterval(fetchNodes, 5000)
    return () => {
      window.clearTimeout(initialFetch)
      window.clearInterval(interval)
    }
  }, [fetchNodes])

  return { fleet: nodes, loading, error, refetch: fetchNodes }
}

export function useCommands(nodeId?: string) {
  const [commands, setCommands] = useState<Command[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const initialLoadRef = useRef(true)

  const fetchCommands = useCallback(async () => {
    try {
      if (initialLoadRef.current) {
        setLoading(true)
      }
      const url = nodeId ? `/api/commands/node/${nodeId}` : '/api/commands'
      const data = await api.request<unknown>(url)
      setCommands(listFrom<Command>(data, 'commands'))
      setError(null)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch commands')
    } finally {
      if (initialLoadRef.current) {
        setLoading(false)
        initialLoadRef.current = false
      }
    }
  }, [nodeId])

  useEffect(() => {
    const initialFetch = window.setTimeout(fetchCommands, 0)
    const interval = window.setInterval(fetchCommands, 3000)
    return () => {
      window.clearTimeout(initialFetch)
      window.clearInterval(interval)
    }
  }, [fetchCommands])

  // The server's body is `{ command_type, data }`. `command_type` is a
  // tagged enum: the named commands are plain strings and anything else is
  // `{ custom: "<name>" }`, so a free-typed command travels as the latter or
  // the whole request comes back 422 without ever reaching a node.
  const executeCommand = async (nodeId: string, commandType: string, data?: unknown) => {
    const named = [
      'shutdown',
      'restart',
      'reload',
      'get_status',
      'get_metrics',
      'execute_scheduled',
    ]
    const command_type = named.includes(commandType) ? commandType : { custom: commandType }
    const result = await api.request<unknown>(`/api/commands/execute/${nodeId}`, {
      method: 'POST',
      body: JSON.stringify({ command_type, data }),
    })
    await fetchCommands()
    return result
  }

  const cancelCommand = async (commandId: string) => {
    await api.request(`/api/commands/cancel/${commandId}`, { method: 'POST' })
    await fetchCommands()
  }

  return { commands, loading, error, refetch: fetchCommands, executeCommand, cancelCommand }
}

export function useEvents() {
  const [events, setEvents] = useState<Event[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const initialLoadRef = useRef(true)

  const fetchEvents = useCallback(async () => {
    try {
      if (initialLoadRef.current) {
        setLoading(true)
      }
      const data = await api.request<unknown>('/api/events')
      setEvents(listFrom<Event>(data, 'events'))
      setError(null)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch events')
    } finally {
      if (initialLoadRef.current) {
        setLoading(false)
        initialLoadRef.current = false
      }
    }
  }, [])

  useEffect(() => {
    const initialFetch = window.setTimeout(fetchEvents, 0)
    const interval = window.setInterval(fetchEvents, 5000)
    return () => {
      window.clearTimeout(initialFetch)
      window.clearInterval(interval)
    }
  }, [fetchEvents])

  return { events, loading, error, refetch: fetchEvents }
}

export function useConfigs(nodeId?: string) {
  const [configs, setConfigs] = useState<Record<string, unknown>>({})
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const initialLoadRef = useRef(true)

  const fetchConfigs = useCallback(async () => {
    try {
      if (initialLoadRef.current) {
        setLoading(true)
      }
      const url = nodeId ? `/api/configs/${nodeId}` : '/api/configs'
      const data = await api.request<unknown>(url)
      setConfigs(objectFrom(data, 'data'))
      setError(null)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch configs')
    } finally {
      if (initialLoadRef.current) {
        setLoading(false)
        initialLoadRef.current = false
      }
    }
  }, [nodeId])

  useEffect(() => {
    initialLoadRef.current = true
    const initialFetch = window.setTimeout(fetchConfigs, 0)
    return () => window.clearTimeout(initialFetch)
  }, [fetchConfigs])

  const updateConfig = async (nodeId: string, configs: Record<string, unknown>) => {
    await api.request(`/api/configs/${nodeId}`, {
      method: 'POST',
      body: JSON.stringify({ configs }),
    })
    await fetchConfigs()
  }

  return { configs, loading, error, refetch: fetchConfigs, updateConfig }
}
