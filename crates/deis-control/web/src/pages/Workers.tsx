import { useWorkers } from '@/hooks/useApi'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { RefreshCw, RotateCw } from 'lucide-react'
import { useState } from 'react'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { api } from '@/lib/api'
import { toast } from 'sonner'

function formatDate(dateStr: string) {
  return new Date(dateStr).toLocaleString()
}

function getStateBadgeVariant(state: string) {
  switch (state) {
    case 'Active':
      return 'success'
    case 'Inactive':
    case 'Disconnected':
      return 'secondary'
    case 'Error':
      return 'destructive'
    default:
      return 'outline'
  }
}

export function Workers() {
  const { workers, loading, error, refetch } = useWorkers()
  const [restartingWorker, setRestartingWorker] = useState<string | null>(null)
  const [workerToRestart, setWorkerToRestart] = useState<{ id: string; type: string } | null>(null)

  const handleRestartClick = (workerId: string, workerType: string) => {
    setWorkerToRestart({ id: workerId, type: workerType })
  }

  const handleConfirmRestart = async () => {
    if (!workerToRestart) return

    setRestartingWorker(workerToRestart.id)
    setWorkerToRestart(null)

    try {
      const result = await api.executeCommand(workerToRestart.id, {
        command_type: 'custom',
        command_data: 'graceful_restart',
      })

      toast.success('Graceful restart initiated', {
        description: `Worker ${workerToRestart.type} is restarting. Command ID: ${result.command_id}`,
      })

      // 延迟一下再刷新，给 worker 时间处理
      setTimeout(() => {
        refetch()
        setRestartingWorker(null)
      }, 2000)
    } catch (err) {
      toast.error('Failed to restart worker', {
        description: err instanceof Error ? err.message : 'Unknown error',
      })
      setRestartingWorker(null)
    }
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-3xl font-bold">Workers</h1>
          <p className="text-muted-foreground">Manage connected worker nodes</p>
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

      {loading && workers.length === 0 ? (
        <div className="flex items-center justify-center h-64">
          <p className="text-muted-foreground">Loading workers...</p>
        </div>
      ) : workers.length === 0 ? (
        <Card>
          <CardContent className="pt-6">
            <p className="text-center text-muted-foreground">No workers connected</p>
          </CardContent>
        </Card>
      ) : (
        <div className="grid gap-4">
          {workers.map((worker) => (
            <Card key={worker.id}>
              <CardHeader className="pb-2">
                <div className="flex items-center justify-between">
                  <CardTitle className="text-lg">{worker.worker_type}</CardTitle>
                  <div className="flex items-center gap-2">
                    <Badge variant={getStateBadgeVariant(worker.state)}>{worker.state}</Badge>
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => handleRestartClick(worker.id, worker.worker_type)}
                      disabled={restartingWorker === worker.id || worker.state !== 'Active'}
                      className="h-8"
                    >
                      <RotateCw className={`h-3.5 w-3.5 mr-1.5 ${restartingWorker === worker.id ? 'animate-spin' : ''}`} />
                      {restartingWorker === worker.id ? 'Restarting...' : 'Restart'}
                    </Button>
                  </div>
                </div>
              </CardHeader>
              <CardContent>
                <div className="grid gap-2 text-sm md:grid-cols-2 lg:grid-cols-4">
                  <div>
                    <p className="text-muted-foreground">ID</p>
                    <p className="font-mono text-xs">{worker.id}</p>
                  </div>
                  <div>
                    <p className="text-muted-foreground">Instance</p>
                    <p className="font-mono text-xs">{worker.instance_id}</p>
                  </div>
                  <div>
                    <p className="text-muted-foreground">Address</p>
                    <p>{worker.host}:{worker.ports.join(', ')}</p>
                  </div>
                  <div>
                    <p className="text-muted-foreground">Connected</p>
                    <p>{formatDate(worker.connected_at)}</p>
                  </div>
                  <div>
                    <p className="text-muted-foreground">Last Heartbeat</p>
                    <p>{formatDate(worker.last_heartbeat)}</p>
                  </div>
                  <div>
                    <p className="text-muted-foreground">Power Reserve</p>
                    <p>{worker.power_reserve}</p>
                  </div>
                </div>
              </CardContent>
            </Card>
          ))}
        </div>
      )}

      {/* Restart Confirmation Dialog */}
      <Dialog open={!!workerToRestart} onOpenChange={(open: boolean) => !open && setWorkerToRestart(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Confirm Graceful Restart</DialogTitle>
            <DialogDescription>
              Are you sure you want to restart worker <strong>{workerToRestart?.type}</strong>?
              <br /><br />
              This will initiate a zero-downtime graceful restart:
              <ul className="list-disc list-inside mt-2 space-y-1">
                <li>New process will start and inherit listener FDs</li>
                <li>Existing connections will continue uninterrupted</li>
                <li>Old process will gracefully shutdown after transfer</li>
              </ul>
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setWorkerToRestart(null)}>
              Cancel
            </Button>
            <Button onClick={handleConfirmRestart}>
              Restart Worker
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  )
}
