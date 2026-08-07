import { useEffect, useState } from 'react'
import {
  fetchBootstrap,
  isAdmin,
  keepTokenFresh,
  storeTokens,
  username,
  type ItemDetail,
  type Session,
} from './api'
import Auth from './views/Auth'
import Libraries from './views/Libraries'
import Library from './views/Library'
import Detail from './views/Detail'
import Player from './views/Player'
import Admin from './views/Admin'
import Settings from './views/Settings'

type Route =
  | { view: 'libraries' }
  | { view: 'library'; id: string }
  | { view: 'admin' }
  | { view: 'settings' }
  | { view: 'detail'; id: string; autoPlay?: boolean; fromLib: string }
  | { view: 'player'; item: ItemDetail; session: Session; resumeMs: number; fromLib: string }

type Phase = 'boot' | 'setup' | 'login' | 'app'

const BASE = '/app'

// URL ↔ route. Items browsed from a library live under it —
// /app/library/{lib}/item/{id} — so the back-target survives reload
// and link sharing (collections are many-to-many with libraries; the
// URL is the navigation context). Nothing mints library-less item
// links, so there is no bare item form.
// The player is transient (sessions die with it): its /play URL
// re-enters the detail view with autoplay.
function routeToPath(route: Route): string {
  switch (route.view) {
    case 'libraries':
      return `${BASE}/`
    case 'library':
      return `${BASE}/library/${route.id}`
    case 'admin':
      return `${BASE}/admin`
    case 'settings':
      return `${BASE}/settings`
    case 'detail':
      return `${BASE}/library/${route.fromLib}/item/${route.id}`
    case 'player':
      return `${BASE}/library/${route.fromLib}/item/${route.item.id}/play`
  }
}

function pathToRoute(pathname: string): Route {
  const rel = pathname.startsWith(BASE) ? pathname.slice(BASE.length) : pathname
  const parts = rel.split('/').filter(Boolean)
  if (parts[0] === 'admin') return { view: 'admin' }
  if (parts[0] === 'settings') return { view: 'settings' }
  if (parts[0] === 'library' && parts[1]) {
    if (parts[2] === 'item' && parts[3]) {
      return {
        view: 'detail',
        id: parts[3],
        autoPlay: parts[4] === 'play',
        fromLib: parts[1],
      }
    }
    return { view: 'library', id: parts[1] }
  }
  return { view: 'libraries' }
}

export default function App() {
  const [phase, setPhase] = useState<Phase>('boot')
  const [route, setRoute] = useState<Route>(() => pathToRoute(window.location.pathname))
  // One search box, two meanings, decided by where you are: on the home
  // screen it searches every library; on a library it filters that one.
  // The text itself is shared, which is what lets a result lead into its
  // library with the query still standing.
  const [search, setSearch] = useState('')
  const [query, setQuery] = useState('')

  // Typing hits the database — on the home screen once per library — so
  // wait for a pause. Debounced HERE rather than in each view: two views
  // debouncing the same text would each have their own idea of when it
  // settled, and the query would change twice on every keystroke.
  useEffect(() => {
    const t = setTimeout(() => setQuery(search), 250)
    return () => clearTimeout(t)
  }, [search])

  // Forward navigation: push a history entry and switch views.
  // `replace` swaps the current entry instead — closing the player uses
  // it so the /play URL doesn't linger in history and re-trigger
  // autoplay on browser-back.
  const navigate = (r: Route, opts?: { replace?: boolean }) => {
    const path = routeToPath(r)
    if (path !== window.location.pathname) {
      if (opts?.replace) window.history.replaceState(null, '', path)
      else window.history.pushState(null, '', path)
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

  // One public endpoint states which screen to open on. This used to be
  // read off the STATUS of /api/v1/items — 503 meant setup, 401 meant
  // login — which inferred the client's own state from an error path and
  // pulled the whole catalogue (1.4 MB) for a body it discarded.
  useEffect(() => {
    ;(async () => {
      const s = await fetchBootstrap().catch(() => null)
      if (!s) setPhase('login')
      else if (s.setup_required) setPhase('setup')
      else setPhase(s.authenticated ? 'app' : 'login')
    })()
  }, [])

  // Keep the media cookie fresh: <video> and hls.js requests authenticate
  // with it, and access tokens expire after 15 minutes.
  //
  // Scheduled from the token's OWN expiry, not on a fixed interval. A
  // 10-minute interval looks like enough margin against a 15-minute
  // token until it restarts: every mount began the count again without
  // refreshing, so a reload landing partway through left a gap longer
  // than the token lived. Measured 2026-08-07 — token issued 14:38:07,
  // dead 14:53:07, refreshed 14:56:48. In those three minutes hls.js
  // got 401s, stopped loading, and the session it was reading was
  // reaped for idleness; the 401 also masked the 410 that would have
  // told the player to recover.
  useEffect(() => {
    if (phase !== 'app') return
    keepTokenFresh()
  }, [phase])

  if (phase === 'boot') return null
  if (phase === 'setup' || phase === 'login')
    return <Auth mode={phase} onDone={() => setPhase('app')} />

  return (
    <div className="shell">
      <header className="topbar">
        <button
          className="wordmark"
          onClick={() => {
            // Going home is a fresh start: a standing filter that
            // silently follows you there reads as missing items.
            setSearch('')
            navigate({ view: 'libraries' })
          }}
        >
          kahawai<span className="tilde">~</span>
        </button>
        {/* Only where it means something. On the player, admin and
            settings there is nothing for it to search, and a box that
            silently does nothing is worse than no box. */}
        {(route.view === 'libraries' || route.view === 'library') && (
          <input
            className="topbar-search"
            placeholder={
              route.view === 'library' ? 'Filter this library' : 'Search all libraries'
            }
            value={search}
            onChange={(e) => setSearch(e.target.value)}
          />
        )}
        <div className="topbar-right">
          {isAdmin() && (
            <button className="btn ghost small" onClick={() => navigate({ view: 'admin' })}>
              Admin
            </button>
          )}
          <button className="btn ghost small" onClick={() => navigate({ view: 'settings' })}>
            Settings
          </button>
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
      {route.view === 'settings' && <Settings />}
      {route.view === 'libraries' && (
        <Libraries
          query={query}
          onOpen={(id) => navigate({ view: 'library', id })}
          onOpenItem={(id, fromLib) => navigate({ view: 'detail', id, fromLib })}
        />
      )}
      {route.view === 'library' && (
        <Library
          libraryId={route.id}
          query={query}
          onOpen={(id) => navigate({ view: 'detail', id, fromLib: route.id })}
          onResetSearch={() => setSearch('')}
        />
      )}
      {route.view === 'detail' && (
        <Detail
          id={route.id}
          autoPlay={route.autoPlay}
          fromLib={route.fromLib}
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
          libraryId={route.fromLib}
          onClose={() =>
            navigate(
              { view: 'detail', id: route.item.id, fromLib: route.fromLib },
              { replace: true }
            )
          }
          // Capability-debug restart: a new session id remounts the
          // player (keyed above), which ends the old one in cleanup.
          onRestart={(session, at) =>
            navigate(
              { view: 'player', item: route.item, session, resumeMs: at, fromLib: route.fromLib },
              { replace: true }
            )
          }
        />
      )}
    </div>
  )
}
