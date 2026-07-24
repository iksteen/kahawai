import { useEffect, useState } from 'react'
import { api, isAdmin, refreshTokens, storeTokens, username, type Item, type Session } from './api'
import Auth from './views/Auth'
import Libraries from './views/Libraries'
import Library from './views/Library'
import Detail from './views/Detail'
import Player from './views/Player'
import Admin from './views/Admin'

type Route =
  | { view: 'libraries' }
  | { view: 'library'; id: string }
  | { view: 'admin' }
  | { view: 'detail'; id: string; autoPlay?: boolean }
  | { view: 'player'; item: Item; session: Session; resumeMs: number }

type Phase = 'boot' | 'setup' | 'login' | 'app'

const BASE = '/app'

// URL ↔ route. The player itself is transient (sessions die with it),
// so it lives at the item's /play URL: deep-loading or forward-ing onto
// it re-enters the detail view with autoplay instead.
function routeToPath(route: Route): string {
  switch (route.view) {
    case 'libraries':
      return `${BASE}/`
    case 'library':
      return `${BASE}/lib/${route.id}`
    case 'admin':
      return `${BASE}/admin`
    case 'detail':
      return `${BASE}/item/${route.id}`
    case 'player':
      return `${BASE}/item/${route.item.id}/play`
  }
}

function pathToRoute(pathname: string): Route {
  const rel = pathname.startsWith(BASE) ? pathname.slice(BASE.length) : pathname
  const parts = rel.split('/').filter(Boolean)
  if (parts[0] === 'admin') return { view: 'admin' }
  if (parts[0] === 'lib' && parts[1]) return { view: 'library', id: parts[1] }
  if (parts[0] === 'item' && parts[1]) {
    return { view: 'detail', id: parts[1], autoPlay: parts[2] === 'play' }
  }
  return { view: 'libraries' }
}

export default function App() {
  const [phase, setPhase] = useState<Phase>('boot')
  const [route, setRoute] = useState<Route>(() => pathToRoute(window.location.pathname))

  // Forward navigation: push a history entry and switch views.
  const navigate = (r: Route) => {
    const path = routeToPath(r)
    if (path !== window.location.pathname) {
      window.history.pushState(null, '', path)
    }
    setRoute(r)
  }

  // Back/forward: rebuild the view from the URL. Session state cannot
  // ride history (it holds live server objects), so landing back on a
  // /play URL becomes detail-with-autoplay.
  useEffect(() => {
    const onPop = () => setRoute(pathToRoute(window.location.pathname))
    window.addEventListener('popstate', onPop)
    return () => window.removeEventListener('popstate', onPop)
  }, [])

  useEffect(() => {
    ;(async () => {
      const r = await api('/api/v1/items')
      if (r.status === 503) setPhase('setup')
      else if (r.status === 401) setPhase('login')
      else setPhase('app')
    })()
  }, [])

  // Keep the media cookie fresh: <video> and hls.js requests authenticate
  // with it, and access tokens expire after 15 minutes.
  useEffect(() => {
    if (phase !== 'app') return
    const t = setInterval(refreshTokens, 10 * 60 * 1000)
    return () => clearInterval(t)
  }, [phase])

  if (phase === 'boot') return null
  if (phase === 'setup' || phase === 'login')
    return <Auth mode={phase} onDone={() => setPhase('app')} />

  return (
    <div className="shell">
      <header className="topbar">
        <button className="wordmark" onClick={() => navigate({ view: 'libraries' })}>
          kahawai<span className="tilde">~</span>
        </button>
        <div className="topbar-right">
          {isAdmin() && (
            <button className="btn ghost small" onClick={() => navigate({ view: 'admin' })}>
              Admin
            </button>
          )}
          <span className="whoami">{username()}</span>
          <button
            className="btn ghost small"
            onClick={() => {
              storeTokens(null)
              setPhase('login')
            }}
          >
            Sign out
          </button>
        </div>
      </header>
      {route.view === 'admin' && <Admin />}
      {route.view === 'libraries' && (
        <Libraries onOpen={(id) => navigate({ view: 'library', id })} />
      )}
      {route.view === 'library' && (
        <Library
          libraryId={route.id}
          onOpen={(id) => navigate({ view: 'detail', id })}
        />
      )}
      {route.view === 'detail' && (
        <Detail
          id={route.id}
          autoPlay={route.autoPlay}
          onBack={() => window.history.back()}
          onPlay={(item, session, resumeMs) =>
            navigate({ view: 'player', item, session, resumeMs })
          }
          onOpenEpisode={(id) => navigate({ view: 'detail', id })}
        />
      )}
      {route.view === 'player' && (
        <Player
          item={route.item}
          session={route.session}
          resumeMs={route.resumeMs}
          onClose={() => window.history.back()}
        />
      )}
    </div>
  )
}
