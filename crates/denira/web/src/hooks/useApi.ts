import { useState, useEffect, useCallback, useRef } from 'react'

interface Worker {
  id: string
  instance_id: string
  worker_type: string
  host: string
  ports: number[]
  state: string
  connected_at: string
  last_heartbeat: string
  power_reserve: number
}


interface Event {
  event_type: string
  worker_id?: string
  severity: string
  message: string
  metadata?: Record<string, unknown>
  timestamp: string
}

interface MetricsSummary {
  workers: {
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

export function useWorkers() {
  const [workers, setWorkers] = useState<Worker[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const initialLoadRef = useRef(true)

  const fetchWorkers = useCallback(async () => {
    try {
      // 只在初始加载时显示 loading
      if (initialLoadRef.current) {
        setLoading(true)
      }
      const res = await fetch('/api/workers')
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const data = await res.json()
      setWorkers(data.workers || (Array.isArray(data) ? data : []))
      setError(null)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch workers')
    } finally {
      if (initialLoadRef.current) {
        setLoading(false)
        initialLoadRef.current = false
      }
    }
  }, [])

  useEffect(() => {
    fetchWorkers()
    const interval = setInterval(fetchWorkers, 5000)
    return () => clearInterval(interval)
  }, [fetchWorkers])

  return { workers, loading, error, refetch: fetchWorkers }
}

export function useCommands(workerId?: string) {
  const [commands, setCommands] = useState<any[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const initialLoadRef = useRef(true)

  const fetchCommands = useCallback(async () => {
    try {
      if (initialLoadRef.current) {
        setLoading(true)
      }
      const url = workerId ? `/api/commands/worker/${workerId}` : '/api/commands'
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
  }, [workerId])

  useEffect(() => {
    fetchCommands()
    const interval = setInterval(fetchCommands, 3000)
    return () => clearInterval(interval)
  }, [fetchCommands])

  const executeCommand = async (workerId: string, commandType: string, commandData?: any) => {
    const res = await fetch(`/api/commands/execute/${workerId}`, {
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

export function useConfigs(workerId?: string) {
  const [configs, setConfigs] = useState<Record<string, unknown>>({})
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const initialLoadRef = useRef(true)

  const fetchConfigs = useCallback(async () => {
    try {
      if (initialLoadRef.current) {
        setLoading(true)
      }
      const url = workerId ? `/api/configs/${workerId}` : '/api/configs'
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
  }, [workerId])

  useEffect(() => {
    initialLoadRef.current = true
    fetchConfigs()
  }, [fetchConfigs])

  const updateConfig = async (workerId: string, configs: Record<string, unknown>) => {
    const res = await fetch(`/api/configs/${workerId}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ configs }),
    })
    if (!res.ok) throw new Error('Failed to update config')
    await fetchConfigs()
  }

  return { configs, loading, error, refetch: fetchConfigs, updateConfig }
}
