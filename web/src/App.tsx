import { useEffect, useRef, useState } from 'react'
import {
  endSession,
  fetchBootstrap,
  fetchLibraries,
  isAdmin,
  keepTokenFresh,
  onTokensCleared,
  signOut,
  username,
  type Item,
  type LibrarySummary,
} from './api'
import Failed from './Failed'
import Icon, { type IconName } from './icons'
import type { GainMode, QueueEntry } from './replaygain'
import { SEARCH_LIST_ID, searchOptionId } from './search-nav'
import { NOTICE_MS, onNotice } from './toast'
import Auth from './views/Auth'
import Libraries from './views/Libraries'
import SearchOverlay from './views/SearchOverlay'
import Library from './views/Library'
import AlbumPlayer from './views/AlbumPlayer'
import Detail from './views/Detail'
import Season from './views/Season'
import PlayerRoute from './views/PlayerRoute'
import Boundary from './Boundary'
import Admin from './views/Admin'
import Settings from './views/Settings'

type Route =
  | { view: 'libraries' }
  | { view: 'library'; id: string }
  | { view: 'admin' }
  | { view: 'settings' }
  | { view: 'detail'; id: string; fromLib: string }
  | { view: 'season'; showId: string; season: number | null; fromLib: string }
  | { view: 'player'; id: string; fromLib: string; fromStart?: boolean }

type Phase = 'boot' | 'setup' | 'login' | 'app'

const BASE = '/app'

// URL ↔ route. Items browsed from a library live under it —
// /app/library/{lib}/item/{id} — so the back-target survives reload
// and link sharing (collections are many-to-many with libraries; the
// URL is the navigation context). Nothing mints library-less item
// links, so there is no bare item form.
// `/play` is the player's own address: it re-enters the player, which starts
// its own session.
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
    case 'season':
      // `all` rather than an empty segment: a null season is ABSOLUTE
      // numbering, a real answer, and it needs a spelling of its own.
      return `${BASE}/library/${route.fromLib}/item/${route.showId}/season/${route.season ?? 'all'}`
    case 'player':
      return `${BASE}/library/${route.fromLib}/item/${route.id}/play`
  }
}

/// Which SCREEN you are on, which is not the same as which address.
///
/// They agree everywhere but the player, whose address carries the episode.
/// Keying the boundary on the address meant the autoplay handover — which
/// changes the URL to the next episode and nothing else — remounted the whole
/// player route: the session `Player` had already started was dropped on the
/// floor unreported, so nobody ever ended it and nobody pinged it, and a
/// third one was started for the same episode. Every episode boundary cost a
/// leaked transcoder slot and a rebuilt frame with the starting veil back.
///
/// `mode` survives this too, but that is not the same as staying fullscreen:
/// the element the browser holds is the `.videobox` inside `Player`, which is
/// keyed on the session and is replaced regardless. Fullscreen across a
/// handover is a separate matter and is not fixed here.
///
/// The cost is that a throw inside the player stays latched across a handover
/// to the next episode — which cannot happen, because a player that threw is
/// not playing anything to the end. Leaving the player clears it as before.
function boundaryKey(route: Route): string {
  return route.view === 'player' ? `${BASE}/library/${route.fromLib}/play` : routeToPath(route)
}

function pathToRoute(pathname: string): Route {
  const rel = pathname.startsWith(BASE) ? pathname.slice(BASE.length) : pathname
  const parts = rel.split('/').filter(Boolean)
  if (parts[0] === 'admin') return { view: 'admin' }
  if (parts[0] === 'settings') return { view: 'settings' }
  if (parts[0] === 'library' && parts[1]) {
    if (parts[2] === 'item' && parts[3]) {
      if (parts[4] === 'season' && parts[5]) {
        return {
          view: 'season',
          showId: parts[3],
          season: parts[5] === 'all' ? null : Number(parts[5]),
          fromLib: parts[1],
        }
      }
      // `/play` is the player's own address now, so a deep link, a reload and
      // a forward all land in the same place as pressing Play.
      if (parts[4] === 'play') return { view: 'player', id: parts[3], fromLib: parts[1] }
      return { view: 'detail', id: parts[3], fromLib: parts[1] }
    }
    return { view: 'library', id: parts[1] }
  }
  return { view: 'libraries' }
}

/// A popover and the sheet that closes it. The sheet is a sibling rather
/// than a document listener: a click outside lands on it and nowhere else,
/// so nothing behind the menu acts on the click that dismissed it.
function Menu({
  open,
  onClose,
  align,
  children,
}: {
  open: boolean
  onClose: () => void
  align: 'left' | 'right'
  children: React.ReactNode
}) {
  if (!open) return null
  return (
    <>
      <div className="menu-sheet" onClick={onClose} />
      <div className={`menu menu-${align}`} role="menu">
        {children}
      </div>
    </>
  )
}

/// Where you are, in a menu row: filled, lit, and with a lit glyph.
function menuRowClass(here: boolean) {
  return here ? 'menu-item here' : 'menu-item'
}

/// Which glyph a library wears in the jump menu. Series and anime are
/// both shows on screen; only the providers behind them differ.
function libGlyph(mediaType: string): IconName {
  if (mediaType === 'music') return 'album'
  if (mediaType === 'movies') return 'movie'
  return 'show'
}

export default function App() {
  const [phase, setPhase] = useState<Phase>('boot')
  /// The bootstrap request itself failed. Distinct from every other error in
  /// the app because it happens before there IS an app: no header, no route,
  /// nothing to put a toast on.
  const [bootError, setBootError] = useState('')
  const [bootAttempt, setBootAttempt] = useState(0)
  /// Readable from the token-cleared handler, which is registered once and
  /// would otherwise only ever see 'boot'.
  const phaseRef = useRef(phase)
  phaseRef.current = phase
  /// Why the sign-in screen is showing, when it was not asked for.
  const [endedNote, setEndedNote] = useState('')
  /// Sign-out in two steps, because the order was wrong in a way that cost a
  /// transcoder slot every time.
  ///
  /// Clearing the tokens first meant the player unmounted afterwards, so its
  /// final progress report went out unauthenticated, 401'd, found no refresh
  /// token and never landed. Setting the phase first unmounts the player while
  /// the token still works; this effect then runs after that commit and does
  /// the rest.
  ///
  /// The session's release is on the same clock and for the same reason, but it
  /// is no longer the player that sends it — see the effect below, and the
  /// explicit release inside this one. Ordering is the whole subject here: two
  /// requests that only work while the credentials still exist, sent from a
  /// path whose job is to destroy them.
  const [signingOut, setSigningOut] = useState(false)
  const [route, setRoute] = useState<Route>(() => pathToRoute(window.location.pathname))

  /// The playback session outlives any single mount of the player.
  ///
  /// It used to be released by the player's own effect cleanup, which made
  /// "this component went away" mean "the viewer is finished with this
  /// session" — two different things. React tears a component down and builds
  /// it again whenever it likes, and does exactly that on every mount under
  /// StrictMode: the rebuilt player then inherited a session the hub had
  /// already disposed of, and answered 404 on the playlist, on every segment
  /// and on the progress ping. Playback simply did not work in development.
  ///
  /// The route owns it instead, because the route is what changes when the
  /// viewer actually leaves. Releasing the PREVIOUS id covers all three ways a
  /// session stops being the current one — leaving the player, a restart that
  /// replaces it, and rolling into the next episode — and a remount is not one
  /// of them, because the id did not change.
  /// Reported by the player route as it starts one, replaces one, or goes
  /// away. Held here because releasing is the shell's job — see below.
  const [playingId, setPlayingId] = useState<string | null>(null)
  const playing = route.view === 'player' ? playingId : null
  const playedRef = useRef<string | null>(null)
  useEffect(() => {
    const previous = playedRef.current
    playedRef.current = playing
    // The player reports its final position from its own cleanup, which runs
    // before this: it is the half that knows where the viewer got to.
    if (previous && previous !== playing) void endSession(previous, true)
  }, [playing])

  /// Closing the tab is not a route change, so it needs saying separately.
  /// `keepalive` is what lets the request outlive the page.
  useEffect(() => {
    if (!playing) return
    const release = () => void endSession(playing, true)
    window.addEventListener('beforeunload', release)
    return () => window.removeEventListener('beforeunload', release)
  }, [playing])
  // One search box, two meanings, decided by where you are: on the home
  // screen it searches every library; on a library it filters that one.
  // The text itself is shared, which is what lets a result lead into its
  // library with the query still standing.
  const [search, setSearch] = useState('')
  const [query, setQuery] = useState('')
  /// Whether the results panel is showing. Not derived from `search`, because
  /// dismissing it has to be possible without clearing the box: a click
  /// elsewhere puts it away and leaves the text where it is, and focusing the
  /// box brings it back.
  const [overlayOpen, setOverlayOpen] = useState(false)
  /// What the panel currently offers the keyboard, mirrored up here for the
  /// benefit of the input's ARIA alone.
  ///
  /// The panel owns the highlight, since it owns the rows — but a combobox
  /// announces its state on the INPUT (`aria-expanded`, and
  /// `aria-activedescendant` naming the lit row), and that element lives here.
  /// So the panel reports both and this holds them. `setCombo` is passed down
  /// as-is because a `useState` setter is stable: an inline lambda would change
  /// identity every render and the effect reporting into it would never settle.
  const [combo, setCombo] = useState({ shown: false, highlight: -1 })
  /// The box itself, so the panel can find the keys and the search area around
  /// it. Walking the highlight never moves focus off this element — that is
  /// what `aria-activedescendant` is for, and taking focus onto a row would
  /// take the caret out of the field you are still typing in.
  const searchBox = useRef<HTMLInputElement>(null)
  // Header chrome. Both menus and the notice live here because the header
  // does; no view has an opinion about any of them.
  const [navOpen, setNavOpen] = useState(false)
  const [profileOpen, setProfileOpen] = useState(false)
  const [libs, setLibs] = useState<LibrarySummary[]>([])
  /// The music queue lives here, not in the album page that started it:
  /// it has to survive navigating away, which is the whole point of a
  /// queue.
  const [queue, setQueue] = useState<{ entries: QueueEntry[]; at: number } | null>(null)
  /// Which queue this is, for the boundary below. A new record is a new
  /// generation; appending to the one playing is not.
  const [queueGen, setQueueGen] = useState(0)

  /// Put something on now, replacing whatever was queued.
  const playNow = (tracks: Item[], at: number, gain: GainMode) => {
    setQueueGen((n) => n + 1)
    setQueue({ entries: tracks.map((track) => ({ track, gain })), at })
  }

  /// Add to the end without disturbing what is playing. With nothing
  /// playing there is nothing to disturb, so it starts.
  const enqueue = (tracks: Item[], gain: GainMode) =>
    setQueue((q) => {
      const added = tracks.map((track) => ({ track, gain }))
      return q ? { ...q, entries: [...q.entries, ...added] } : { entries: added, at: 0 }
    })
  const [notice, setNotice] = useState('')
  const noticeTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)

  // Typing hits the database — on the home screen once per library — so
  // wait for a pause. Debounced HERE rather than in each view: two views
  // debouncing the same text would each have their own idea of when it
  // settled, and the query would change twice on every keystroke.
  useEffect(() => {
    const t = setTimeout(() => setQuery(search), 250)
    return () => clearTimeout(t)
  }, [search])

  // Forward navigation: push a history entry and switch views.
  // `replace` swaps the current entry instead — closing the player uses it so
  // the /play URL does not linger in history.
  /// Returns true when it actually PUSHED a history entry, which the player's
  /// close needs to know: only a pushed entry may be popped.
  const navigate = (r: Route, opts?: { replace?: boolean }) => {
    const path = routeToPath(r)
    let pushed = false
    if (path !== window.location.pathname) {
      // `ours` marks an entry THIS app pushed, and it rides the history entry
      // rather than a ref so it survives a reload and a forward navigation. A
      // ref did not: reloading while watching brought it back false with the
      // entry still there, so Close replaced instead of popping and left two
      // identical entries — the first press of browser-back doing nothing,
      // which is the exact bug the popping exists to avoid.
      //
      // A replace keeps whatever the entry already claimed: replacing a URL
      // somebody typed does not make it ours to pop.
      if (opts?.replace) window.history.replaceState(window.history.state, '', path)
      else {
        window.history.pushState({ ours: true }, '', path)
        pushed = true
      }
    }
    setRoute(r)
    // A push is a new screen, and the browser does not move for one. The
    // library grid reserves the whole library's height — tens of thousands of
    // pixels — so opening an item from row 150 kept `scrollY` and clamped it to
    // the short page's maximum: you landed at the BOTTOM of the item, on the
    // sources list, with the title and Play button off-screen above. Back and
    // forward are left to the browser, which restores their positions itself.
    if (pushed) window.scrollTo({ top: 0 })
    return pushed
  }

  // Back/forward: rebuild the view from the URL. A /play URL rebuilds the
  // player, which starts a session of its own — no state rides history.
  useEffect(() => {
    const onPop = () => {
      setRoute(pathToRoute(window.location.pathname))
    }
    window.addEventListener('popstate', onPop)
    return () => window.removeEventListener('popstate', onPop)
  }, [])

  // One public endpoint states which screen to open on. This used to be
  // read off the STATUS of /api/v1/items — 503 meant setup, 401 meant
  // login — which inferred the client's own state from an error path and
  // pulled the whole catalogue (1.4 MB) for a body it discarded.
  useEffect(() => {
    ;(async () => {
      try {
        const s = await fetchBootstrap()
        setBootError('')
        if (s.setup_required) setPhase('setup')
        else setPhase(s.authenticated ? 'app' : 'login')
      } catch (e) {
        // NOT the sign-in screen. Conflating "the hub did not answer" with
        // "you are not signed in" sent a signed-in viewer to a password box
        // over one blip, with their tokens still perfectly good — and there is
        // nothing to sign in TO while the hub is unreachable, so the one thing
        // that screen offers cannot work either.
        // `String(e)` alone: `Offline` already reads "Could not reach the
        // hub.", and hardcoding that as the HEADING printed it twice for an
        // unreachable hub and lied for every other failure — a 502 from a proxy,
        // a 500, or an HTML body that makes the JSON parse throw all mean the
        // hub answered.
        setBootError(String(e))
      }
    })()
  }, [bootAttempt])

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
  // reaped for idleness; the 401 also masked the 404 that would have
  // told the player to recover.
  useEffect(() => {
    if (phase !== 'app') return
    keepTokenFresh()
  }, [phase])

  // The jump menu lists the libraries, so the shell needs them. Failing
  // to get them costs the menu its entries and nothing else, so it stays
  // quiet: a toast about the header would be the first thing a working
  // session ever said.
  useEffect(() => {
    if (phase !== 'app') return
    let live = true
    fetchLibraries()
      .then((r) => live && setLibs(r.libraries))
      .catch(() => {})
    return () => {
      live = false
    }
  }, [phase])

  // A session that ends while you are using the app: go to sign-in and say
  // so. Registered once, for the life of the app, because the tokens can be
  // cleared from anywhere — a background refresh, a request retry, the
  // profile menu.
  useEffect(() => {
    onTokensCleared((deliberate) => {
      // The queue outlives the shell, so it goes whichever way the session
      // ended. An expiry is not a change of person, but nothing should still
      // be playing to a sign-in screen — and the next account in this tab
      // would inherit a queue whose tracks it cannot see, which the album
      // player retries for ever because a track it may not read looks exactly
      // like a mediahost that is down.
      setQueue(null)
      if (phaseRef.current !== 'app') return
      // Signing out is not something that happened to you, so it gets no
      // explanation. Only a session that ended by itself needs one.
      if (!deliberate) setEndedNote('Your session ended. Sign in to carry on where you were.')
      setPhase('login')
    })
    return () => onTokensCleared(null)
  }, [])

  useEffect(() => {
    if (!signingOut) return
    setSigningOut(false)
    // The route is deliberately KEPT when a session expires — signing back in
    // as yourself returns you to the page you were reading — but a deliberate
    // sign-out is a different act. It has to reach the URL and not just the
    // state: the address bar is what a reload reads, so leaving it on
    // /library/x/item/y restores the previous account's page. (The queue is
    // dropped for both, in the cleared handler above.)
    navigate({ view: 'libraries' }, { replace: true })
    // Release before the tokens go, not as a consequence of the route change
    // below: this effect is declared above the one that watches `playing`, so
    // it runs first, and `signOut` clears the access token, the refresh token
    // and the media cookie synchronously. By the time the route change was
    // observed there was nothing left to authenticate the DELETE with — it
    // 401'd, found no refresh token to repair itself with, and the error was
    // swallowed. Measured: sign out mid-film and the session was still on the
    // hub, holding its transcoder slot until the reaper took it.
    //
    // Clearing the ref stops the route change from sending a second one.
    if (playedRef.current) {
      void endSession(playedRef.current, true)
      playedRef.current = null
    }
    // Tokens dropped at once and the hub told afterwards from a captured copy,
    // which is `signOut`'s whole design: awaiting the network first left a
    // window in which a fast sign-in — autofill and Enter — was wiped by the
    // late answer to the sign-out before it, unexplained, with ITS family left
    // live on the hub.
    void signOut()
  }, [signingOut])

  // The one notice host (UX-1). Registered while the shell is up, so a
  // view can report a failure without a provider or a prop.
  useEffect(() => {
    onNotice((msg) => {
      clearTimeout(noticeTimer.current)
      setNotice(msg)
      noticeTimer.current = setTimeout(() => setNotice(''), NOTICE_MS)
    })
    return () => {
      onNotice(null)
      clearTimeout(noticeTimer.current)
    }
  }, [])

  // Escape closes whichever popover is open. One listener, and only while
  // something is open, so it cannot swallow an Escape the player wants.
  useEffect(() => {
    if (!navOpen && !profileOpen) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      setNavOpen(false)
      setProfileOpen(false)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [navOpen, profileOpen])

  if (bootError)
    return (
      <Failed
        what="Could not start."
        message={bootError}
        // The error stays on screen while the retry is out, and the effect
        // clears it on success. Clearing it here put `phase === 'boot'` back,
        // which renders nothing — so pressing Try again against a wedged hub
        // gave a blank page for the full ten seconds before the message
        // returned.
        onRetry={() => setBootAttempt((n) => n + 1)}
      />
    )
  if (phase === 'boot') return null
  if (phase === 'setup' || phase === 'login')
    return (
      <Auth
        mode={phase}
        note={endedNote}
        // The route is untouched by any of this, so signing back in returns
        // you to the page you were reading rather than to the home screen.
        onDone={() => {
          setEndedNote('')
          setPhase('app')
        }}
      />
    )

  /// Whether this route has a panel at all. Typing in a library page's filter
  /// used to set the open flag too, with nothing there to show it — and the
  /// flag outlived the route, so going back to the home screen mounted the
  /// panel already open over a page nobody had searched.
  const hasPanel = route.view === 'libraries'

  /// The combobox half of the search box, on the one route that has a panel.
  ///
  /// A library page's box is a filter with nothing to pop up, and telling a
  /// screen reader it is a combobox would promise a list that never arrives —
  /// so these go on only where the panel is mounted. `aria-expanded` comes from
  /// the panel rather than being guessed at from `overlayOpen` and a row count:
  /// only the panel knows whether it drew anything, and a query that matched
  /// nothing draws a message with no rows in it. `aria-controls` likewise names
  /// the list only while there is a list — the rest of the time it would point
  /// at an id that is not in the document.
  const combobox = hasPanel
    ? {
        role: 'combobox',
        'aria-autocomplete': 'list' as const,
        'aria-controls': combo.shown ? SEARCH_LIST_ID : undefined,
        'aria-expanded': combo.shown,
        'aria-activedescendant': combo.highlight >= 0 ? searchOptionId(combo.highlight) : undefined,
      }
    : {}

  return (
    <div className="shell">
      <header className="topbar">
        <div className="menu-anchor">
          <button
            className="wordmark"
            title="Jump to…"
            aria-expanded={navOpen}
            onClick={() => {
              setProfileOpen(false)
              setNavOpen((o) => !o)
            }}
          >
            <span>
              kahawai<span className="tilde">~</span>
            </span>
            <span className="wordmark-caret">
              <Icon name={navOpen ? 'chevronUp' : 'chevronDown'} />
            </span>
          </button>
          <Menu open={navOpen} align="left" onClose={() => setNavOpen(false)}>
            <button
              className={menuRowClass(route.view === 'libraries')}
              onClick={() => {
                setNavOpen(false)
                // Going home is a fresh start: a standing filter that
                // silently follows you there reads as missing items.
                setSearch('')
                navigate({ view: 'libraries' })
              }}
            >
              <span className="menu-glyph">
                <Icon name="home" />
              </span>
              Home
            </button>
            {libs.length > 0 && <span className="menu-sep" />}
            {libs.map((l) => (
              <button
                key={l.id}
                className={menuRowClass(route.view === 'library' && route.id === l.id)}
                onClick={() => {
                  setNavOpen(false)
                  setSearch('')
                  navigate({ view: 'library', id: l.id })
                }}
              >
                <span className="menu-glyph">
                  <Icon name={libGlyph(l.media_type)} />
                </span>
                {l.name}
              </button>
            ))}
          </Menu>
        </div>
        {/* Only where it means something. On the player, admin and
            settings there is nothing for it to search, and a box that
            silently does nothing is worse than no box. */}
        {(route.view === 'libraries' || route.view === 'library') && (
          <div className="search">
            <span className="search-icon">
              <Icon name="search" />
            </span>
            <input
              {...combobox}
              ref={searchBox}
              className="search-input"
              placeholder={
                route.view === 'library' ? 'Filter this library' : 'Search all libraries'
              }
              value={search}
              onChange={(e) => {
                setSearch(e.target.value)
                // Typing brings it back, so a dismissed panel is not a dead
                // box — and emptying the field puts it away, since there is
                // nothing left to have results for.
                setOverlayOpen(hasPanel && e.target.value.trim() !== '')
              }}
              // Coming back to a box that still has text should show what it
              // found rather than an empty dropdown or nothing at all.
              onFocus={() => setOverlayOpen(hasPanel && search.trim() !== '')}
              // And clicking it, which is not the same event. Opening a library
              // from the panel leaves focus in the box and navigates; come back
              // to the home screen and the box still holds the query but has
              // never lost focus, so no focus event can fire again. The panel
              // was then unreachable — clicks did nothing, the arrows do not
              // exist while it is closed — and editing the text was the only way
              // back to results you had already fetched.
              onClick={() => setOverlayOpen(hasPanel && search.trim() !== '')}
            />
            {search !== '' && (
              <button
                className="search-clear"
                title="Clear"
                onClick={() => {
                  setSearch('')
                  setOverlayOpen(false)
                }}
              >
                ✕
              </button>
            )}
            {/* Anchored to `.search`, which is already `position: relative` —
                the panel was the one part of the design that never got ported,
                and its anchor point has been sitting here unused. Home only:
                on a library page this box filters that library in place, and a
                dropdown of cross-library hits over a page that is already
                filtering would be two answers to one question. */}
            {route.view === 'libraries' && (
              <SearchOverlay
                open={overlayOpen}
                query={query}
                libs={libs}
                inputRef={searchBox}
                onNav={setCombo}
                onOpenLibrary={(id) => {
                  // The text stays, and becomes that library's filter.
                  setOverlayOpen(false)
                  navigate({ view: 'library', id })
                }}
                onOpenItem={(id, fromLib) => {
                  // Cleared in the same handler as the navigation, so it cannot
                  // be forgotten: you asked for this one thing and got it.
                  setOverlayOpen(false)
                  setSearch('')
                  navigate({ view: 'detail', id, fromLib })
                }}
                onClose={() => setOverlayOpen(false)}
              />
            )}
          </div>
        )}
        <div className="menu-anchor">
          <button
            className="profile-btn"
            title={username()}
            aria-expanded={profileOpen}
            onClick={() => {
              setNavOpen(false)
              setProfileOpen((o) => !o)
            }}
          >
            <span className="avatar">
              <Icon name="user" size={13} />
            </span>
            <span className="profile-name">{username()}</span>
            <span className="wordmark-caret">
              <Icon name={profileOpen ? 'chevronUp' : 'chevronDown'} size={13} />
            </span>
          </button>
          <Menu open={profileOpen} align="right" onClose={() => setProfileOpen(false)}>
            <button
              className={menuRowClass(route.view === 'settings')}
              onClick={() => {
                setProfileOpen(false)
                navigate({ view: 'settings' })
              }}
            >
              <span className="menu-glyph">
                <Icon name="gear" />
              </span>
              Settings
            </button>
            {isAdmin() && (
              <button
                className={menuRowClass(route.view === 'admin')}
                onClick={() => {
                  setProfileOpen(false)
                  navigate({ view: 'admin' })
                }}
              >
                <span className="menu-glyph">
                  <Icon name="shield" />
                </span>
                Admin
              </button>
            )}
            <span className="menu-sep" />
            <button
              className="menu-item leaving"
              // Phase first, tokens after — see the effect below.
              onClick={() => {
                setSigningOut(true)
                setPhase('login')
              }}
            >
              <span className="menu-glyph">
                <Icon name="signOut" />
              </span>
              Sign out
            </button>
          </Menu>
        </div>
      </header>
      {/* Keyed on the screen, which is the path everywhere but the player —
          see `boundaryKey`. A screen that threw stays broken until something
          changes, and going somewhere else is the obvious something. On the
          VIEW alone it was not enough — two items are two screens, so one
          item's caught throw stayed latched over the next one, which would
          have rendered perfectly well. */}
      <Boundary key={boundaryKey(route)} onHome={() => navigate({ view: 'libraries' })}>
        {/* Gated here as well as in the menu. The hub refuses every admin
            route regardless (require_admin covers the whole router), so this
            is not the security boundary — but rendering the screen to
            somebody who cannot use it produces a page of refusals and
            invites the reading that something is broken. */}
        {route.view === 'admin' &&
          (isAdmin() ? (
            <Admin />
          ) : (
            <main className="admin">
              <p className="dim">Administration is for admin accounts.</p>
            </main>
          ))}
        {route.view === 'settings' && <Settings />}
        {route.view === 'libraries' && (
          <Libraries
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
            onHome={() => {
              setSearch('')
              navigate({ view: 'libraries' })
            }}
          />
        )}
        {route.view === 'detail' && (
          <Detail
            // Keyed on the item, as the season view already is. Without it an
            // id change reused the instance, so `Detail`'s own departure guard
            // — a ref flipped in an unmount cleanup — never fired: pressing
            // Play on one episode and clicking another while the negotiation
            // was out opened the player on the FIRST one, over the page you
            // had just moved to.
            key={route.id}
            id={route.id}
            fromLib={route.fromLib}
            onPlay={(id, fromStart) => {
              navigate({ view: 'player', id, fromLib: route.fromLib, fromStart })
            }}
            onOpenEpisode={(id) => navigate({ view: 'detail', id, fromLib: route.fromLib })}
            onOpenLibrary={(id) => navigate({ view: 'library', id })}
            onOpenSeason={(season) =>
              navigate({ view: 'season', showId: route.id, season, fromLib: route.fromLib })
            }
            // An album played whole levels by album gain; a single track
            // dropped into the queue levels by its own. See QueueEntry.
            onPlayAlbum={(tracks, at) => playNow(tracks, at, 'album')}
            onEnqueueAlbum={(tracks) => enqueue(tracks, 'album')}
            onEnqueueTrack={(track) => enqueue([track], 'track')}
            playingId={queue ? (queue.entries[queue.at]?.track.id ?? null) : null}
          />
        )}
        {route.view === 'season' && (
          <Season
            key={`${route.showId}/${route.season}`}
            showId={route.showId}
            season={route.season}
            onOpenShow={(id) => navigate({ view: 'detail', id, fromLib: route.fromLib })}
            onPlay={(id, fromStart) => {
              navigate({ view: 'player', id, fromLib: route.fromLib, fromStart })
            }}
          />
        )}
        {route.view === 'player' && (
          <PlayerRoute
            id={route.id}
            fromLib={route.fromLib}
            fromStart={route.fromStart}
            onSession={setPlayingId}
            onLeave={() => {
              // Pop the entry that opening the player pushed. Replacing it with
              // the item page left two adjacent identical entries, so the first
              // press of browser-back changed nothing — after every single
              // watch, which is how most viewing ends.
              if (window.history.state?.ours) {
                window.history.back()
              } else {
                // Deep-linked straight to /play: there is no entry of ours to
                // pop, and going back would leave the app.
                navigate(
                  { view: 'detail', id: route.id, fromLib: route.fromLib },
                  { replace: true },
                )
              }
            }}
            onHome={() => navigate({ view: 'libraries' })}
            // The next episode replaces this entry rather than stacking one:
            // browser-back should leave the player, not walk back through an
            // evening's autoplay. The route keeps the session it has already
            // started, so this only moves the address.
            onOpenItem={(nextId) =>
              navigate({ view: 'player', id: nextId, fromLib: route.fromLib }, { replace: true })
            }
          />
        )}
      </Boundary>
      {/* Its OWN boundary, not the route's. The queue deliberately survives
          every navigation, so it has the longest life and the most state to get
          wrong — and rendering it outside a boundary meant a throw in it was a
          white page with no header and no way back, which is the incident
          `AlbumPlayer` records at the top of its own file. Keyed on the queue's
          first track so a caught throw clears when the queue changes, and
          `onHome` drops the queue rather than navigating: the thing that threw
          is the thing to put down. */}
      {/* Keyed on a generation, not on the first track: putting the same
          record on again produces the same first track, so a caught throw
          stayed caught and pressing Play did nothing at all, silently, while
          the item page still marked a track as playing. Bumped only by
          `playNow` — appending must not remount a queue mid-track. */}
      <Boundary key={`queue:${queueGen}`} onHome={() => setQueue(null)} className="queue-fail">
        {queue && (
          <AlbumPlayer
            // Not keyed, and it does not need to be: the player matches its
            // warmed sessions to tracks by id, so a queue that changes under
            // it — a different record, or one appended — is handled without
            // throwing away the element that is playing.
            entries={queue.entries}
            at={queue.at}
            onTrackChange={(at) => setQueue((q) => (q ? { ...q, at } : q))}
            onStop={() => setQueue(null)}
            // One pair of ears: the video player takes the sound while it is
            // on screen, and the queue resumes when you leave it.
            paused={route.view === 'player'}
          />
        )}
      </Boundary>
      {/* Outside every view, so a notice survives the view that raised it
          navigating away — and so it cannot be clicked through to. */}
      {notice !== '' && (
        <div className="toast" role="status">
          {notice}
        </div>
      )}
    </div>
  )
}
