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
  | { view: 'detail'; id: string; autoPlay?: boolean; fromLib?: string }
  | { view: 'player'; item: Item; session: Session; resumeMs: number; fromLib?: string }

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

function pathToRoute(pathname: string, state?: unknown): Route {
  const rel = pathname.startsWith(BASE) ? pathname.slice(BASE.length) : pathname
  const parts = rel.split('/').filter(Boolean)
  if (parts[0] === 'admin') return { view: 'admin' }
  if (parts[0] === 'lib' && parts[1]) return { view: 'library', id: parts[1] }
  if (parts[0] === 'item' && parts[1]) {
    // Which library "back" returns to is navigation context, not item
    // data (collections are many-to-many with libraries): it rides
    // history.state so back/forward restore it; deep links fall back
    // to the item's own library server-side.
    const fromLib =
      state && typeof state === 'object' && 'fromLib' in state
        ? ((state as { fromLib?: string }).fromLib ?? undefined)
        : undefined
    return { view: 'detail', id: parts[1], autoPlay: parts[2] === 'play', fromLib }
  }
  return { view: 'libraries' }
}

export default function App() {
  const [phase, setPhase] = useState<Phase>('boot')
  const [route, setRoute] = useState<Route>(() =>
    pathToRoute(window.location.pathname, window.history.state)
  )

  // Forward navigation: push a history entry and switch views.
  // `replace` swaps the current entry instead — closing the player uses
  // it so the /play URL doesn't linger in history and re-trigger
  // autoplay on browser-back.
  const navigate = (r: Route, opts?: { replace?: boolean }) => {
    const path = routeToPath(r)
    const state = 'fromLib' in r && r.fromLib ? { fromLib: r.fromLib } : null
    if (path !== window.location.pathname) {
      if (opts?.replace) window.history.replaceState(state, '', path)
      else window.history.pushState(state, '', path)
    } else {
      window.history.replaceState(state, '', path)
    }
    setRoute(r)
  }

  // Back/forward: rebuild the view from the URL. Session state cannot
  // ride history (it holds live server objects), so landing back on a
  // /play URL becomes detail-with-autoplay.
  useEffect(() => {
    const onPop = (e: PopStateEvent) =>
      setRoute(pathToRoute(window.location.pathname, e.state))
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
          onOpen={(id) => navigate({ view: 'detail', id, fromLib: route.id })}
        />
      )}
      {route.view === 'detail' && (
        <Detail
          id={route.id}
          autoPlay={route.autoPlay}
          fromLib={route.fromLib}
          onBack={() => navigate({ view: 'libraries' })}
          onPlay={(item, session, resumeMs) =>
            navigate({ view: 'player', item, session, resumeMs, fromLib: route.fromLib })
          }
          onOpenEpisode={(id) => navigate({ view: 'detail', id, fromLib: route.fromLib })}
          onOpenLibrary={(id) => navigate({ view: 'library', id })}
        />
      )}
      {route.view === 'player' && (
        <Player
          key={route.session.session_id}
          item={route.item}
          session={route.session}
          resumeMs={route.resumeMs}
          onClose={() =>
            navigate(
              { view: 'detail', id: route.item.id, fromLib: route.fromLib },
              { replace: true }
            )
          }
        />
      )}
    </div>
  )
}
