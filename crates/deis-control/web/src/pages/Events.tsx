import { useEvents } from '@/hooks/useApi'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { ScrollArea } from '@/components/ui/scroll-area'
import { RefreshCw } from 'lucide-react'

function getSeverityBadgeVariant(severity: string) {
  switch (severity) {
    case 'Error':
      return 'destructive'
    case 'Warning':
      return 'warning'
    case 'Info':
      return 'success'
    default:
      return 'secondary'
  }
}

export function Events() {
  const { events, loading, error, refetch } = useEvents()

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-3xl font-bold">Events</h1>
          <p className="text-muted-foreground">System event log</p>
        </div>
        <Button variant="outline" size="sm" onClick={refetch} disabled={loading}>
          <RefreshCw className={`h-4 w-4 mr-2 ${loading ? 'animate-spin' : ''}`} />
          Refresh
        </Button>
      </div>

      {error && (
        <Card className="border-destructive">
          <CardContent className="pt-6">
            <p className="text-destructive">{error}</p>
          </CardContent>
        </Card>
      )}

      <Card>
        <CardHeader>
          <CardTitle>Recent Events</CardTitle>
        </CardHeader>
        <CardContent>
          {loading && events.length === 0 ? (
            <p className="text-muted-foreground">Loading events...</p>
          ) : events.length === 0 ? (
            <p className="text-muted-foreground">No events recorded</p>
          ) : (
            <ScrollArea className="h-[600px]">
              <div className="space-y-4">
                {events.map((event, index) => (
                  <div
                    key={index}
                    className="flex items-start gap-4 rounded-md border p-4"
                  >
                    <div className="flex flex-col gap-2">
                      <Badge variant={getSeverityBadgeVariant(event.severity)}>
                        {event.severity}
                      </Badge>
                      <Badge variant="outline" className="text-xs">
                        {event.event_type}
                      </Badge>
                    </div>
                    <div className="flex-1 space-y-2">
                      <div className="flex items-center justify-between">
                        <p className="text-sm font-medium">{event.message}</p>
                        <p className="text-xs text-muted-foreground">
                          {new Date(event.timestamp).toLocaleString()}
                        </p>
                      </div>
                      {event.worker_id && (
                        <p className="text-xs text-muted-foreground">
                          Worker: <span className="font-mono">{event.worker_id}</span>
                        </p>
                      )}
                      {event.metadata && Object.keys(event.metadata).length > 0 && (
                        <pre className="mt-2 rounded bg-muted p-2 text-xs overflow-auto max-h-32">
                          {JSON.stringify(event.metadata, null, 2)}
                        </pre>
                      )}
                    </div>
                  </div>
                ))}
              </div>
            </ScrollArea>
          )}
        </CardContent>
      </Card>
    </div>
  )
}
