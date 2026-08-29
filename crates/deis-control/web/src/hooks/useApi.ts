import { useState, useEffect, useCallback, useRef } from 'react'

interface NodeSummary {
  id: string
  instance_id: string
  node_role: string
  host: string
  ports: number[]
  state: string
  connected_at: string
  last_heartbeat: string
  power_reserve: number
}


/**
 * One deis-runtime node, as `/api/runtime` reports it.
 *
 * These are not the same objects as `NodeSummary` above: those are the
 * processes holding a deis-wire control channel open, while these are the
 * runtime nodes the control plane dials over gRPC to deploy versions, probe
 * health and fire cron. A node gets on this list either because it is named
 * in the control plane's configuration file or because it registered itself,
 * which is what `source` says.
 */
interface RuntimeNodeSummary {
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
      const res = await fetch('/api/metrics/summary')
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const data = await res.json()
      setMetrics(data.metrics || data)
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
    fetchMetrics()
    const interval = setInterval(fetchMetrics, 2000)
    return () => clearInterval(interval)
  }, [fetchMetrics])

  return { metrics, loading, error, refetch: fetchMetrics }
}

export function useNodes() {
  const [nodes, setNodes] = useState<NodeSummary[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const initialLoadRef = useRef(true)

  const fetchNodes = useCallback(async () => {
    try {
      // Only show the loading state on the first fetch, so background refreshes
      // do not blank out data the user is already reading
      if (initialLoadRef.current) {
        setLoading(true)
      }
      const res = await fetch('/api/nodes')
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const data = await res.json()
      setNodes(data.nodes || (Array.isArray(data) ? data : []))
      setError(null)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch nodes')
    } finally {
      if (initialLoadRef.current) {
        setLoading(false)
        initialLoadRef.current = false
      }
    }
  }, [])

  useEffect(() => {
    fetchNodes()
    const interval = setInterval(fetchNodes, 5000)
    return () => clearInterval(interval)
  }, [fetchNodes])

  return { nodes, loading, error, refetch: fetchNodes }
}

/**
 * The runtime fleet: every node that can be handed a deployment or a cron
 * occurrence, whether it came from the configuration file or registered
 * itself.
 */
export function useRuntimeNodes() {
  const [nodes, setNodes] = useState<RuntimeNodeSummary[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const initialLoadRef = useRef(true)

  const fetchNodes = useCallback(async () => {
    try {
      if (initialLoadRef.current) {
        setLoading(true)
      }
      const res = await fetch('/api/runtime')
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const data = await res.json()
      setNodes(data.nodes || (Array.isArray(data) ? data : []))
      setError(null)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch runtime nodes')
    } finally {
      if (initialLoadRef.current) {
        setLoading(false)
        initialLoadRef.current = false
      }
    }
  }, [])

  useEffect(() => {
    fetchNodes()
    const interval = setInterval(fetchNodes, 5000)
    return () => clearInterval(interval)
  }, [fetchNodes])

  return { runtimeNodes: nodes, loading, error, refetch: fetchNodes }
}

export function useCommands(nodeId?: string) {
  const [commands, setCommands] = useState<any[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const initialLoadRef = useRef(true)

  const fetchCommands = useCallback(async () => {
    try {
      if (initialLoadRef.current) {
        setLoading(true)
      }
      const url = nodeId ? `/api/commands/node/${nodeId}` : '/api/commands'
      const res = await fetch(url)
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const data = await res.json()
      setCommands(data.commands || (Array.isArray(data) ? data : []))
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
    fetchCommands()
    const interval = setInterval(fetchCommands, 3000)
    return () => clearInterval(interval)
  }, [fetchCommands])

  const executeCommand = async (nodeId: string, commandType: string, commandData?: any) => {
    const res = await fetch(`/api/commands/execute/${nodeId}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ command_type: commandType, command_data: commandData }),
    })
    if (!res.ok) throw new Error('Failed to execute command')
    await fetchCommands()
    return await res.json()
  }

  const cancelCommand = async (commandId: string) => {
    const res = await fetch(`/api/commands/cancel/${commandId}`, {
      method: 'POST',
    })
    if (!res.ok) throw new Error('Failed to cancel command')
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
      const res = await fetch('/api/events')
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const data = await res.json()
      setEvents(data.events || (Array.isArray(data) ? data : []))
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
    fetchEvents()
    const interval = setInterval(fetchEvents, 5000)
    return () => clearInterval(interval)
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
      const res = await fetch(url)
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const data = await res.json()
      setConfigs(data.data || data || {})
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
    fetchConfigs()
  }, [fetchConfigs])

  const updateConfig = async (nodeId: string, configs: Record<string, unknown>) => {
    const res = await fetch(`/api/configs/${nodeId}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ configs }),
    })
    if (!res.ok) throw new Error('Failed to update config')
    await fetchConfigs()
  }

  return { configs, loading, error, refetch: fetchConfigs, updateConfig }
}
