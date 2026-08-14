import { useEffect, useRef, useState, type ComponentType, type CSSProperties } from 'react'
import { fetchItem } from '../item-query'
import {
  endSession,
  fetchLibraries,
  prefsOrNone,
  resolveTracks,
  startPlaybackSession,
  type ItemDetail,
  type Pref,
  type Session,
} from '../api'
import { loadChunk } from '../chunk'
import { isSourceOffline } from '../recovery'
import Failed from '../Failed'
import { notify } from '../toast'

const importPlayer = () => loadChunk('player', () => import('./Player'))
type PlayerComponent = ComponentType<React.ComponentProps<typeof import('./Player').default>>

/// The player as a page: everything between a `/play` URL and a picture.
///
/// Acquiring a session used to belong to the item page, which made `/play` an
/// instruction to that page rather than an address of its own. Deep-linking
/// showed the details for a second — Play button and all — before swapping,
/// browser-back landed on the home screen because no item entry ever existed,
/// and the same refusal was rendered two ways depending on which button you
/// pressed. All three were one cause: a route carrying objects no URL can
/// reconstruct.
///
/// The session lives here rather than in `Player` so that a restart still
/// remounts the player — it keeps a run's worth of refs and expects a fresh
/// mount per session — while the ROUTE stays the same page.
export default function PlayerRoute({
  id,
  fromLib,
  fromStart,
  onSession,
  onLeave,
  onHome,
  onOpenItem,
}: {
  id: string
  fromLib: string
  /// Play from the beginning rather than resuming. A hint from the button that
  /// was pressed; a bare URL always resumes, which is the safe default.
  fromStart?: boolean
  /// Reported so the shell can release the previous session. Lifetime belongs
  /// to whoever owns the route, which is not this component — it is torn down
  /// and rebuilt for reasons of React's own.
  onSession: (sessionId: string | null) => void
  onLeave: () => void
  onHome: () => void
  /// The next episode: the URL has to follow it, without this component
  /// remounting and throwing away the session it has already started.
  onOpenItem: (itemId: string) => void
}) {
  const [item, setItem] = useState<ItemDetail | null>(null)
  const [session, setSession] = useState<Session | null>(null)
  const [resumeMs, setResumeMs] = useState(0)
  const [failure, setFailure] = useState('')
  const [attempt, setAttempt] = useState(0)
  /// The player's own code, resolved by hand rather than through `lazy` and
  /// `Suspense`. A suspended boundary renders its fallback for a beat even when
  /// the chunk is already in memory, and that fallback is a second <main>: the
  /// frame was being thrown away and rebuilt twice on the way to a picture.
  const [PlayerComp, setPlayerComp] = useState<PlayerComponent | null>(null)
  /// The frame's own state, because the frame is the thing that persists. The
  /// player decides it and is replaced whenever the session is; this element is
  /// not, which is the whole point — the window, and the way out of it, must
  /// not blink when the picture behind them is rebuilt.
  const [mode, setMode] = useState<'window' | 'theater' | 'full'>('window')
  /// A session started after the viewer left is one nobody will play, ping or
  /// end. Reset on setup as well as teardown: StrictMode mounts twice.
  const left = useRef(false)
  /// The failure is the chunk, so only a reload can clear it. A rejected
  /// `import()` is recorded against the specifier for the life of the page:
  /// asking again returns the SAME rejection with no request — measured at 0ms
  /// against 2ms for the first attempt — so a Try again that re-imports is a
  /// button that cannot work. Reloading is what `loadChunk` itself does for the
  /// first failure, and this is the viewer asking for the second.
  const chunkDead = useRef(false)
  useEffect(() => {
    left.current = false
    // In parallel with the session, not after it. `lazy` only starts fetching
    // when React first renders the component — which is when the session
    // lands — so the chunk's round trip used to be a second wait, and its
    // Suspense fallback a second frame between the two.
    void importPlayer()
      // Stored as a value, so React does not read the component as an updater.
      .then((m) => setPlayerComp(() => m.default))
      .catch((e: unknown) => {
        if (left.current) return
        // `loadChunk` has already reloaded the page once by the time it lets a
        // rejection through — that is its contract, and it means the reload did
        // not help. Swallowed, this rendered the starting veil for ever: a
        // spinner labelled "Starting playback", with no error and no way out.
        // The session is released by the shell on the way out, but nothing
        // pings it until then, so it holds its slot until the hub reaps it.
        chunkDead.current = true
        setFailure(`Could not load the player. ${e}`)
      })
    return () => {
      left.current = true
    }
  }, [])

  useEffect(() => {
    // Before the guard, or Try again cannot clear a failure that was not the
    // session's. A chunk that failed leaves this route holding a good session
    // for the right item, so the guard returns — and with the clear below it,
    // the retry it just asked for silently did nothing, for ever.
    setFailure('')
    // Already playing this item — the next-episode handover sets both at once,
    // and the URL catching up must not start a second session for it.
    if (session && item?.id === id) return
    let stale = false
    void (async () => {
      try {
        const d = await fetchItem(id)
        if (stale || left.current) return
        setItem(d)
        const start = fromStart ? 0 : (d.resume_position_ms ?? 0)
        const audio = d.sources_detail[0]?.streams?.audio ?? []
        let audioTrack = 0
        let prefs: Pref[] = []
        try {
          const [p, libs] = await Promise.all([
            prefsOrNone(),
            // Guarded: `prefs` is assigned after this await, so a rejection
            // here left it `[]` — and `[]` is not nullish, so the prefs that
            // DID arrive were replaced by nothing. That drops the bandwidth cap
            // silently and starts on track 0, which is the anime-in-English
            // bug. The media type is the only thing actually at stake.
            fetchLibraries().catch((e: unknown) => {
              notify(`Could not load the library details: ${e}`)
              return { libraries: [] }
            }),
          ])
          prefs = p.prefs
          audioTrack = resolveTracks(
            p.prefs,
            d.parent_id ?? d.id,
            d.id,
            libs.libraries.find((x) => x.id === fromLib)?.media_type ?? '',
            d.metadata?.original_language,
            audio,
          ).audioTrack
        } catch (e) {
          // Both halves report and fall back, so this is `resolveTracks` itself
          // — a bug rather than an outage. Said out loud, because the track it
          // failed to pick is the one about to play.
          notify(`Could not resolve the audio track: ${e}`)
        }
        const s = await startPlaybackSession(d, start, audioTrack, 0, prefs)
        if (stale || left.current) return void endSession(s.session_id, true)
        setResumeMs(start)
        setSession(s)
      } catch (e) {
        if (stale || left.current) return
        // Whatever session this route was holding, it is not the one on screen
        // any more: the guard above reads `session && item?.id === id`, so a
        // failure that leaves the OLD session beside the NEW item makes Try
        // again return early and hand the player one item's metadata over
        // another item's stream. Dropping it also reports the release, so the
        // shell ends it rather than leaving it to be reaped.
        setSession(null)
        // A 503 is the hub saying the machine holding the file is not
        // answering. It is a wait rather than a fault, and it reads the same
        // here as it does inside the player.
        setFailure(
          isSourceOffline(e)
            ? 'The machine holding this file is not answering. Try again in a moment.'
            : String(e),
        )
      }
    })()
    return () => {
      stale = true
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id, attempt])

  useEffect(() => {
    if (session) onSession(session.session_id)
    // Reported as gone on the way out, so the shell is not left holding an id
    // it has already released — a tab closed during the next start would arm
    // its unload handler on the dead one.
    return () => onSession(null)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session?.session_id])

  if (failure)
    return (
      <Failed
        what="Could not start playback."
        message={failure}
        onRetry={() => (chunkDead.current ? location.reload() : setAttempt((n) => n + 1))}
        away={{ label: 'Back to the item', go: onLeave }}
      />
    )

  /// One frame for the whole visit: the window, and the way out of it. What
  /// goes inside changes — a veil while the session is being started, then the
  /// player, then a different player each time a restart replaces the session —
  /// and none of those swaps touches this element, so nothing about the page
  /// around the picture ever blinks.
  return (
    <main className={`player mode-${mode}`}>
      {mode === 'window' && (
        <button className="btn ghost small back" onClick={onLeave}>
          ← Back
        </button>
      )}
      {!item || !session || !PlayerComp ? (
        <div
          className="videobox"
          data-starting="1"
          style={
            {
              '--video-ratio':
                item?.negotiated?.source?.display_width && item.negotiated.source.display_height
                  ? `${item.negotiated.source.display_width} / ${item.negotiated.source.display_height}`
                  : '16 / 9',
            } as CSSProperties
          }
        >
          <div className="seek-veil" aria-label="Starting playback">
            <span className="seek-veil-spin">&#10227;</span>
          </div>
        </div>
      ) : (
        <PlayerComp
          key={session.session_id}
          item={item}
          session={session}
          resumeMs={resumeMs}
          libraryId={fromLib}
          mode={mode}
          setMode={setMode}
          onClose={onLeave}
          onRestart={(fresh, at) => {
            setResumeMs(at)
            setSession(fresh)
          }}
          onHome={onHome}
          onPlayNext={(nextItem, nextSession) => {
            setItem(nextItem)
            setResumeMs(0)
            setSession(nextSession)
            onOpenItem(nextItem.id)
          }}
        />
      )}
    </main>
  )
}
