import { BrowserRouter, Routes, Route } from 'react-router-dom'
import { Toaster } from 'sonner'
import { Layout } from '@/components/layout/Layout'
import { Dashboard } from '@/pages/Dashboard'
import { Nodes } from '@/pages/Nodes'
import { Commands } from '@/pages/Commands'
import { Configs } from '@/pages/Configs'
import { Events } from '@/pages/Events'

function App() {
  return (
    <BrowserRouter>
      {/*
        Without this nothing rendered the toasts, so every outcome the pages
        reported - a restart accepted, a restart refused - was raised into a
        component that was not mounted and the operator saw nothing at all.
      */}
      <Toaster richColors position="top-right" />
      <Routes>
        <Route path="/" element={<Layout />}>
          <Route index element={<Dashboard />} />
          <Route path="nodes" element={<Nodes />} />
          <Route path="commands" element={<Commands />} />
          <Route path="configs" element={<Configs />} />
          <Route path="events" element={<Events />} />
        </Route>
      </Routes>
    </BrowserRouter>
  )
}

export default App
