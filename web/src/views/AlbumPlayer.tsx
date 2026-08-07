import { useCallback, useEffect, useRef, useState } from 'react'
import { endSession, postProgress, startSessionDirect, type Item, type Session } from '../api'
import { keepSessionAlive } from '../keepalive'
import { isSessionGone, mayRecover } from '../recovery'
import { replayGainFactor } from '../replaygain'

/// Queue playback for an album (HUB-27): one direct-play session per
/// track, auto-advance on ended, prev/next. The <audio> element streams
/// with the media cookie.
///
/// GAPLESS (HUB-19) is why there are TWO elements. Preparing the next
/// track only once the current one ends costs a session round trip plus
/// however long the element needs to buffer — audible on every track
/// boundary, and worst exactly where it matters, on a record that was
/// mixed to run continuously. So the idle element gets the next track's
/// session and buffers it while the current one plays, and `ended` is
/// just a play() on something already loaded.
///
/// The lead time is a compromise between two failures. Too late and the
/// buffer is not warm; too early and the hub reaps the session it
/// belongs to, which it does after about 90 seconds of nobody reading
/// (measured 2026-08-07: started 10:00:53, "ending idle session"
/// 10:02:23). Thirty seconds is comfortably inside that and long enough
/// to fill a buffer over a LAN.
const PRELOAD_LEAD_SECONDS = 30

type Slot = { session: Session | null; index: number | null }

export default function AlbumPlayer({
  tracks,
  startAt,
  onTrackChange,
  onStop,
}: {
  tracks: Item[]
  startAt: number
  onTrackChange: (index: number) => void
  onStop: () => void
}) {
  const [index, setIndex] = useState(startAt)
  const [active, setActive] = useState(0)
  const [error, setError] = useState('')
  // Two elements, two slots, one index into each: `active` says which
  // is playing, the other is the one being warmed.
  const els = [useRef<HTMLAudioElement>(null), useRef<HTMLAudioElement>(null)]
  const slots = useRef<[Slot, Slot]>([
    { session: null, index: null },
    { session: null, index: null },
  ])
  const [, force] = useState(0)
  // ReplayGain rides in a Web Audio gain node rather than the element's
  // volume, because volume is the USER's: setting it here would fight
  // the slider on every track change, and it cannot go above 1.0 for
  // the 126 tracks in this library whose gain is positive. Both
  // elements feed the same node — album gain is one number for the
  // whole record.
  const gainRef = useRef<{ ctx: AudioContext; gain: GainNode } | null>(null)
  const wiredRef = useRef(new WeakSet<HTMLAudioElement>())
  // Where to put the playhead once a recovered session's element loads.
  const resumeAt = useRef(0)

  const release = useCallback((slot: Slot, keepalive = false) => {
    if (slot.session) void endSession(slot.session.session_id, keepalive)
    slot.session = null
    slot.index = null
  }, [])

  /// Give a slot the session for `want`, unless it already has it.
  const prepare = useCallback(
    async (which: 0 | 1, want: number) => {
      const slot = slots.current[which]
      if (slot.index === want || !tracks[want]) return
      release(slot)
      slot.index = want
      try {
        const s = await startSessionDirect(tracks[want].id)
        // The queue may have moved on while the hub answered.
        if (slots.current[which].index !== want) {
          void endSession(s.session_id)
          return
        }
        slots.current[which].session = s
        setError('')
        force((n) => n + 1)
      } catch (e) {
        if (slots.current[which].index === want) setError(String(e))
      }
    },
    [tracks, release]
  )

  // jump when the user clicks another track row
  useEffect(() => setIndex(startAt), [startAt])

  // The active slot must hold the current track. It usually does
  // already, because `ended` swapped to the slot that was warmed; this
  // covers the first track and any jump the user makes.
  useEffect(() => {
    if (!tracks[index]) return
    onTrackChange(index)
    if (slots.current[active].index !== index) void prepare(active as 0 | 1, index)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [index, active, tracks])

  useEffect(
    () => () => {
      // keepalive: the page may be closing, and an unsent DELETE leaves
      // a session for the reaper.
      for (const slot of slots.current) release(slot, true)
      void gainRef.current?.ctx.close()
    },
    [release]
  )

  // A direct-play element stops fetching the moment it has the whole
  // file, which for a FLAC is a minute or two into a track — so without
  // a ping the reaper ends the session under a track that is still
  // playing, and the progress post and DELETE at `ended` both 404.
  // Measured 2026-08-07: track 2 of an album reaped 3½ minutes into
  // being audible. Policy and bound live in keepalive.ts.
  const activeSession = slots.current[active].session
  const idleSession = slots.current[1 - active].session

  /// The hub answered 410: this session is gone and no ping will bring
  /// it back. A direct-play music session is cheap to rebuild — a lease,
  /// not a pipeline — so take a fresh one and put the playhead back where
  /// it was. Nothing here knows how long a session may idle; the 410 is
  /// the entire trigger (see recovery.ts).
  /// Not while the queue is paused: a restart there spends a lease on
  /// audio nobody is listening to, and the fresh session goes idle and
  /// is reaped in turn — a paused queue would respawn one forever. The
  /// death is remembered instead and acted on when play is pressed.
  const deadRef = useRef(false)
  const recoverSlot = useCallback(
    async (which: 0 | 1, want: number, resumeSeconds: number, paused: boolean) => {
      if (paused) {
        deadRef.current = true
        return
      }
      if (!mayRecover(tracks[want]?.id ?? 'album', resumeSeconds * 1000, performance.now())) {
        setError('playback session ended and could not be restarted')
        return
      }
      resumeAt.current = resumeSeconds
      // prepare() no-ops when the slot already claims this index, and it
      // does — with a session the hub has forgotten. Drop the claim.
      slots.current[which].index = null
      await prepare(which, want)
    },
    [tracks, prepare]
  )

  useEffect(() => {
    if (!activeSession) return
    return keepSessionAlive(
      () => (els[active].current?.currentTime ?? 0) * 1000,
      (ms) =>
        void postProgress(activeSession.session_id, ms).then((r) => {
          if (isSessionGone(r))
            void recoverSlot(
              active as 0 | 1,
              index,
              els[active].current?.currentTime ?? 0,
              els[active].current?.paused ?? false
            )
        })
    )
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeSession, active, index])
  // The preloaded session has finished fetching and reads nothing more,
  // so a pause while it is hot would let the reaper take it before it is
  // ever heard and the swap would land on a dead URL. A position that
  // never moves is exactly what keepSessionAlive already handles — and if
  // it is taken anyway, warming it again costs one lease and no audio.
  useEffect(() => {
    if (!idleSession) return
    return keepSessionAlive(
      () => 0,
      (ms) =>
        void postProgress(idleSession.session_id, ms).then((r) => {
          // The preload is judged by the AUDIBLE element: warming the
          // next track while the queue sits paused is the same waste.
          if (isSessionGone(r))
            void recoverSlot(
              (1 - active) as 0 | 1,
              index + 1,
              0,
              els[active].current?.paused ?? false
            )
        })
    )
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [idleSession, active, index])

  const factor = replayGainFactor(tracks[index], 'album')
  useEffect(() => {
    const el = els[active].current
    if (!el) return
    const Ctor =
      window.AudioContext ?? (window as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext
    if (!Ctor) return // no Web Audio: play unlevelled rather than not at all
    if (!gainRef.current) {
      const ctx = new Ctor()
      gainRef.current = { ctx, gain: ctx.createGain() }
      gainRef.current.gain.connect(ctx.destination)
    }
    const { ctx, gain } = gainRef.current
    // A source node can only ever be created ONCE per element, so each
    // element is wired the first time it plays and never again.
    for (const ref of els) {
      const e = ref.current
      if (e && !wiredRef.current.has(e)) {
        ctx.createMediaElementSource(e).connect(gain)
        wiredRef.current.add(e)
      }
    }
    // Autoplay policy suspends a context created before a gesture.
    if (ctx.state === 'suspended') void ctx.resume()
    gain.gain.value = factor
  })

  const advance = (dir: number) => {
    const next = index + dir
    if (next < 0 || next >= tracks.length) {
      onStop()
      return
    }
    setIndex(next)
  }

  /// The current track is nearly over: warm the other slot.
  const onTime = (which: 0 | 1) => {
    if (which !== active) return
    const el = els[which].current
    if (!el || !isFinite(el.duration)) return
    if (el.duration - el.currentTime > PRELOAD_LEAD_SECONDS) return
    const next = index + 1
    if (next < tracks.length) void prepare((1 - which) as 0 | 1, next)
  }

  /// Hand over to the slot that has been buffering. No session start,
  /// no load: the next track is already there.
  const onEnded = (which: 0 | 1) => {
    if (which !== active) return
    const finished = slots.current[which]
    const el = els[which].current
    if (finished.session && el) void postProgress(finished.session.session_id, el.duration * 1000)
    const next = index + 1
    const other = (1 - which) as 0 | 1
    if (next >= tracks.length) {
      onStop()
      return
    }
    release(finished)
    if (slots.current[other].index === next) {
      setActive(other)
      setIndex(next)
      void els[other].current?.play()
      return
    }
    // The warm-up did not happen (a very short track, or a slow hub):
    // fall back to loading in place, which is what this used to do.
    setIndex(next)
  }

  const track = tracks[index]
  return (
    <div className="queue-bar">
      <button className="btn ghost small" onClick={() => advance(-1)} disabled={index === 0}>
        ⏮
      </button>
      <button className="btn ghost small" onClick={() => advance(1)}>
        ⏭
      </button>
      <span className="now mono">
        {track ? `${index + 1}/${tracks.length} · ${track.title}` : ''}
      </span>
      {error && <span className="error">{error}</span>}
      {([0, 1] as const).map((which) => {
        const slot = slots.current[which]
        return (
          slot.session && (
            <audio
              key={which}
              ref={els[which]}
              src={slot.session.stream_url}
              autoPlay={which === active}
              preload="auto"
              controls={which === active}
              hidden={which !== active}
              onPlay={() => {
                if (which !== active || !deadRef.current) return
                deadRef.current = false
                void recoverSlot(which, index, els[which].current?.currentTime ?? 0, false)
              }}
              onTimeUpdate={() => onTime(which)}
              onEnded={() => onEnded(which)}
              // A recovered session streams the same file from the top;
              // put the playhead back where the dead one left off.
              onLoadedMetadata={() => {
                if (which !== active || resumeAt.current <= 0) return
                const el = els[which].current
                if (el) el.currentTime = resumeAt.current
                resumeAt.current = 0
              }}
              // The element reports a failure with no status of its own,
              // so ask the hub what kind it was — 410 means the session
              // went away, anything else is a real media fault.
              onError={() => {
                const s = slots.current[which].session
                if (!s) return
                void postProgress(s.session_id, 0).then((r) => {
                  if (!isSessionGone(r)) return
                  const at = which === active ? (els[which].current?.currentTime ?? 0) : 0
                  void recoverSlot(
                    which,
                    which === active ? index : index + 1,
                    at,
                    els[active].current?.paused ?? false
                  )
                })
              }}
            />
          )
        )
      })}
      <button className="btn ghost small" onClick={onStop}>
        ✕
      </button>
    </div>
  )
}
