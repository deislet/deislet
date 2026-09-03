import { useCallback, useEffect, useState } from 'react'
import { BrowserRouter, Routes, Route } from 'react-router-dom'
import { Toaster } from 'sonner'
import { Layout } from '@/components/layout/Layout'
import { Dashboard } from '@/pages/Dashboard'
import { Nodes } from '@/pages/Nodes'
import { Commands } from '@/pages/Commands'
import { Configs } from '@/pages/Configs'
import { Events } from '@/pages/Events'
import { Targets } from '@/pages/Targets'
import { SignIn } from '@/pages/SignIn'
import { api, ApiError } from '@/lib/api'

/**
 * Whether this browser may read the control plane yet.
 *
 * `checking` is one probe request, `open` is every page mounted and polling,
 * `needs-key` is the screen that takes a credential.
 */
type Access = 'checking' | 'open' | 'needs-key'

function App() {
  const [access, setAccess] = useState<Access>('checking')

  // One question before anything mounts, rather than a guess.
  //
  // `api_auth_enabled` is a control-plane setting the console cannot read, and
  // the two postures want different first screens. With it off, a caller on
  // the machine the control plane runs on is admitted with no credential at
  // all (`crates/deis-control/src/api/authz.rs`), and there is no token to ask
  // anyone for - a key screen there would be a locked door with no key cut for
  // it. With it on, nothing works until a key is presented. A 401 to this one
  // call is the only answer that distinguishes them, so the console asks.
  //
  // Any other outcome - a control plane that is down, a 403 from a scoped
  // token - opens the console: those are conditions the pages themselves
  // report, and turning them into a key prompt would tell an operator their
  // credential is wrong when it is not.
  useEffect(() => {
    let cancelled = false
    api
      .getMetricsSummary()
      .then(() => {
        if (!cancelled) setAccess('open')
      })
      .catch((error: unknown) => {
        if (cancelled) return
        const refused = error instanceof ApiError && error.status === 401
        setAccess(refused ? 'needs-key' : 'open')
      })
    return () => {
      cancelled = true
    }
  }, [])

  // A key can stop working while the console is open - revoked, or removed
  // from the configuration file and the control plane restarted. `api.request`
  // reports that as an event rather than by navigating, and this is what
  // listens: the pages unmount, which also stops their five polling intervals,
  // and the key screen takes their place.
  useEffect(() => api.onUnauthorized(() => setAccess('needs-key')), [])

  const signedIn = useCallback(() => setAccess('open'), [])

  return (
    <>
      {/*
        Outside the branches below, so an outcome a page reports - a restart
        accepted, a restart refused - is rendered wherever the console is.
        Without a mounted Toaster every one of those was raised into a
        component that was not there and the operator saw nothing at all.
      */}
      <Toaster richColors position="top-right" />
      {access === 'checking' ? (
        <div className="flex min-h-screen w-full items-center justify-center bg-background">
          <p className="text-sm text-muted-foreground">Asking the control plane who you are...</p>
        </div>
      ) : access === 'needs-key' ? (
        <SignIn onSignedIn={signedIn} />
      ) : (
        <BrowserRouter>
          <Routes>
            <Route path="/" element={<Layout />}>
              <Route index element={<Dashboard />} />
              <Route path="nodes" element={<Nodes />} />
              <Route path="commands" element={<Commands />} />
              <Route path="configs" element={<Configs />} />
              <Route path="events" element={<Events />} />
              <Route path="targets" element={<Targets />} />
            </Route>
          </Routes>
        </BrowserRouter>
      )}
    </>
  )
}

export default App
