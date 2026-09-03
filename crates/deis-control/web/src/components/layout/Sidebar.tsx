import { useState } from 'react'
import { Link, useLocation } from 'react-router-dom'
import { cn } from '@/lib/utils'
import {
  LayoutDashboard,
  Server,
  Terminal,
  Settings,
  ScrollText,
  ChevronLeft,
  ChevronRight,
  LogOut,
  Globe2,
} from 'lucide-react'
import { Button } from '@/components/ui/button'
import { ScrollArea } from '@/components/ui/scroll-area'
import { api } from '@/lib/api'

interface NavItem {
  title: string
  href: string
  icon: React.ComponentType<{ className?: string }>
}

const navItems: NavItem[] = [
  { title: 'Dashboard', href: '/', icon: LayoutDashboard },
  { title: 'Nodes', href: '/nodes', icon: Server },
  { title: 'Commands', href: '/commands', icon: Terminal },
  { title: 'Configs', href: '/configs', icon: Settings },
  { title: 'Events', href: '/events', icon: ScrollText },
  { title: 'Targets', href: '/targets', icon: Globe2 },
]

export function Sidebar() {
  const [collapsed, setCollapsed] = useState(false)
  const location = useLocation()

  return (
    <div
      className={cn(
        'relative flex flex-col border-r bg-sidebar-background transition-all duration-300',
        collapsed ? 'w-16' : 'w-60'
      )}
    >
      <div className="flex h-14 items-center border-b px-4">
        {!collapsed && (
          <span className="font-semibold text-sidebar-foreground">Control</span>
        )}
        <Button
          variant="ghost"
          size="icon"
          className={cn('ml-auto', collapsed && 'mx-auto')}
          onClick={() => setCollapsed(!collapsed)}
        >
          {collapsed ? (
            <ChevronRight className="h-4 w-4" />
          ) : (
            <ChevronLeft className="h-4 w-4" />
          )}
        </Button>
      </div>

      <ScrollArea className="flex-1 py-2">
        <nav className="flex flex-col gap-1 px-2">
          {navItems.map((item) => {
            const isActive = location.pathname === item.href
            return (
              <Link
                key={item.href}
                to={item.href}
                className={cn(
                  'flex items-center gap-3 rounded-md px-3 py-2 text-sm transition-colors',
                  isActive
                    ? 'bg-sidebar-accent text-sidebar-accent-foreground'
                    : 'text-sidebar-foreground hover:bg-sidebar-accent hover:text-sidebar-accent-foreground',
                  collapsed && 'justify-center px-2'
                )}
              >
                <item.icon className="h-4 w-4 shrink-0" />
                {!collapsed && <span>{item.title}</span>}
              </Link>
            )
          })}
        </nav>
      </ScrollArea>

      <div className="flex flex-col gap-2 border-t p-4">
        {/*
          Only when a key is actually held. A control plane with
          `api_auth_enabled` off admits a direct caller on its own machine with
          no credential at all, and there is nothing to sign out of there - a
          button that cleared nothing and then demanded a key that does not
          exist would be a way to lock yourself out of an open console.
        */}
        {api.getToken() !== null && (
          <Button
            variant="ghost"
            size={collapsed ? 'icon' : 'sm'}
            className={cn('text-muted-foreground', collapsed ? 'mx-auto' : 'justify-start')}
            title="Forget this API key"
            onClick={() => api.signOut()}
          >
            <LogOut className="h-4 w-4 shrink-0" />
            {!collapsed && <span className="ml-2">Forget this key</span>}
          </Button>
        )}
        {!collapsed && (
          <p className="text-xs text-muted-foreground">Control v0.1.0</p>
        )}
      </div>
    </div>
  )
}
