import { useSessions, useFleet, useMetrics, useCommands } from '@/hooks/useApi'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Server, Boxes, Clock, Terminal } from 'lucide-react'

function formatUptime(seconds: number): string {
  const days = Math.floor(seconds / 86400)
  const hours = Math.floor((seconds % 86400) / 3600)
  const minutes = Math.floor((seconds % 3600) / 60)

  if (days > 0) return `${days}d ${hours}h`
  if (hours > 0) return `${hours}h ${minutes}m`
  return `${minutes}m`
}

export function Dashboard() {
  const { sessions, loading: sessionsLoading } = useSessions()
  const { fleet, loading: fleetLoading } = useFleet()
  const { metrics, loading: metricsLoading } = useMetrics()
  const { commands, loading: commandsLoading } = useCommands()

  const activeSessions = sessions.filter((s) => s.state === 'Active').length
  const healthyFleet = fleet.filter((n) => n.healthy).length
  const pendingCommands = commands.filter((c) => c.status === 'Pending' || c.status === 'Sent').length

  const stats = [
    {
      title: 'Control channels',
      value: metrics?.nodes.total || sessions.length,
      icon: Server,
      description: `${activeSessions} active`,
    },
    {
      title: 'Runtime fleet',
      value: fleet.length,
      icon: Boxes,
      description: `${healthyFleet} reachable`,
    },
    {
      title: 'System Uptime',
      value: metrics?.system.uptime_seconds ? formatUptime(metrics.system.uptime_seconds) : '-',
      icon: Clock,
      description: 'System running time',
    },
    {
      title: 'Pending Commands',
      value: pendingCommands,
      icon: Terminal,
      description: commands.length > 0 ? `${commands.length} total` : 'No commands',
    },
  ]

  if (sessionsLoading || fleetLoading || metricsLoading || commandsLoading) {
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
        <p className="text-muted-foreground">Control Control Overview</p>
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
            <CardTitle>Open control channels</CardTitle>
          </CardHeader>
          <CardContent>
            <div className="space-y-4">
              {sessions.slice(0, 5).map((session) => (
                <div key={session.id} className="flex items-center justify-between">
                  <div className="space-y-1">
                    <p className="text-sm font-medium">{session.node_role}</p>
                    <p className="text-xs text-muted-foreground">
                      {session.host}:{session.ports[0]}
                    </p>
                  </div>
                  <Badge variant={session.state === 'Active' ? 'success' : 'secondary'}>
                    {session.state}
                  </Badge>
                </div>
              ))}
              {sessions.length === 0 && (
                <p className="text-sm text-muted-foreground">No node is connected</p>
              )}
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Node Types Distribution</CardTitle>
          </CardHeader>
          <CardContent>
            <div className="space-y-4">
              {metrics?.nodes.by_type && Object.entries(metrics.nodes.by_type).map(([type, count]) => (
                <div key={type} className="flex items-center justify-between">
                  <div className="space-y-1">
                    <p className="text-sm font-medium">{type}</p>
                  </div>
                  <Badge variant="outline">{count}</Badge>
                </div>
              ))}
              {(!metrics?.nodes.by_type || Object.keys(metrics.nodes.by_type).length === 0) && (
                <p className="text-sm text-muted-foreground">No node types available</p>
              )}
            </div>
          </CardContent>
        </Card>
      </div>
    </div>
  )
}
