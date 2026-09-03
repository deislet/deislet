import targetStatusDocument from '@/generated/target-status.json'
import { Badge } from '@/components/ui/badge'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'

interface TargetStatus {
  generatedFrom: {
    repository: string
    sourceCommit: string
    registryPath: string
    registrySha256: string
    registryUpdatedAt: string
  }
  standard: {
    id: string
    version: string
    releaseStatus: string
    normativeRelease: boolean
    certificationUnit: string
    requiredMaturity: string
  }
  backends: Array<{
    id: string
    displayName: string
    role: string
    implementation: {
      repository: string
      sourceCommit: string
      scope: string
    }
    evidence: {
      maturity: string
      observedAt: string
      outcome: string
      records: Array<{
        kind: string
        reference: string
        note: string
      }>
    }
    certification: {
      status: string
      compliant: boolean
      supported: boolean
      validUntil: string | null
      blockers: string[]
    }
  }>
}

const status = targetStatusDocument as TargetStatus

function yesNo(value: boolean): string {
  return value ? 'Yes' : 'No'
}

export function Targets() {
  const registryUrl = `${status.generatedFrom.repository}/blob/${status.generatedFrom.sourceCommit}/${status.generatedFrom.registryPath}`

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-3xl font-bold">Target evidence</h1>
        <p className="text-muted-foreground">
          Registry evidence and whole-standard certification are shown separately.
        </p>
      </div>

      <Card>
        <CardHeader>
          <div className="flex flex-wrap items-center gap-2">
            <CardTitle>{status.standard.id}</CardTitle>
            <Badge variant={status.standard.normativeRelease ? 'success' : 'warning'}>
              {status.standard.releaseStatus}
            </Badge>
          </div>
          <CardDescription>
            Version {status.standard.version}; certification unit: {status.standard.certificationUnit};
            required evidence: {status.standard.requiredMaturity}.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-2 text-sm">
          <p>
            Registry observed {status.generatedFrom.registryUpdatedAt}. Snapshot SHA-256:{' '}
            <code className="break-all text-xs">{status.generatedFrom.registrySha256}</code>
          </p>
          <a
            className="text-primary underline underline-offset-4"
            href={registryUrl}
            target="_blank"
            rel="noreferrer"
          >
            Open the exact locked registry
          </a>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Targets</CardTitle>
          <CardDescription>
            Artifact generation or deployment evidence does not mean compliant or supported.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Target</TableHead>
                <TableHead>Role</TableHead>
                <TableHead>Evidence</TableHead>
                <TableHead>Certification</TableHead>
                <TableHead>Compliant</TableHead>
                <TableHead>Supported</TableHead>
                <TableHead>Valid until</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {status.backends.map((backend) => (
                <TableRow key={backend.id}>
                  <TableCell>
                    <div className="font-medium">{backend.displayName}</div>
                    <code className="text-xs text-muted-foreground">{backend.id}</code>
                  </TableCell>
                  <TableCell>{backend.role}</TableCell>
                  <TableCell>
                    <div>{backend.evidence.maturity}</div>
                    <div className="text-xs text-muted-foreground">{backend.evidence.outcome}</div>
                  </TableCell>
                  <TableCell>{backend.certification.status}</TableCell>
                  <TableCell>{yesNo(backend.certification.compliant)}</TableCell>
                  <TableCell>{yesNo(backend.certification.supported)}</TableCell>
                  <TableCell>{backend.certification.validUntil ?? '—'}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      <div className="grid gap-4 lg:grid-cols-2">
        {status.backends.map((backend) => (
          <Card key={backend.id}>
            <CardHeader>
              <CardTitle className="text-base">{backend.displayName} evidence</CardTitle>
              <CardDescription>Observed {backend.evidence.observedAt}</CardDescription>
            </CardHeader>
            <CardContent className="space-y-4 text-sm">
              <div>
                <div className="font-medium">Implementation</div>
                <div className="text-muted-foreground">{backend.implementation.scope}</div>
                <a
                  className="break-all text-primary underline underline-offset-4"
                  href={backend.implementation.repository}
                  target="_blank"
                  rel="noreferrer"
                >
                  {backend.implementation.repository}@{backend.implementation.sourceCommit.slice(0, 12)}
                </a>
              </div>
              <div>
                <div className="font-medium">Evidence records</div>
                <ul className="list-disc space-y-2 pl-5 text-muted-foreground">
                  {backend.evidence.records.map((record) => (
                    <li key={`${record.kind}:${record.reference}`}>
                      <a
                        className="break-all text-primary underline underline-offset-4"
                        href={record.reference}
                        target="_blank"
                        rel="noreferrer"
                      >
                        {record.kind}
                      </a>
                      <div>{record.note}</div>
                    </li>
                  ))}
                </ul>
              </div>
              <div>
                <div className="font-medium">Certification blockers</div>
                <ul className="list-disc space-y-1 pl-5 text-muted-foreground">
                {backend.certification.blockers.length === 0 ? (
                  <li>None</li>
                ) : (
                  backend.certification.blockers.map((blocker) => <li key={blocker}>{blocker}</li>)
                )}
                </ul>
              </div>
            </CardContent>
          </Card>
        ))}
      </div>
    </div>
  )
}
