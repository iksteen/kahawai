import { useEffect, useState } from 'react'
import { api, isAdmin, refreshTokens, storeTokens, username, type Item, type Session } from './api'
import Auth from './views/Auth'
import Library from './views/Library'
import Detail from './views/Detail'
import Player from './views/Player'
import Admin from './views/Admin'

type Route =
  | { view: 'library' }
  | { view: 'admin' }
  | { view: 'detail'; id: string }
  | { view: 'player'; item: Item; session: Session; resumeMs: number }

type Phase = 'boot' | 'setup' | 'login' | 'app'

export default function App() {
  const [phase, setPhase] = useState<Phase>('boot')
  const [route, setRoute] = useState<Route>({ view: 'library' })

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
        <button className="wordmark" onClick={() => setRoute({ view: 'library' })}>
          kahawai<span className="tilde">~</span>
        </button>
        <div className="topbar-right">
          {isAdmin() && (
            <button className="btn ghost small" onClick={() => setRoute({ view: 'admin' })}>
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
      {route.view === 'library' && (
        <Library onOpen={(id) => setRoute({ view: 'detail', id })} />
      )}
      {route.view === 'detail' && (
        <Detail
          id={route.id}
          onBack={() => setRoute({ view: 'library' })}
          onPlay={(item, session, resumeMs) =>
            setRoute({ view: 'player', item, session, resumeMs })
          }
        />
      )}
      {route.view === 'player' && (
        <Player
          item={route.item}
          session={route.session}
          resumeMs={route.resumeMs}
          onClose={() => setRoute({ view: 'detail', id: route.item.id })}
        />
      )}
    </div>
  )
}
