import { useWorkers, useMetrics, useCommands } from '@/hooks/useApi'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Server, Activity, Clock, Terminal } from 'lucide-react'

function formatUptime(seconds: number): string {
  const days = Math.floor(seconds / 86400)
  const hours = Math.floor((seconds % 86400) / 3600)
  const minutes = Math.floor((seconds % 3600) / 60)

  if (days > 0) return `${days}d ${hours}h`
  if (hours > 0) return `${hours}h ${minutes}m`
  return `${minutes}m`
}

export function Dashboard() {
  const { workers, loading: workersLoading } = useWorkers()
  const { metrics, loading: metricsLoading } = useMetrics()
  const { commands, loading: commandsLoading } = useCommands()

  const activeWorkers = workers.filter((w) => w.state === 'Active').length
  const pendingCommands = commands.filter((c) => c.status === 'Pending' || c.status === 'Sent').length

  const stats = [
    {
      title: 'Total Workers',
      value: metrics?.workers.total || workers.length,
      icon: Server,
      description: `${activeWorkers} active`,
    },
    {
      title: 'System Uptime',
      value: metrics?.system.uptime_seconds ? formatUptime(metrics.system.uptime_seconds) : '-',
      icon: Clock,
      description: 'System running time',
    },
    {
      title: 'Total Events',
      value: metrics?.system.total_events || 0,
      icon: Activity,
      description: 'Events recorded',
    },
    {
      title: 'Pending Commands',
      value: pendingCommands,
      icon: Terminal,
      description: commands.length > 0 ? `${commands.length} total` : 'No commands',
    },
  ]

  if (workersLoading || metricsLoading || commandsLoading) {
    return (
      <div className="flex items-center justify-center h-64">
        <p className="text-muted-foreground">Loading...</p>
      </div>
    )
  }

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-3xl font-bold">Dashboard</h1>
        <p className="text-muted-foreground">Control Supervisor Overview</p>
      </div>

      <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-4">
        {stats.map((stat) => (
          <Card key={stat.title}>
            <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
              <CardTitle className="text-sm font-medium">{stat.title}</CardTitle>
              <stat.icon className="h-4 w-4 text-muted-foreground" />
            </CardHeader>
            <CardContent>
              <div className="text-2xl font-bold">{stat.value}</div>
              <p className="text-xs text-muted-foreground">{stat.description}</p>
            </CardContent>
          </Card>
        ))}
      </div>

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>Recent Workers</CardTitle>
          </CardHeader>
          <CardContent>
            <div className="space-y-4">
              {workers.slice(0, 5).map((worker) => (
                <div key={worker.id} className="flex items-center justify-between">
                  <div className="space-y-1">
                    <p className="text-sm font-medium">{worker.worker_type}</p>
                    <p className="text-xs text-muted-foreground">{worker.host}:{worker.ports[0]}</p>
                  </div>
                  <Badge variant={worker.state === 'Active' ? 'success' : 'secondary'}>
                    {worker.state}
                  </Badge>
                </div>
              ))}
              {workers.length === 0 && (
                <p className="text-sm text-muted-foreground">No workers connected</p>
              )}
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Worker Types Distribution</CardTitle>
          </CardHeader>
          <CardContent>
            <div className="space-y-4">
              {metrics?.workers.by_type && Object.entries(metrics.workers.by_type).map(([type, count]) => (
                <div key={type} className="flex items-center justify-between">
                  <div className="space-y-1">
                    <p className="text-sm font-medium">{type}</p>
                  </div>
                  <Badge variant="outline">{count}</Badge>
                </div>
              ))}
              {(!metrics?.workers.by_type || Object.keys(metrics.workers.by_type).length === 0) && (
                <p className="text-sm text-muted-foreground">No worker types available</p>
              )}
            </div>
          </CardContent>
        </Card>
      </div>
    </div>
  )
}
