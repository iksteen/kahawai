import { useCallback, useEffect, useRef, useState } from 'react'
import { endSession, postProgress, startSessionDirect, type Session } from '../api'
import { keepSessionAlive } from '../keepalive'
import { isSessionGone, mayRecover, startRetry } from '../recovery'
import { replayGainFactor, type QueueEntry } from '../replaygain'
import Icon from '../icons'

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

/// How long to wait before asking again for a track the hub could not start.
/// A client's own pacing choice, not a mirror of anything the hub knows.
const RETRY_MS = 5000

/// How many times to ask again about a refusal that MIGHT clear — see
/// `startRetry`.
///
/// This used to carry the stream cap: the hub said 409 both for "this item has
/// no sources" and for "too many concurrent streams", so the client could not
/// tell a permanent refusal from a queue holding two sessions while a film
/// holds another, and three attempts was a guess standing in for an answer the
/// hub could give. It gives it now — the cap is a 429, `startRetry` calls it
/// `busy`, and this player waits it out under a much larger ceiling of its
/// own (`BUSY_TRIES`).
///
/// What is left is a backstop for a 409 whose cause this client does not
/// enumerate. Three attempts spans two retry intervals, which costs ten
/// seconds before an unplayable track stops asking.
const REFUSAL_TRIES = 3

/// How many times to ask again about the account's STREAM CAP, which clears
/// by itself — but not always by itself here.
///
/// This player holds two sessions, so with `max_sessions_per_user` set low
/// enough the warm slot is refused by the album's own active one and the
/// condition can never clear: unbounded, that tab re-POSTs a session every
/// five seconds for as long as it is open. Before the cap became its own
/// status it was bounded at three, by accident, because it was
/// indistinguishable from a dead track.
///
/// Five minutes rather than ten seconds, because the ordinary case — a film
/// playing elsewhere — really does clear, and giving up on it in ten seconds
/// is what this whole split was meant to stop. It is a backstop against
/// polling for ever, not a guess at how long a film is.
const BUSY_TRIES = 60

/// How long a session start may take before it counts as failed.
///
/// `fetch` has no timeout of its own, and a request that never settles used to
/// hold a slot's claim for ever — nothing rendered an element for it, so no
/// error arrived, and the claim is what stops anything else from retrying. Our
/// own pacing choice, not a mirror of any hub timeout: on a LAN a lease is
/// milliseconds, and a hub too busy to answer in fifteen seconds is a hub the
/// retry timer should be waiting out instead.
const START_TIMEOUT_MS = 15000

/// The lead time is a compromise between two failures. Too late and the buffer
/// is not warm; too early and the hub reaps the session it belongs to, which it
/// does after about 90 seconds of nobody reading (measured 2026-08-07: started
/// 10:00:53, "ending idle session" 10:02:23). Thirty seconds is comfortably
/// inside that and long enough to fill a buffer over a LAN.
const PRELOAD_LEAD_SECONDS = 30

/// A warmed session, remembered by WHICH TRACK it is for.
///
/// It used to remember the index, and that was wrong the moment the queue
/// could change under it: index 5 of one record counted as a match for
/// index 5 of the next, so the bar went on playing the old album.
///
/// The KEY is the claim: a slot holds it from the moment it asks for a session
/// until something releases it, so nothing starts a second request for the same
/// track. A failed claim is KEPT, which is what stops `onTimeUpdate` asking
/// again on the next frame; only the retry timer drops one.
///
/// `trouble` says why a slot cannot be used, and `null` — idle, asking, or
/// holding a good session — is the case everything else is written for. Each
/// value has exactly one thing that acts on it: `failed` is the retry timer's,
/// `dead` is the play button's, `refused` is nobody's because there is nothing
/// to do about it, and `lost` is nobody's either, which is the point of it.
/// Anything handing playback to a slot checks for `null`.
///
/// `error` is per slot because a failed PRELOAD is not something to show: the
/// track playing is fine, the timer is still trying, and if it never succeeds
/// the handover falls back to loading in place and reports it then. One shared
/// string put the idle slot's failure under a playing track and left it there.
type Slot = {
  session: Session | null
  key: string | null
  trouble: 'failed' | 'refused' | 'dead' | 'lost' | null
  error: string
  /// Consecutive refusals that might clear, and the track they are counted
  /// against. Both live here rather than being reset by `release`: that runs
  /// before EVERY attempt, including the retry timer's, so zeroing it there put
  /// the ceiling permanently out of reach and the queue back to asking for
  /// ever. A counter tied to its track needs no resetting — it simply does not
  /// apply to the next one.
  tries: number
  triesFor: string | null
  /// Consecutive stream-cap refusals, NOT tied to a track.
  ///
  /// The cap is about the ACCOUNT, so counting it against `triesFor` meant it
  /// never counted: the warm slot takes a new track id on every advance, which
  /// zeroed the count long before any ceiling. With three-minute tracks a cap
  /// that never clears reached about 36 of 60 and then started over, so the
  /// tab asked every five seconds for the whole album — twelve times the rate
  /// it had before the cap was given a status of its own.
  busyTries: number
}

export default function AlbumPlayer({
  entries,
  at: index,
  onTrackChange,
  onStop,
  paused: forcePaused = false,
}: {
  entries: QueueEntry[]
  /// WHICH TRACK is playing — owned by the parent, which is where the queue
  /// lives. This used to be a starting point mirrored into local state, and the
  /// mirror wrote back through `onTrackChange`: switching records with the old
  /// index still in hand put the two into a stable two-cycle, one album's index
  /// against the other's, a session started and dropped on every render.
  /// Measured before the change: 44,000 session starts from one album switch,
  /// about a thousand a second, until React gave up with "Maximum update depth
  /// exceeded" — thrown outside the error boundary, so it took the app with it.
  at: number
  /// The only way the index moves. There is no local copy to disagree with.
  onTrackChange: (index: number) => void
  onStop: () => void
  /// One pair of ears: the video player asks for silence while it has the
  /// screen, and the queue picks up where it left off afterwards.
  paused?: boolean
}) {
  const [active, setActive] = useState(0)
  // Two elements, two slots, one index into each: `active` says which
  // is playing, the other is the one being warmed.
  const els = [useRef<HTMLAudioElement>(null), useRef<HTMLAudioElement>(null)]
  const slots = useRef<[Slot, Slot]>([
    { session: null, key: null, trouble: null, error: '', tries: 0, triesFor: null, busyTries: 0 },
    { session: null, key: null, trouble: null, error: '', tries: 0, triesFor: null, busyTries: 0 },
  ])
  /// Bumped on every prepare, won or lost. The value is read only as an
  /// effect dependency: a retry that fails again has to re-arm the timer, and
  /// an identical error string is not a state change.
  const [tick, force] = useState(0)
  // ReplayGain rides in a Web Audio gain node rather than the element's
  // volume, because volume is the USER's: setting it here would fight
  // the slider on every track change, and it cannot go above 1.0 for
  // the 126 tracks in this library whose gain is positive. Both
  // elements feed the same node — album gain is one number for the
  // whole record.
  const gainRef = useRef<{ ctx: AudioContext; gain: GainNode } | null>(null)
  const wiredRef = useRef(new WeakSet<HTMLAudioElement>())
  /// Where to put the playhead once a recovered session's element loads, and
  /// WHICH TRACK that position belongs to. A bare number outlived the track it
  /// was measured on: recover at 0:42, jump to another track before the new
  /// session arrives, and the jumped-to track started 42 seconds in — or ended
  /// at once, if it was shorter than that.
  const resumeAt = useRef<{ key: string; at: number } | null>(null)
  const [queueOpen, setQueueOpen] = useState(false)
  const [pos, setPos] = useState(0)
  const [dur, setDur] = useState(0)
  const [isPaused, setIsPaused] = useState(false)

  const release = useCallback((slot: Slot, keepalive = false) => {
    if (slot.session) void endSession(slot.session.session_id, keepalive)
    slot.session = null
    slot.key = null
    slot.trouble = null
    slot.error = ''
  }, [])

  /// Give a slot the session for `want`, unless it already has it.
  const prepare = useCallback(
    async (which: 0 | 1, want: number) => {
      const slot = slots.current[which]
      const track = entries[want]?.track
      if (!track || slot.key === track.id) return
      release(slot)
      slot.key = track.id
      slot.trouble = null
      try {
        const s = await startSessionDirect(track.id, AbortSignal.timeout(START_TIMEOUT_MS))
        // The queue may have moved on while the hub answered — or another
        // attempt for this same track may have got there first, which the key
        // alone cannot tell apart: `recoverSlot` drops a claim without knowing
        // whether a request is already out on it, and the loser used to
        // overwrite `session` and leak the one it replaced.
        if (slots.current[which].key !== track.id || slots.current[which].session) {
          void endSession(s.session_id)
          return
        }
        slots.current[which].session = s
        slots.current[which].error = ''
        slots.current[which].tries = 0
        slots.current[which].triesFor = null
        // This slot always; the other one only when TWO sessions now coexist.
        //
        // Three cuts, because the signal is narrower than it looks. Per track
        // was wrong: the cap is about the account, so a new track id says
        // nothing about it and the ceiling was never reached. Both slots on
        // any success was also wrong, in the opposite direction: the active
        // slot releases its own session before starting the next track, so
        // under a permanent cap it succeeds on every advance and kept clearing
        // the warm slot's count — the ceiling unreachable again, and the tab
        // polling for the whole album.
        //
        // What makes a `busy` count stale is proof the account has room for a
        // SECOND session, and only two live sessions are that.
        slots.current[which].busyTries = 0
        const other = which === 0 ? 1 : 0
        if (slots.current[other].session) slots.current[other].busyTries = 0
        force((n) => n + 1)
      } catch (e) {
        if (slots.current[which].key !== track.id) return
        // The claim is KEPT, and the retry timer below is the only thing that
        // drops it. Giving it back here is what let `onTimeUpdate` ask again on
        // the very next frame — four times a second while a host was away, and
        // during the index disagreement above as fast as the hub could refuse.
        //
        // Worth asking again only when the answer could change. 503 is the
        // mediahost being away and a timeout is nobody answering; both come
        // back. Everything else the start endpoint says — 409 for no sources,
        // unplayable, or over the stream cap — refuses for ever, and this was
        // re-POSTing it every five seconds for as long as the tab was open,
        // never advancing and never giving up. `startRetry` is where that line
        // lives now, for the film as well as the track — they used to disagree
        // about what counts as "come back later".
        // Nothing here is on screen to read a sentence, so `wait` and `busy`
        // both mean "ask again" — but they are counted apart, because only
        // `wait` is certain to be somebody else's to clear. A player in front
        // of a person tells them apart differently; see `StartRetry`.
        const verdict = startRetry(e)
        const slot_ = slots.current[which]
        // A condition that clears itself starts the count over, so a host that
        // flaps for an hour is still waited out.
        //
        // Two counters, because the two refusals are about different things. A
        // 409 is about the TRACK, so it is counted against the track and does
        // not apply to the next one. The cap is about the ACCOUNT, so it is
        // counted on the slot and survives an advance — counted per track it
        // never reached its ceiling at all, which is the whole reason it has
        // one.
        if (slot_.triesFor !== track.id) {
          slot_.triesFor = track.id
          slot_.tries = 0
        }
        // Only `wait` clears them, and it clears both: it is the one verdict
        // that says nothing is wrong with either the track or the account.
        // Zeroing each on the OTHER's verdict was a way to poll for ever — a
        // cap that flickers while a track is genuinely unplayable alternates
        // 429 and 409, and each answer reset the other's count, so neither
        // ceiling was ever reached.
        if (verdict === 'wait') {
          slot_.busyTries = 0
          slot_.tries = 0
        } else if (verdict === 'busy') {
          slot_.busyTries += 1
        } else {
          slot_.tries += 1
        }
        const asking =
          verdict === 'wait' ||
          (verdict === 'busy' ? slot_.busyTries < BUSY_TRIES : slot_.tries < REFUSAL_TRIES)
        slots.current[which].trouble = asking ? 'failed' : 'refused'
        // A timeout arrives as `Offline` like any other unanswered request:
        // api() wraps every fetch rejection, abort included.
        slots.current[which].error = String(e)
        force((n) => n + 1)
      }
    },
    [entries, release],
  )

  // The active slot must hold the current track. It usually does
  // already, because `ended` swapped to the slot that was warmed; this
  // covers the first track and any jump the user makes.
  useEffect(() => {
    if (!entries[index]) return
    if (slots.current[active].key !== entries[index].track.id) void prepare(active as 0 | 1, index)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [index, active, entries])

  // Keep asking, the way the video player waits out an absent mediahost
  // (UI-19). Without this a failed prepare was terminal for the queue: the
  // host coming back changed nothing, because nothing was still looking.
  // ...for BOTH slots. The idle one had no timer at all: its only driver was
  // `onTimeUpdate`, so a failed preload was retried on every frame of the
  // current track and then never again once that track ended.
  useEffect(() => {
    const failed = ([0, 1] as const).filter((w) => slots.current[w].trouble === 'failed')
    if (!failed.length) return
    const t = setTimeout(() => {
      for (const w of failed) {
        // Drop the claim: prepare() no-ops while a slot still holds one, which
        // is exactly what keeps everything else from retrying in the meantime.
        slots.current[w].key = null
        slots.current[w].trouble = null
        void prepare(w, w === active ? index : index + 1)
      }
    }, RETRY_MS)
    return () => clearTimeout(t)
    // `tick` is in the deps because a failed attempt re-renders through it,
    // which is what re-arms this timer — an identical failure is not a change
    // any other dependency can see.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [index, active, entries, tick])

  // A queue change orphans whatever the OTHER slot was warming: it is a track
  // from the album you just left, and its own keepalive keeps it alive
  // against the per-user session cap — four of those and a film that was
  // playing cannot recover, because its restart is refused for concurrency.
  useEffect(() => {
    const idle = slots.current[1 - active]
    // On the claim, not the session: a request still out for a track from the
    // record you just left arrives to a key that still matches, and is stored
    // and then pinged for half an hour against the per-user session cap —
    // which is the very thing this effect was written to prevent. Releasing
    // clears the key, so the reply ends itself instead.
    if (idle.key && idle.key !== entries[index + 1]?.track.id) release(idle)
    // `index` as well as `entries`: jumping to another track within the SAME
    // queue leaves `entries` identical, so the warmed slot kept a session for a
    // track nobody is going to play — pinged by its own keepalive, against a
    // per-user cap of four. Two queue sessions plus one orphan plus a film is
    // the cap, and the film is the one that cannot recover.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [entries, index, active])

  useEffect(
    () => () => {
      // keepalive: the page may be closing, and an unsent DELETE leaves
      // a session for the reaper.
      for (const slot of slots.current) release(slot, true)
      void gainRef.current?.ctx.close()
    },
    [release],
  )

  // A direct-play element stops fetching the moment it has the whole
  // file, which for a FLAC is a minute or two into a track — so without
  // a ping the reaper ends the session under a track that is still
  // playing, and the progress post and DELETE at `ended` both 404.
  // Measured 2026-08-07: track 2 of an album reaped 3½ minutes into
  // being audible. Policy and bound live in keepalive.ts.
  const activeSession = slots.current[active].session
  const idleSession = slots.current[1 - active].session

  /// The hub answered 404: this session is gone and no ping will bring
  /// it back. A direct-play music session is cheap to rebuild — a lease,
  /// not a pipeline — so take a fresh one and put the playhead back where
  /// it was. Nothing here knows how long a session may idle; the 404 is
  /// the entire trigger (see recovery.ts).
  /// Not while the queue is paused: a restart there spends a lease on
  /// audio nobody is listening to, and the fresh session goes idle and
  /// is reaped in turn — a paused queue would respawn one forever. The
  /// death is remembered instead and acted on when play is pressed.
  const recoverSlot = useCallback(
    async (which: 0 | 1, want: number, resumeSeconds: number, paused: boolean) => {
      if (paused) {
        slots.current[which].trouble = 'dead'
        return
      }
      if (
        !mayRecover(entries[want]?.track.id ?? 'queue', resumeSeconds * 1000, performance.now())
      ) {
        // Not released, and not retried: the slot keeps a session the hub has
        // forgotten precisely so that the claim stops anything asking again —
        // which is what `mayRecover` just refused. Marked so the handover
        // cannot pick it up, because a dead preload would otherwise become the
        // audible track.
        slots.current[which].trouble = 'lost'
        slots.current[which].error = 'playback session ended and could not be restarted'
        force((n) => n + 1)
        return
      }
      // Only the audible slot has a position worth restoring, and a preload
      // recovering at 0 would otherwise wipe one the active slot was waiting
      // to use.
      const resuming = entries[want]?.track.id
      if (resumeSeconds > 0 && resuming) resumeAt.current = { key: resuming, at: resumeSeconds }
      // prepare() no-ops when the slot already claims this track, and it
      // does — with a session the hub has forgotten. Drop the claim.
      slots.current[which].key = null
      await prepare(which, want)
    },
    [entries, prepare],
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
              els[active].current?.paused ?? false,
            )
        }),
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
              els[active].current?.paused ?? false,
            )
        }),
    )
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [idleSession, active, index])

  // Per entry, not per queue: see QueueEntry.
  const factor = replayGainFactor(entries[index]?.track, entries[index]?.gain ?? 'album')
  useEffect(() => {
    const el = els[active].current
    if (!el) return
    const Ctor =
      window.AudioContext ??
      (window as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext
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
    if (next < 0 || next >= entries.length) {
      onStop()
      return
    }
    onTrackChange(next)
  }

  /// Straight to a track in the list. Same mechanism as advance — the
  /// index is what drives the slots.
  const goTo = (want: number) => {
    if (want < 0 || want >= entries.length || want === index) return
    onTrackChange(want)
  }

  const togglePause = () => {
    const el = els[active].current
    if (!el) return
    if (el.paused) void el.play()
    else el.pause()
  }

  // Silence on request, and back to whatever it was doing afterwards —
  // resuming something the listener had paused themselves would be the
  // player deciding for them.
  const wasPlaying = useRef(false)
  useEffect(() => {
    const el = els[active].current
    if (!el) return
    if (forcePaused) {
      wasPlaying.current = !el.paused
      el.pause()
    } else if (wasPlaying.current) {
      wasPlaying.current = false
      void el.play()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [forcePaused, active])

  /// The current track is nearly over: warm the other slot.
  const onTime = (which: 0 | 1) => {
    if (which !== active) return
    const el = els[which].current
    if (!el) return
    setPos(el.currentTime)
    setDur(isFinite(el.duration) ? el.duration : 0)
    if (!isFinite(el.duration)) return
    if (el.duration - el.currentTime > PRELOAD_LEAD_SECONDS) return
    const next = index + 1
    if (next < entries.length) void prepare((1 - which) as 0 | 1, next)
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
    if (next >= entries.length) {
      onStop()
      return
    }
    release(finished)
    // The warmed slot is the one already holding the next TRACK — with a
    // session. Keeping a failed claim made "claimed but unplayable" reachable
    // for the first time, and matching on the key alone handed playback to a
    // slot with no element at all: no audio, and the effect above declining to
    // help because the key it wanted was already claimed.
    if (
      slots.current[other].session &&
      !slots.current[other].trouble &&
      slots.current[other].key === entries[next]?.track.id
    ) {
      setActive(other)
      onTrackChange(next)
      void els[other].current?.play()
      return
    }
    // The warm-up did not happen (a very short track, or a slow hub):
    // fall back to loading in place, which is what this used to do.
    onTrackChange(next)
  }

  // The audible slot's failure, and only that one.
  const error = slots.current[active].error
  const track = entries[index]?.track
  const pct = dur > 0 ? Math.min(100, (pos / dur) * 100) : 0
  const how = slots.current[active].session?.content_type?.split('/').pop() ?? ''

  return (
    <div className="queue-dock">
      {queueOpen && (
        <div className="queue-list">
          <div className="queue-list-head">
            <span className="mono">QUEUE · {entries.length}</span>
            <button className="linklike mono queue-clear" onClick={onStop}>
              clear
            </button>
          </div>
          <div className="queue-rows">
            {entries.map(({ track: t }, i) => (
              <button
                key={t.id}
                className={`queue-row${i === index ? ' now' : ''}`}
                onClick={() => goTo(i)}
              >
                {/* The playing row is marked rather than numbered: which
                    one it is matters more than where it sits. */}
                <span className="mono queue-mark">{i === index ? '▶' : i + 1}</span>
                <span className="queue-row-title">{t.title}</span>
                <span className="mono queue-row-sub">{t.artist ?? ''}</span>
              </button>
            ))}
          </div>
        </div>
      )}
      <div className="queue-bar">
        <button
          className="tbtn"
          title="Previous"
          onClick={() => advance(-1)}
          disabled={index === 0}
        >
          <Icon name="prev" size={15} />
        </button>
        <button className="queue-play" title={isPaused ? 'Play' : 'Pause'} onClick={togglePause}>
          <Icon name={isPaused ? 'play' : 'pause'} size={14} />
        </button>
        <button className="tbtn" title="Next" onClick={() => advance(1)}>
          <Icon name="next" size={15} />
        </button>
        <span className="queue-now">
          <span className="queue-now-title">{track?.title ?? ''}</span>
          <span className="mono queue-now-sub">
            {[track?.artist, track?.parent_title].filter(Boolean).join(' · ')}
          </span>
        </span>
        <span className="queue-progress">
          <span className="queue-progress-fill" style={{ width: `${pct}%` }} />
        </span>
        {/* Music always plays direct (HUB-19): no pipeline, just the file. */}
        <span className="mono queue-how">{how ? `direct · ${how}` : 'direct'}</span>
        <button
          className={`tpill mono${queueOpen ? ' on' : ''}`}
          title="Queue"
          onClick={() => setQueueOpen((v) => !v)}
        >
          queue {entries.length}
        </button>
        <button className="tbtn queue-x" title="Stop and clear" onClick={onStop}>
          ✕
        </button>
        {([0, 1] as const).map((which) => {
          const slot = slots.current[which]
          return (
            slot.session && (
              <audio
                key={which}
                ref={els[which]}
                src={slot.session.stream_url}
                // Not while the video player has the screen. `prepare` swaps a
                // new `stream_url` into this same element, and per the media
                // load algorithm that resets the can-autoplay flag — so an
                // element the force-pause effect had silenced started playing
                // again on the next track, with neither `forcePaused` nor
                // `active` changing, so that effect never re-ran. Pressing Next
                // on the queue dock, which stays clickable over the player,
                // put music on top of the film.
                autoPlay={which === active && !forcePaused}
                preload="auto"
                hidden={which !== active}
                onPlay={() => {
                  if (which !== active) return
                  // Either slot can have died while the queue sat paused, and
                  // which one it was matters. One shared flag rebuilt the
                  // ACTIVE session whatever had actually gone, so a dead
                  // preload stayed dead and the handover at `ended` landed on
                  // a URL the hub had forgotten.
                  for (const w of [0, 1] as const) {
                    if (slots.current[w].trouble !== 'dead') continue
                    const audible = w === active
                    void recoverSlot(
                      w,
                      audible ? index : index + 1,
                      audible ? (els[w].current?.currentTime ?? 0) : 0,
                      false,
                    )
                  }
                }}
                onTimeUpdate={() => onTime(which)}
                onPlaying={() => setIsPaused(false)}
                onPause={() => setIsPaused(true)}
                onEnded={() => onEnded(which)}
                // A recovered session streams the same file from the top;
                // put the playhead back where the dead one left off.
                onLoadedMetadata={() => {
                  const want = resumeAt.current
                  if (which !== active || !want) return
                  // Only onto the track it was measured on.
                  if (slots.current[which].key !== want.key) return
                  const el = els[which].current
                  if (el) el.currentTime = want.at
                  resumeAt.current = null
                }}
                // The element reports a failure with no status of its own,
                // so ask the hub what kind it was — 404 means the session
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
                      els[active].current?.paused ?? false,
                    )
                  })
                }}
              />
            )
          )
        })}
      </div>
      {/* Under the bar, not in it: the bar is a row of controls, and an
          error wedged between them pushed the transport around and got
          truncated. */}
      {error && <div className="error queue-error">{error}</div>}
    </div>
  )
}
