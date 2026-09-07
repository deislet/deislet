import { useState, useEffect, useCallback, useRef } from 'react'
import { api } from '@/lib/api'
import type { Command, Event, FleetNode, SessionSummary } from '@/lib/api'
import { describeRefusal, nextPollDelayMs, type RefusalState } from '@/lib/refusal'

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
// stops the five polls below from reading a control plane that is refusing
// all of them.
//
// The two list hooks (`useCommands`, `useEvents`) read the first page of the
// unified list envelope on every tick and stop there: the newest rows are
// what a live view shows, and walking a history with no ceiling every few
// seconds is what `page.nextCursor` exists to spare the control plane. A
// view that needs the rest reads it once through `listAll` in `@/lib/api`.
//
// What a refused read leaves behind is a `RefusalState`, not a sentence.
// Every one of these hooks used to answer `err.message`, which flattened a
// scope refusal, a conflict, a rate limit and a crashed control plane into
// one red line and told the operator nothing about which of them they had.
// `describeRefusal` in `@/lib/refusal` draws that distinction once and
// `RefusalNotice` renders it; the rows already on screen are left alone,
// because a stale list under an explicit "this stopped reading" banner is
// more use than an empty page.

/**
 * The empty lists a page renders before its first answer arrives. Module
 * constants rather than fresh `[]` literals, so that a page holding one in a
 * dependency array does not see a new list on every render.
 */
const NO_SESSIONS: SessionSummary[] = []
const NO_NODES: FleetNode[] = []
const NO_COMMANDS: Command[] = []
const NO_EVENTS: Event[] = []
const NO_CONFIGS: Record<string, unknown> = {}

/** What every hook below answers, around whatever it read. */
interface PolledRead<T> {
  data: T | null
  loading: boolean
  error: RefusalState | null
  refetch: () => Promise<void>
}

/**
 * Read one route now, and keep re-reading it.
 *
 * `read` must be stable across renders - wrap it in `useCallback` - because
 * it is what the polling effect is keyed on. `subject` names what is being
 * read, as a noun phrase, so that a refusal can say which of a page's two
 * lists went quiet. `intervalMs` of `null` reads once and stops.
 *
 * Each tick is scheduled after the previous one has finished rather than on
 * a fixed interval. Two things follow, and both are the point: a control
 * plane that is answering slowly is never asked twice concurrently, and a
 * refusal can push the next attempt out - by exactly as long as the server
 * asked for on a 429, and by doubling otherwise (`nextPollDelayMs`). A
 * console that keeps knocking every two seconds through a rate limit is
 * part of the problem it is displaying.
 */
function usePolledRead<T>(
  read: () => Promise<T>,
  subject: string,
  intervalMs: number | null
): PolledRead<T> {
  const [data, setData] = useState<T | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<RefusalState | null>(null)
  // Only the first read shows the loading state, so that a background
  // refresh does not blank out rows the operator is reading.
  const initialLoadRef = useRef(true)
  // How the next delay is chosen. Refs, not state: the scheduler reads them
  // right after an attempt finishes, and neither belongs in a render.
  const refusalsRef = useRef(0)
  const lastRefusalRef = useRef<RefusalState | null>(null)

  const readOnce = useCallback(async () => {
    try {
      if (initialLoadRef.current) {
        setLoading(true)
      }
      const value = await read()
      setData(value)
      setError(null)
      refusalsRef.current = 0
      lastRefusalRef.current = null
    } catch (err) {
      const refusal = describeRefusal(err, subject)
      refusalsRef.current += 1
      lastRefusalRef.current = refusal
      setError(refusal)
    } finally {
      if (initialLoadRef.current) {
        setLoading(false)
        initialLoadRef.current = false
      }
    }
  }, [read, subject])

  useEffect(() => {
    // A new `read` is a different question - another node's configuration,
    // say - so its first answer gets the loading state again.
    initialLoadRef.current = true
    refusalsRef.current = 0
    lastRefusalRef.current = null
    let stopped = false
    let timer = 0
    const tick = () => {
      void readOnce().then(() => {
        if (stopped || intervalMs === null) return
        const delay = nextPollDelayMs(intervalMs, refusalsRef.current, lastRefusalRef.current)
        timer = window.setTimeout(tick, delay)
      })
    }
    timer = window.setTimeout(tick, 0)
    return () => {
      stopped = true
      window.clearTimeout(timer)
    }
  }, [readOnce, intervalMs])

  return { data, loading, error, refetch: readOnce }
}

/** The control plane's own counters, re-read every couple of seconds. */
export function useMetrics() {
  const read = useCallback(() => api.getMetricsSummary(), [])
  const { data, loading, error, refetch } = usePolledRead(read, 'the metrics summary', 2000)
  return { metrics: data?.metrics ?? null, loading, error, refetch }
}

/**
 * The control channels open to the control plane: every node that
 * authenticated, registered and is heartbeating right now. Commands and
 * configuration pushes travel down these, and nothing else can receive one.
 */
export function useSessions() {
  const read = useCallback(() => api.getSessions(), [])
  const { data, loading, error, refetch } = usePolledRead(read, 'the control channels', 5000)
  return { sessions: data?.sessions ?? NO_SESSIONS, loading, error, refetch }
}

/**
 * The runtime fleet: every node that can be handed a deployment or a cron
 * occurrence, whether it came from the configuration file or registered
 * itself.
 */
export function useFleet() {
  const read = useCallback(() => api.getFleet(), [])
  const { data, loading, error, refetch } = usePolledRead(read, 'the runtime fleet', 5000)
  return { fleet: data?.nodes ?? NO_NODES, loading, error, refetch }
}

/**
 * The newest page of the command history - the whole fleet's, or one
 * node's - re-read every few seconds. `total` is how many the history holds
 * behind this page, counted by the control plane in the same read.
 */
export function useCommands(nodeId?: string) {
  const read = useCallback(
    () => (nodeId ? api.getNodeCommands(nodeId) : api.getCommands()),
    [nodeId]
  )
  const { data, loading, error, refetch } = usePolledRead(read, 'the command history', 3000)

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
    await refetch()
    return result
  }

  const cancelCommand = async (commandId: string) => {
    await api.cancelCommand(commandId)
    await refetch()
  }

  return {
    commands: data?.items ?? NO_COMMANDS,
    total: data?.page.total ?? 0,
    loading,
    error,
    refetch,
    executeCommand,
    cancelCommand,
  }
}

/**
 * The newest page of the event log, re-read every few seconds. `total` is
 * how many events the log holds behind this page.
 */
export function useEvents() {
  const read = useCallback(() => api.getEvents(), [])
  const { data, loading, error, refetch } = usePolledRead(read, 'the event log', 5000)
  return {
    events: data?.items ?? NO_EVENTS,
    total: data?.page.total ?? 0,
    loading,
    error,
    refetch,
  }
}

/**
 * One node's configuration, or every node's. Read once when the selection
 * changes rather than polled: an operator is editing this, and a poll that
 * overwrote the editor under their hands would be worse than a stale read.
 */
export function useConfigs(nodeId?: string) {
  // One node's configuration is keyed by config key, every node's is keyed
  // by node id, and this page renders whichever it got as JSON. The return
  // type is named rather than inferred so that the two answers meet in the
  // one shape the page reads instead of the hook being fixed to whichever
  // branch came first.
  const read = useCallback(
    (): Promise<{ data: Record<string, unknown> }> =>
      nodeId ? api.getNodeConfig(nodeId) : api.getConfigs(),
    [nodeId]
  )
  const { data, loading, error, refetch } = usePolledRead(read, 'the configuration', null)

  const updateConfig = async (nodeId: string, configs: Record<string, unknown>) => {
    await api.updateNodeConfig(nodeId, configs)
    await refetch()
  }

  return { configs: data?.data ?? NO_CONFIGS, loading, error, refetch, updateConfig }
}
