import { useState } from 'react'
import { useSessions, useConfigs } from '@/hooks/useApi'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { RefusalNotice } from '@/components/RefusalNotice'

import { RefreshCw, Save } from 'lucide-react'

export function Configs() {
  const { sessions: nodes } = useSessions()
  const [selectedNode, setSelectedNode] = useState<string | null>(null)
  const { configs, loading, error, refetch, updateConfig } = useConfigs(selectedNode || undefined)
  const [editedConfigs, setEditedConfigs] = useState<string>('')
  const [saving, setSaving] = useState(false)

  const handleNodeSelect = (nodeId: string) => {
    setSelectedNode(nodeId)
    setEditedConfigs('')
  }

  const handleSave = async () => {
    if (!selectedNode) return
    try {
      setSaving(true)
      const parsed = JSON.parse(editedConfigs)
      await updateConfig(selectedNode, parsed)
      setEditedConfigs('')
    } catch (err) {
      console.error('Failed to save config:', err)
    } finally {
      setSaving(false)
    }
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-3xl font-bold">Configs</h1>
          <p className="text-muted-foreground">Manage node configurations</p>
        </div>
        <Button variant="outline" size="sm" onClick={refetch} disabled={loading}>
          <RefreshCw className={`h-4 w-4 mr-2 ${loading ? 'animate-spin' : ''}`} />
          Refresh
        </Button>
      </div>

      <div className="grid gap-4 md:grid-cols-4">
        <Card className="md:col-span-1">
          <CardHeader>
            <CardTitle className="text-base">Nodes</CardTitle>
          </CardHeader>
          <CardContent>
            <div className="space-y-2">
              {nodes.map((node) => (
                <Button
                  key={node.id}
                  variant={selectedNode === node.id ? 'default' : 'ghost'}
                  className="w-full justify-start text-left"
                  onClick={() => handleNodeSelect(node.id)}
                >
                  <div className="truncate">
                    <p className="text-sm font-medium">{node.node_role}</p>
                    <p className="text-xs text-muted-foreground truncate">{node.id}</p>
                  </div>
                </Button>
              ))}
              {nodes.length === 0 && (
                <p className="text-sm text-muted-foreground">No nodes connected</p>
              )}
            </div>
          </CardContent>
        </Card>

        <Card className="md:col-span-3">
          <CardHeader className="flex flex-row items-center justify-between">
            <CardTitle className="text-base">
              {selectedNode ? 'Configuration' : 'Select a node'}
            </CardTitle>
            {selectedNode && editedConfigs && (
              <Button size="sm" onClick={handleSave} disabled={saving}>
                <Save className="h-4 w-4 mr-2" />
                Save
              </Button>
            )}
          </CardHeader>
          <CardContent>
            {error && (
              <div className="mb-4">
                <RefusalNotice refusal={error} onRetry={refetch} />
              </div>
            )}
            
            {!selectedNode ? (
              <p className="text-muted-foreground">Select a node to view its configuration</p>
            ) : loading ? (
              <p className="text-muted-foreground">Loading configuration...</p>
            ) : (
              <div className="space-y-4">
                <div>
                  <label className="text-sm font-medium">Current Configuration</label>
                  <pre className="mt-2 rounded-md bg-muted p-4 text-sm overflow-auto max-h-64">
                    {JSON.stringify(configs, null, 2) || 'No configuration'}
                  </pre>
                </div>
                
                <div>
                  <label className="text-sm font-medium">Edit Configuration (JSON)</label>
                  <textarea
                    className="mt-2 w-full h-48 rounded-md border bg-background p-4 text-sm font-mono resize-none focus:outline-none focus:ring-2 focus:ring-ring"
                    placeholder='{"key": "value"}'
                    value={editedConfigs}
                    onChange={(e) => setEditedConfigs(e.target.value)}
                  />
                </div>
              </div>
            )}
          </CardContent>
        </Card>
      </div>
    </div>
  )
}
