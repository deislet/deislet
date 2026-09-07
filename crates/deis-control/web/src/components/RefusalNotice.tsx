import {
  CircleAlert,
  CloudOff,
  Hourglass,
  KeyRound,
  RefreshCw,
  SearchX,
  ServerCrash,
  ShieldX,
  TriangleAlert,
} from 'lucide-react'
import type { LucideIcon } from 'lucide-react'
import { api } from '@/lib/api'
import type { RefusalKind, RefusalState } from '@/lib/refusal'
import { Card, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'

/**
 * What the console shows when a read was refused.
 *
 * Every page used to render `<p className="text-destructive">{error}</p>` -
 * one red sentence for a scope refusal, a conflict, a rate limit and a
 * crashed control plane alike. They are not the same event and they do not
 * want the same thing from the operator, so this component says which one
 * happened, what to do about it, and - where the answer is "nothing, wait" -
 * that the console is already waiting and for how long.
 *
 * The rows behind this notice are deliberately left on screen. A list that
 * was read successfully thirty seconds ago is still worth looking at; what
 * the operator must not be left believing is that it is current.
 */

/**
 * Whether a refusal will lift on its own. It decides the colour: red for the
 * ones that need somebody to do something, amber for the ones the console is
 * sitting out.
 */
const TRANSIENT: ReadonlySet<RefusalKind> = new Set<RefusalKind>([
  'offline',
  'rate-limited',
  'unavailable',
  'server',
])

const ICONS: Record<RefusalKind, LucideIcon> = {
  offline: CloudOff,
  unauthorized: KeyRound,
  forbidden: ShieldX,
  missing: SearchX,
  conflict: TriangleAlert,
  'rate-limited': Hourglass,
  unavailable: ServerCrash,
  server: ServerCrash,
  refused: CircleAlert,
}

/** What the operator can do about it, in one sentence per class. */
function nextStep(refusal: RefusalState): string {
  switch (refusal.kind) {
    case 'offline':
      return 'The console keeps trying; check that the control plane is up and reachable from this browser.'
    case 'unauthorized':
      return 'The stored key is no longer accepted. Sign in again with a current one.'
    case 'forbidden':
      return refusal.code === 'insufficient-scope'
        ? 'The key is valid but its scopes do not cover this read. Use a key that has them, or have this one widened.'
        : 'The control plane will not serve this read to this caller.'
    case 'missing':
      return 'The route or the resource is gone. A control plane older than this console can answer this way.'
    case 'conflict':
      return 'Refresh to read the current state before acting on what is on screen.'
    case 'rate-limited':
      return refusal.retryAfterSeconds === undefined
        ? 'The console has slowed down on its own and will pick up again shortly.'
        : `The control plane asked for ${refusal.retryAfterSeconds}s; the console is waiting that long before reading again.`
    case 'unavailable':
      return 'A dependency the control plane needs is not answering. The console keeps trying, more slowly.'
    case 'server':
      return 'The control plane logged the failure under the request id below. The console keeps trying, more slowly.'
    case 'refused':
      return 'The control plane refused the request as it was made.'
  }
}

interface RefusalNoticeProps {
  refusal: RefusalState
  /** Read again now. Omitted where the page has no refresh of its own. */
  onRetry?: () => void
}

export function RefusalNotice({ refusal, onRetry }: RefusalNoticeProps) {
  const transient = TRANSIENT.has(refusal.kind)
  const Icon = ICONS[refusal.kind]
  const tone = transient ? 'text-warning' : 'text-destructive'

  return (
    <Card className={transient ? 'border-warning' : 'border-destructive'}>
      <CardContent className="pt-6">
        <div className="flex items-start gap-3">
          <Icon className={`mt-0.5 h-5 w-5 shrink-0 ${tone}`} />
          <div className="min-w-0 flex-1 space-y-1">
            <p className={`font-medium ${tone}`}>{refusal.title}</p>
            <p className="text-sm text-muted-foreground">{refusal.message}</p>
            <p className="text-sm text-muted-foreground">{nextStep(refusal)}</p>
            {refusal.violations.length > 0 && (
              <ul className="list-disc pl-5 text-sm text-muted-foreground">
                {refusal.violations.map((violation) => (
                  <li key={`${violation.field}:${violation.code}`}>
                    <span className="font-mono">{violation.field}</span> - {violation.message}
                  </li>
                ))}
              </ul>
            )}
            <p className="pt-1 font-mono text-xs text-muted-foreground">
              {/* The two things an operator needs to hand to whoever reads
                  the control plane's log. `status 0` is this browser never
                  getting an answer, which has no code and no request id. */}
              {refusal.status === 0 ? 'no answer' : `HTTP ${refusal.status}`}
              {refusal.code ? ` - ${refusal.code}` : ''}
              {refusal.requestId ? ` - request ${refusal.requestId}` : ''}
            </p>
          </div>
          <div className="flex shrink-0 gap-2">
            {onRetry && (
              <Button variant="outline" size="sm" onClick={onRetry}>
                <RefreshCw className="mr-2 h-4 w-4" />
                Retry
              </Button>
            )}
            {(refusal.kind === 'unauthorized' || refusal.kind === 'forbidden') && (
              <Button variant="outline" size="sm" onClick={() => api.signOut()}>
                <KeyRound className="mr-2 h-4 w-4" />
                Use another key
              </Button>
            )}
          </div>
        </div>
      </CardContent>
    </Card>
  )
}
