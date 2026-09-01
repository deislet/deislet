import { useState, type FormEvent } from 'react'
import { api, ApiError } from '@/lib/api'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'

/**
 * The screen an operator gets when the control plane wants a credential.
 *
 * It exists because there was nowhere to put a key. With
 * `api_auth_enabled = true` - the posture the deployment guide tells an
 * operator to run - every call the console makes answered 401, and the only
 * thing the console did with a 401 was reload itself, so the console could not
 * be used at all in the only configuration it should be used in.
 *
 * The key is spent on a real call before the console commits to it. A key that
 * is merely stored is a key whose first refusal arrives on a dashboard page as
 * "failed to fetch metrics", which is where somebody starts debugging the
 * control plane instead of re-reading what they pasted.
 */
export function SignIn({ onSignedIn }: { onSignedIn: () => void }) {
  const [key, setKey] = useState('')
  const [checking, setChecking] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const submit = async (event: FormEvent) => {
    event.preventDefault()
    const candidate = key.trim()
    if (!candidate) {
      setError('Paste the key first - an empty one is what this control plane already refused.')
      return
    }

    setChecking(true)
    setError(null)
    // Stored first because `api.request` reads the key out of storage; put
    // back if the control plane refuses it, so a bad paste does not leave a
    // key behind that every later poll would fail on.
    api.setToken(candidate)
    try {
      await api.getMetricsSummary()
      onSignedIn()
    } catch (attempt) {
      const status = attempt instanceof ApiError ? attempt.status : 0
      if (status === 401) {
        api.removeToken()
        setError(
          'The control plane refused that key. Mint one with `deis token create`, or use a value from api_keys in its configuration file.'
        )
      } else if (status === 0) {
        api.removeToken()
        setError(
          `The control plane could not be reached, so this key has not been tried: ${
            attempt instanceof Error ? attempt.message : 'network error'
          }`
        )
      } else {
        // Anything else - a 403 from a scoped token that may not read
        // metrics, a 500 - means the credential itself resolved. Keeping it
        // and letting each page report its own refusal is the honest answer;
        // sending the operator back here would say the key is wrong when it
        // is the scope that is narrow.
        onSignedIn()
      }
    } finally {
      setChecking(false)
    }
  }

  return (
    <div className="flex min-h-screen w-full items-center justify-center bg-background p-6">
      <Card className="w-full max-w-md">
        <CardHeader>
          <CardTitle>Control</CardTitle>
          <CardDescription>
            This control plane requires a credential. Everything on the console reads or changes
            the fleet, so nothing is shown until one is presented.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <form onSubmit={submit} className="flex flex-col gap-4">
            <div className="flex flex-col gap-2">
              <Label htmlFor="api-key">API key</Label>
              <Input
                id="api-key"
                type="password"
                autoFocus
                autoComplete="off"
                spellCheck={false}
                placeholder="deis token create"
                value={key}
                onChange={(event) => setKey(event.target.value)}
              />
            </div>

            {error && (
              <p className="text-sm text-destructive" role="alert">
                {error}
              </p>
            )}

            <Button type="submit" disabled={checking}>
              {checking ? 'Checking the key...' : 'Use this key'}
            </Button>

            <p className="text-xs text-muted-foreground">
              The key is kept in this browser only, and is sent to this control plane as
              <code className="mx-1">X-API-Key</code>. It never leaves the origin the console was
              loaded from.
            </p>
          </form>
        </CardContent>
      </Card>
    </div>
  )
}
