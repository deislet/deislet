import { useState } from 'react'
import { useWorkers, useConfigs } from '@/hooks/useApi'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'

import { RefreshCw, Save } from 'lucide-react'

export function Configs() {
  const { workers } = useWorkers()
  const [selectedWorker, setSelectedWorker] = useState<string | null>(null)
  const { configs, loading, error, refetch, updateConfig } = useConfigs(selectedWorker || undefined)
  const [editedConfigs, setEditedConfigs] = useState<string>('')
  const [saving, setSaving] = useState(false)

  const handleWorkerSelect = (workerId: string) => {
    setSelectedWorker(workerId)
    setEditedConfigs('')
  }

  const handleSave = async () => {
    if (!selectedWorker) return
    try {
      setSaving(true)
      const parsed = JSON.parse(editedConfigs)
      await updateConfig(selectedWorker, parsed)
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
          <p className="text-muted-foreground">Manage worker configurations</p>
        </div>
        <Button variant="outline" size="sm" onClick={refetch} disabled={loading}>
          <RefreshCw className={`h-4 w-4 mr-2 ${loading ? 'animate-spin' : ''}`} />
          Refresh
        </Button>
      </div>

      <div className="grid gap-4 md:grid-cols-4">
        <Card className="md:col-span-1">
          <CardHeader>
            <CardTitle className="text-base">Workers</CardTitle>
          </CardHeader>
          <CardContent>
            <div className="space-y-2">
              {workers.map((worker) => (
                <Button
                  key={worker.id}
                  variant={selectedWorker === worker.id ? 'default' : 'ghost'}
                  className="w-full justify-start text-left"
                  onClick={() => handleWorkerSelect(worker.id)}
                >
                  <div className="truncate">
                    <p className="text-sm font-medium">{worker.worker_type}</p>
                    <p className="text-xs text-muted-foreground truncate">{worker.id}</p>
                  </div>
                </Button>
              ))}
              {workers.length === 0 && (
                <p className="text-sm text-muted-foreground">No workers connected</p>
              )}
            </div>
          </CardContent>
        </Card>

        <Card className="md:col-span-3">
          <CardHeader className="flex flex-row items-center justify-between">
            <CardTitle className="text-base">
              {selectedWorker ? 'Configuration' : 'Select a worker'}
            </CardTitle>
            {selectedWorker && editedConfigs && (
              <Button size="sm" onClick={handleSave} disabled={saving}>
                <Save className="h-4 w-4 mr-2" />
                Save
              </Button>
            )}
          </CardHeader>
          <CardContent>
            {error && <p className="text-destructive mb-4">{error}</p>}
            
            {!selectedWorker ? (
              <p className="text-muted-foreground">Select a worker to view its configuration</p>
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
