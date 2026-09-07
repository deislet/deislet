import { useSessions, useFleet } from '@/hooks/useApi'
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

// The fleet's health instants are optional as well as nullable in the
// document (`NodeHealth`): a node the control plane has not probed yet has
// no `last_checked` at all, and the page shows the same dash either way.
function formatOptionalDate(dateStr: string | null | undefined) {
  return dateStr ? formatDate(dateStr) : '—'
}

function getStateBadgeVariant(state: string) {
  switch (state) {
    case 'Active':
      return 'success'
    case 'Inactive':
    case 'Disconnected':
      return 'secondary'
    // Still connected but silent past heartbeat_timeout, which is a node to
    // go and look at rather than one that simply went away.
    case 'Unresponsive':
      return 'destructive'
    case 'Error':
      return 'destructive'
    default:
      return 'outline'
  }
}

export function Nodes() {
  const { sessions, loading, error, refetch } = useSessions()
  const {
    fleet,
    loading: fleetLoading,
    error: fleetError,
    refetch: refetchFleet,
  } = useFleet()
  const [restartingNode, setRestartingNode] = useState<string | null>(null)
  const [nodeToRestart, setNodeToRestart] = useState<{ id: string; type: string } | null>(null)

  const handleRestartClick = (nodeId: string, nodeType: string) => {
    setNodeToRestart({ id: nodeId, type: nodeType })
  }

  const handleConfirmRestart = async () => {
    if (!nodeToRestart) return

    setRestartingNode(nodeToRestart.id)
    setNodeToRestart(null)

    try {
      // `restart` is one of the named command types the control plane
      // deserializes, and a node treats it as the zero-downtime upgrade this
      // dialog describes. The body is `{ command_type, data }`: it used to be
      // sent as `{ command_type: 'custom', command_data: ... }`, which the
      // server rejected with 422 before the command reached any node.
      const result = await api.executeCommand(nodeToRestart.id, {
        command_type: 'restart',
      })

      // Only now, with the control plane's own answer in hand. The command
      // is accepted, not finished: the node reports the outcome separately,
      // which is what the Commands page follows.
      toast.success('Restart accepted by the control plane', {
        description: `Sent to ${nodeToRestart.type}. Command ID: ${result.command_id}`,
      })
      await refetch()
    } catch (err) {
      toast.error('The control plane refused the restart', {
        description: err instanceof Error ? err.message : 'Unknown error',
      })
    } finally {
      setRestartingNode(null)
    }
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-3xl font-bold">Nodes</h1>
          <p className="text-muted-foreground">
            Two lists, on purpose: who is connected, and who can be given work
          </p>
        </div>
        <Button
          variant="outline"
          size="sm"
          onClick={() => {
            refetch()
            refetchFleet()
          }}
          disabled={loading || fleetLoading}
        >
          <RefreshCw className={`h-4 w-4 mr-2 ${loading || fleetLoading ? 'animate-spin' : ''}`} />
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

      {/*
        Every node holding a control channel open to this control plane. A
        command or a configuration push travels down one of these, so a node
        that is not here cannot be sent either. In practice deis-proxy is
        what appears in this list.
      */}
      <div className="space-y-2">
        <div>
          <h2 className="text-xl font-semibold">Control channels</h2>
          <p className="text-sm text-muted-foreground">
            Nodes connected to the control plane right now; commands and configuration go down
            these
          </p>
        </div>

      {loading && sessions.length === 0 ? (
        <div className="flex items-center justify-center h-64">
          <p className="text-muted-foreground">Loading control channels...</p>
        </div>
      ) : sessions.length === 0 ? (
        <Card>
          <CardContent className="pt-6">
            <p className="text-center text-muted-foreground">
              No node is holding a control channel open, so no command or configuration push can
              reach anything from here.
            </p>
          </CardContent>
        </Card>
      ) : (
        <div className="grid gap-4">
          {sessions.map((node) => (
            <Card key={node.id}>
              <CardHeader className="pb-2">
                <div className="flex items-center justify-between">
                  <CardTitle className="text-lg">{node.node_role}</CardTitle>
                  <div className="flex items-center gap-2">
                    <Badge variant={getStateBadgeVariant(node.state)}>{node.state}</Badge>
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => handleRestartClick(node.id, node.node_role)}
                      disabled={restartingNode === node.id || node.state !== 'Active'}
                      className="h-8"
                    >
                      <RotateCw className={`h-3.5 w-3.5 mr-1.5 ${restartingNode === node.id ? 'animate-spin' : ''}`} />
                      {restartingNode === node.id ? 'Restarting...' : 'Restart'}
                    </Button>
                  </div>
                </div>
              </CardHeader>
              <CardContent>
                <div className="grid gap-2 text-sm md:grid-cols-2 lg:grid-cols-4">
                  <div>
                    <p className="text-muted-foreground">ID</p>
                    <p className="font-mono text-xs">{node.id}</p>
                  </div>
                  <div>
                    <p className="text-muted-foreground">Instance</p>
                    <p className="font-mono text-xs">{node.instance_id}</p>
                  </div>
                  <div>
                    <p className="text-muted-foreground">Address</p>
                    <p>{node.host}:{node.ports.join(', ')}</p>
                  </div>
                  <div>
                    <p className="text-muted-foreground">Connected</p>
                    <p>{formatDate(node.connected_at)}</p>
                  </div>
                  <div>
                    <p className="text-muted-foreground">Last Heartbeat</p>
                    <p>{formatDate(node.last_heartbeat)}</p>
                  </div>
                </div>
              </CardContent>
            </Card>
          ))}
        </div>
      )}
      </div>

      {/*
        The runtime fleet is a different list from the one above, and the two
        have no rows in common in a normal deployment: these are the
        deis-runtime nodes the control plane dials to apply a deployment, probe
        health and fire cron. A node appears here either because the control
        plane's configuration file names it or because it registered itself,
        and once it is here it is eligible for both, whichever way it arrived.
      */}
      <div className="space-y-2">
        <div>
          <h2 className="text-xl font-semibold">Runtime fleet</h2>
          <p className="text-sm text-muted-foreground">
            Nodes that receive deployments and run cron occurrences
          </p>
        </div>

        {fleetError && (
          <Card className="border-destructive">
            <CardContent className="pt-6">
              <p className="text-destructive">{fleetError}</p>
            </CardContent>
          </Card>
        )}

        {fleetLoading && fleet.length === 0 ? (
          <Card>
            <CardContent className="pt-6">
              <p className="text-center text-muted-foreground">Loading the runtime fleet...</p>
            </CardContent>
          </Card>
        ) : fleet.length === 0 ? (
          <Card>
            <CardContent className="pt-6">
              <p className="text-center text-muted-foreground">
                No runtime node is known. Deployments and cron occurrences have nowhere to go until
                one registers or the control plane's configuration names one.
              </p>
            </CardContent>
          </Card>
        ) : (
          <div className="grid gap-4">
            {fleet.map((node) => (
              <Card key={node.id}>
                <CardHeader className="pb-2">
                  <div className="flex items-center justify-between">
                    <CardTitle className="text-lg font-mono">{node.id}</CardTitle>
                    <div className="flex items-center gap-2">
                      <Badge variant={node.source === 'registered' ? 'outline' : 'secondary'}>
                        {node.source === 'registered' ? 'self-registered' : 'from config'}
                      </Badge>
                      <Badge variant={node.healthy ? 'success' : 'destructive'}>
                        {node.healthy ? 'Healthy' : 'Unreachable'}
                      </Badge>
                    </div>
                  </div>
                </CardHeader>
                <CardContent>
                  <div className="grid gap-2 text-sm md:grid-cols-2 lg:grid-cols-4">
                    <div>
                      <p className="text-muted-foreground">Address</p>
                      <p className="font-mono text-xs">{node.addr}</p>
                    </div>
                    <div>
                      <p className="text-muted-foreground">Version</p>
                      <p>{node.version ?? '—'}</p>
                    </div>
                    <div>
                      <p className="text-muted-foreground">Last checked</p>
                      <p>{formatOptionalDate(node.last_checked)}</p>
                    </div>
                    <div>
                      <p className="text-muted-foreground">Last registered</p>
                      <p>{formatOptionalDate(node.last_registered_at)}</p>
                    </div>
                  </div>
                  {node.last_error && (
                    <p className="mt-3 text-xs text-destructive">{node.last_error}</p>
                  )}
                </CardContent>
              </Card>
            ))}
          </div>
        )}
      </div>

      {/* Restart Confirmation Dialog */}
      <Dialog open={!!nodeToRestart} onOpenChange={(open: boolean) => !open && setNodeToRestart(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Confirm Graceful Restart</DialogTitle>
            <DialogDescription>
              Are you sure you want to restart node <strong>{nodeToRestart?.type}</strong>?
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
            <Button variant="outline" onClick={() => setNodeToRestart(null)}>
              Cancel
            </Button>
            <Button onClick={handleConfirmRestart}>
              Restart Node
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  )
}
