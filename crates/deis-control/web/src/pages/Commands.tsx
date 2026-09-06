import { useState } from 'react'
import { useSessions, useCommands } from '@/hooks/useApi'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from '@/components/ui/dialog'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { RefreshCw, Terminal, XCircle } from 'lucide-react'

function formatDate(dateStr: string) {
  return new Date(dateStr).toLocaleString()
}

function getCommandStatusBadgeVariant(status: string) {
  switch (status) {
    case 'Completed':
      return 'success'
    case 'Failed':
    case 'Timeout':
      return 'destructive'
    case 'Pending':
    case 'Sent':
      return 'secondary'
    case 'Acknowledged':
      return 'outline'
    default:
      return 'outline'
  }
}

interface ExecuteCommandDialogProps {
  /// Only a node holding a control channel open can be sent a command, so
  /// this is the control-channel list and not the runtime fleet.
  nodes: { id: string; node_role: string }[]
  onExecute: (nodeId: string, commandType: string, data?: unknown) => Promise<void>
}

function ExecuteCommandDialog({ nodes, onExecute }: ExecuteCommandDialogProps) {
  const [open, setOpen] = useState(false)
  const [selectedNode, setSelectedNode] = useState<string>('')
  const [commandType, setCommandType] = useState<string>('')
  const [commandData, setCommandData] = useState<string>('')
  const [executing, setExecuting] = useState(false)

  const handleExecute = async () => {
    if (!selectedNode || !commandType) return

    try {
      setExecuting(true)
      let parsedData
      if (commandData.trim()) {
        try {
          parsedData = JSON.parse(commandData)
        } catch {
          alert('Invalid JSON in command data')
          return
        }
      }

      await onExecute(selectedNode, commandType, parsedData)
      setOpen(false)
      setSelectedNode('')
      setCommandType('')
      setCommandData('')
    } catch (err) {
      console.error('Failed to execute command:', err)
      alert('Failed to execute command')
    } finally {
      setExecuting(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button size="sm">
          <Terminal className="h-4 w-4 mr-2" />
          Execute Command
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Execute Command</DialogTitle>
          <DialogDescription>
            Send a command to a specific node for execution.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-4">
          <div className="space-y-2">
            <Label htmlFor="node">Select Node</Label>
            <Select value={selectedNode} onValueChange={setSelectedNode}>
              <SelectTrigger id="node">
                <SelectValue placeholder="Choose a node..." />
              </SelectTrigger>
              <SelectContent>
                {nodes.map((node) => (
                  <SelectItem key={node.id} value={node.id}>
                    {node.node_role} ({node.id.substring(0, 8)}...)
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>

          <div className="space-y-2">
            <Label htmlFor="command-type">Command Type</Label>
            <Input
              id="command-type"
              placeholder="reload, restart, etc."
              value={commandType}
              onChange={(e) => setCommandType(e.target.value)}
            />
          </div>

          <div className="space-y-2">
            <Label htmlFor="command-data">Command Data (JSON, optional)</Label>
            <textarea
              id="command-data"
              className="w-full h-24 rounded-md border bg-background p-2 text-sm font-mono resize-none focus:outline-none focus:ring-2 focus:ring-ring"
              placeholder='{"key": "value"}'
              value={commandData}
              onChange={(e) => setCommandData(e.target.value)}
            />
          </div>
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={() => setOpen(false)}>
            Cancel
          </Button>
          <Button onClick={handleExecute} disabled={!selectedNode || !commandType || executing}>
            {executing ? 'Executing...' : 'Execute'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

export function Commands() {
  const { sessions } = useSessions()
  const { commands, total, loading, error, refetch, executeCommand, cancelCommand } = useCommands()
  const [cancelling, setCancelling] = useState<string | null>(null)

  const handleExecuteCommand = async (nodeId: string, commandType: string, data?: unknown) => {
    await executeCommand(nodeId, commandType, data)
  }

  const handleCancelCommand = async (commandId: string) => {
    try {
      setCancelling(commandId)
      await cancelCommand(commandId)
    } catch (err) {
      console.error('Failed to cancel command:', err)
    } finally {
      setCancelling(null)
    }
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-3xl font-bold">Commands</h1>
          <p className="text-muted-foreground">
            Execute and monitor node commands
            {total > commands.length ? ` - newest ${commands.length} of ${total}` : ''}
          </p>
        </div>
        <div className="flex gap-2">
          <Button variant="outline" size="sm" onClick={refetch} disabled={loading}>
            <RefreshCw className={`h-4 w-4 mr-2 ${loading ? 'animate-spin' : ''}`} />
            Refresh
          </Button>
          <ExecuteCommandDialog nodes={sessions} onExecute={handleExecuteCommand} />
        </div>
      </div>

      {error && (
        <Card className="border-destructive">
          <CardContent className="pt-6">
            <p className="text-destructive">{error}</p>
          </CardContent>
        </Card>
      )}

      {loading && commands.length === 0 ? (
        <div className="flex items-center justify-center h-64">
          <p className="text-muted-foreground">Loading commands...</p>
        </div>
      ) : commands.length === 0 ? (
        <Card>
          <CardContent className="pt-6">
            <p className="text-center text-muted-foreground">No commands executed yet</p>
          </CardContent>
        </Card>
      ) : (
        <div className="grid gap-4">
          {commands.map((command) => (
            <Card key={command.id}>
              <CardHeader className="pb-2">
                <div className="flex items-center justify-between">
                  <CardTitle className="text-lg font-mono text-sm">{command.id}</CardTitle>
                  <div className="flex items-center gap-2">
                    <Badge variant={getCommandStatusBadgeVariant(command.status)}>
                      {command.status}
                    </Badge>
                    {(command.status === 'Pending' || command.status === 'Sent') && (
                      <Button
                        variant="ghost"
                        size="icon"
                        onClick={() => handleCancelCommand(command.id)}
                        disabled={cancelling === command.id}
                      >
                        <XCircle className="h-4 w-4 text-destructive" />
                      </Button>
                    )}
                  </div>
                </div>
              </CardHeader>
              <CardContent>
                <div className="grid gap-2 text-sm md:grid-cols-2 lg:grid-cols-4">
                  <div>
                    <p className="text-muted-foreground">Node ID</p>
                    <p className="font-mono text-xs">{command.node_id}</p>
                  </div>
                  <div>
                    <p className="text-muted-foreground">Command Type</p>
                    <p>{command.command_type}</p>
                  </div>
                  <div>
                    <p className="text-muted-foreground">Created</p>
                    <p>{formatDate(command.created_at)}</p>
                  </div>
                  {command.completed_at && (
                    <div>
                      <p className="text-muted-foreground">Completed</p>
                      <p>{formatDate(command.completed_at)}</p>
                    </div>
                  )}
                </div>

                {/* `GET /api/platform/commands` and `.../node/:id` do not
                    carry the command's payload, its result or an error
                    message - see the `Command` type in `@/lib/api`. There is
                    nothing to show here until the control plane's list
                    endpoints answer with more than id/type/status/timestamps. */}
              </CardContent>
            </Card>
          ))}
        </div>
      )}
    </div>
  )
}
