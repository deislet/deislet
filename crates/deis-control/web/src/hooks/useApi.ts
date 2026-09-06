import { useState, useEffect, useCallback, useRef } from 'react'
import { api } from '@/lib/api'
import type { Command, Event, FleetNode, MetricsSummary, SessionSummary } from '@/lib/api'

// Every request below goes through `api` rather than a bare `fetch`. These
// hooks used to call `fetch` directly, which meant they sent no `X-API-Key`:
// with `api_auth_enabled = true` the whole console answered 401 on every
// poll. One request layer, one place that carries the key, handles a 401,
// reads the control plane's error envelope and puts the `/api/platform`
// prefix on - every route these hooks read lives there.
//
// These hooks never mount without a credential the control plane has already
// accepted: App.tsx asks one question before it renders them and shows the key
// screen instead when the answer is 401 (see `Access` there). That is what
// stops the five intervals below from polling a control plane that is refusing
// all of them.
//
// The two list hooks (`useCommands`, `useEvents`) read the first page of the
// unified list envelope on every tick and stop there: the newest rows are
// what a live view shows, and walking a history with no ceiling every few
// seconds is what `page.nextCursor` exists to spare the control plane. A
// view that needs the rest reads it once through `listAll` in `@/lib/api`.

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
      const data = await api.getMetricsSummary()
      setMetrics(data.metrics)
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
      const data = await api.getSessions()
      setSessions(data.sessions)
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
      const data = await api.getFleet()
      setNodes(data.nodes)
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

/**
 * The newest page of the command history - the whole fleet's, or one
 * node's - re-read every few seconds. `total` is how many the history holds
 * behind this page, counted by the control plane in the same read.
 */
export function useCommands(nodeId?: string) {
  const [commands, setCommands] = useState<Command[]>([])
  const [total, setTotal] = useState(0)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const initialLoadRef = useRef(true)

  const fetchCommands = useCallback(async () => {
    try {
      if (initialLoadRef.current) {
        setLoading(true)
      }
      const page = nodeId ? await api.getNodeCommands(nodeId) : await api.getCommands()
      setCommands(page.items)
      setTotal(page.page.total)
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
    ] as const
    const isNamed = (name: string): name is (typeof named)[number] =>
      (named as readonly string[]).includes(name)
    const command_type = isNamed(commandType) ? commandType : { custom: commandType }
    const result = await api.executeCommand(nodeId, { command_type, data })
    await fetchCommands()
    return result
  }

  const cancelCommand = async (commandId: string) => {
    await api.cancelCommand(commandId)
    await fetchCommands()
  }

  return { commands, total, loading, error, refetch: fetchCommands, executeCommand, cancelCommand }
}

/**
 * The newest page of the event log, re-read every few seconds. `total` is
 * how many events the log holds behind this page.
 */
export function useEvents() {
  const [events, setEvents] = useState<Event[]>([])
  const [total, setTotal] = useState(0)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const initialLoadRef = useRef(true)

  const fetchEvents = useCallback(async () => {
    try {
      if (initialLoadRef.current) {
        setLoading(true)
      }
      const page = await api.getEvents()
      setEvents(page.items)
      setTotal(page.page.total)
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

  return { events, total, loading, error, refetch: fetchEvents }
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
      const data = nodeId ? await api.getNodeConfig(nodeId) : await api.getConfigs()
      setConfigs(data.data)
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
    await api.updateNodeConfig(nodeId, configs)
    await fetchConfigs()
  }

  return { configs, loading, error, refetch: fetchConfigs, updateConfig }
}
