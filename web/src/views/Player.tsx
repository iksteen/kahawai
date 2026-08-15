import { useEffect, useReducer, useRef, useState } from 'react'
import Hls from 'hls.js'
import { fetchItem } from '../item-query'
import {
  adminSessionLogUrl,
  fetchChildren,
  artworkUrl,
  accessToken,
  api,
  endSession,
  fetchLibraries,
  prefsOrNone,
  postProgress,
  refreshTokens,
  putPref,
  resolveTracks,
  seekSession,
  startPlaybackSession,
  subtitleLabel,
  subtitleFileUrl,
  type ItemDetail,
  type Session,
  isAdmin,
  downloadWithAuth,
} from '../api'
import { loadMask, maskSummary } from '../capabilities'
import { keepSessionAlive } from '../keepalive'
import { notify } from '../toast'
import {
  SESSION_GONE,
  forgetRecoveries,
  isSessionDead,
  isSessionGone,
  mayRecover,
  startRetry,
} from '../recovery'
import CapabilityDebug from './CapabilityDebug'
import Icon from '../icons'
import { isTypingTarget, playerIntent } from '../player-keys'
import PlayerNote from '../PlayerNote'
import { playerNote } from '../player-note'
import { playerPhase } from '../player-phase'

import { initialTracks, tracks } from '../player-tracks'
import { initialSubtitle, needsBurnRestart } from '../track-choice'
import { absoluteMs, nextPartSeekMs, nudgeTarget, planSeek, producedEndMs } from '../player-time'
import { initialHealth, isFrozen, sessionHealth, type SessionEvent } from '../player-session'
import { subtitleRoute } from '../subtitle-route'
import { useSubtitleRenderers } from '../use-subtitles'
import { seLabel } from '../label'
import { deliveryPlan } from '../delivery'

function fmt(ms: number) {
  const s = Math.max(0, Math.floor(ms / 1000))
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  const sec = s % 60
  return h > 0
    ? `${h}:${String(m).padStart(2, '0')}:${String(sec).padStart(2, '0')}`
    : `${m}:${String(sec).padStart(2, '0')}`
}

/// Seconds of lead-in before the next episode starts by itself. Long
/// enough to read what is coming and stop it, short enough that nobody
/// sits through it waiting.
const UP_NEXT_S = 9

/// How long controls stay visible after a fresh player mounts or the pointer
/// moves over it.
const CONTROLS_HIDE_MS = 2600

/// How long a restart gets to produce a frame before the veil comes down. Not
/// a guess at the hub's speed — a ceiling on how long a spinner is allowed to
/// be the whole story. Past it the picture is frozen and the play veil at
/// least offers something to press.
const RESTART_GIVEUP_MS = 25_000

/// How loud, across every session in this visit.
///
/// Module scope on purpose. `PlayerRoute` renders this component keyed on the
/// session id, so a restart, a stand-by resume, a capability change and every
/// next episode replace it — and component state started over at 1.0 and
/// UNMUTED, writing that into the fresh element rather than merely failing to
/// restore it. An evening of a series at 15% became an episode at 100% at every
/// boundary. The frame lifted `mode` out for exactly this reason; volume needs
/// no frame involvement, because there is only ever one pair of ears — the same
/// reason the music queue yields the screen to the player.
///
/// Not persisted across reloads; that would be a different decision, and a
/// stored volume that outlives a change of headphones has its own surprise.
let lastHeard = { volume: 1, muted: false }
/// How many times a fatal hls.js network error may be answered with
/// `startLoad()` before the viewer is asked instead. hls.js paces its own
/// retries but never stops asking, so unbounded this polls a hub that has gone
/// away for as long as the tab is open. Reset once a segment actually arrives,
/// so a long watch over a flaky link is not slowly used up.
const NET_RESTART_LIMIT = 5

/// How often to ask whether the host is back. Slow enough that an evening of
/// waiting is not a request storm, quick enough that you do not sit looking at a
/// dialog after the host has already returned.
const STANDBY_RETRY_MS = 5000

export default function Player({
  item,
  session,
  resumeMs,
  libraryId,
  mode,
  setMode,
  onClose,
  onRestart,
  onHome,
  onPlayNext,
}: {
  item: ItemDetail
  session: Session
  resumeMs: number
  libraryId: string
  /// How big the picture is. Owned by the frame around this component, which
  /// outlives it — a restart replaces the player, and the window it sits in
  /// must not blink while that happens.
  mode: 'window' | 'theater' | 'full'
  setMode: (m: 'window' | 'theater' | 'full') => void
  onClose: () => void
  /** Play again from `at` on a freshly negotiated session (capability
   *  debug: a mask only takes effect on a new session). */
  onRestart: (session: Session, at: number) => void
  /// The way out of a stand-by that never resolves.
  onHome: () => void
  /** Move on to another item on its own fresh session — the next episode,
   *  either because the countdown ran out or because it was asked for. */
  onPlayNext: (item: ItemDetail, session: Session) => void
}) {
  const videoRef = useRef<HTMLVideoElement>(null)
  // No capsRef any more: the subtitle list used to be a second call
  // that had to be told this client's bits, and could therefore
  // disagree with the session. It now arrives with the item, computed
  // server-side from the same profile the QUERY asked with.
  /// Which panel is open, if any. They occupy the same corner and only one can
  /// be up, which two booleans could not say.
  const [panel, setPanel] = useState<'none' | 'caps' | 'info'>('none')
  const showCaps = panel === 'caps'
  const infoOpen = panel === 'info'
  /// The health machine: playing, waiting for a host, restarting, finished.
  ///
  /// `healthRef` is the authority and `health` is its shadow for rendering,
  /// which is the opposite of the usual arrangement and deliberate. Listeners
  /// drive almost all of this, and a listener reads whatever the render that
  /// created it captured — so the current value has to live somewhere a stale
  /// closure cannot reach. `send` applies the transition to the ref at once and
  /// dispatches the same event for the picture: the recovery guard in
  /// particular is a mutex between two detectors that can fire in one tick, and
  /// a value that only lands next render would let both through.
  ///
  /// Nothing may call `dispatch` directly; the two would drift.
  /// Why the last start failed, quoted if the next one fails at the same point.
  /// Not part of the machine: nothing decides on it and nothing renders it, it
  /// only chooses the wording of one message.
  const lastFailure = useRef('')
  const [health, dispatch] = useReducer(sessionHealth, undefined, initialHealth)
  const healthRef = useRef(health)
  const send = (e: SessionEvent) => {
    healthRef.current = sessionHealth(healthRef.current, e)
    dispatch(e)
  }
  const { restarting, awaitingGen, gone, standby, capsError } = health
  const maskedRef = useRef(maskSummary(loadMask()))
  const hlsRef = useRef<Hls | null>(null)
  const isHls = session.stream_url.endsWith('.m3u8')
  // For HLS sessions the pipeline itself starts at resumeMs, so the
  // playlist's t=0 is that offset; direct sessions play the real file.
  const offsetRef = useRef(isHls ? resumeMs : 0)
  /// Whether the element's clock means anything yet.
  ///
  /// A direct session plays the real file, so its resume is applied by
  /// seeking the element once `loadedmetadata` arrives — until then
  /// `currentTime` is 0 and `offsetRef` is 0, and reporting that writes the
  /// beginning of the film over the position the viewer left at. Opening a
  /// film and pressing Back within the second was enough to lose it.
  ///
  /// An HLS session has nothing to wait for: the pipeline itself starts at
  /// `resumeMs`, so `offsetRef` is already the truth at mount.
  const positionKnown = useRef(isHls || resumeMs === 0)
  /// Where the viewer is, absolutely.
  ///
  /// Before `loadedmetadata` on a direct session the element's clock reads 0 and
  /// means nothing — the truth is still the resume point, and reporting 0 wrote
  /// the beginning of the film over it. So `positionKnown` picks the value here
  /// instead of gating the callers: gating suppressed the keepalive ping too,
  /// and that ping's 404 is the only automatic notice a direct session gets
  /// that its session has died.
  /// `v` defaults to the live ref, but the callers that outlive it pass the
  /// element they captured. React detaches a ref while deleting the tree, and
  /// the session effect's cleanup — the final progress report on the way out —
  /// runs after that: reading the ref there would take the `resumeMs` branch
  /// and post the position the viewer STARTED at, discarding the whole
  /// sitting. That did not reproduce when measured, which means it depends on
  /// commit ordering inside React rather than on anything here, and this is
  /// two characters cheaper than depending on it.
  const absMs = (v = videoRef.current) =>
    absoluteMs({
      known: positionKnown.current && !!v,
      offsetMs: offsetRef.current,
      currentTimeS: v?.currentTime ?? 0,
      resumeMs,
    })
  // Multi-part sources: the pipeline's start.pos is local to its part;
  // the absolute timeline origin is partBase + start.pos.
  const partBaseRef = useRef(session.part_base_ms ?? 0)
  const durationMs = session.duration_ms ?? 0
  /// What the element is doing, as the transport needs to draw it: where the
  /// film is, how much the pipeline has written, and the element's own
  /// reporting on itself. One slot because one set of listeners writes all of
  /// it — `timeupdate` sets the first two together, `play`/`pause` and
  /// `volumechange` the rest — and the bar reads them together.
  ///
  /// Read from events rather than tracked in parallel: the element pauses for
  /// reasons of its own, and a button drawn from a guess disagrees with it.
  const [playing, setPlaying] = useState({
    posMs: offsetRef.current,
    producedMs: 0,
    paused: false,
    ...lastHeard,
  })
  const { posMs, producedMs, paused, volume, muted } = playing
  const setPosMs = (ms: number) => setPlaying((s) => ({ ...s, posMs: ms }))
  /// The pipeline restart whose PICTURE has not arrived yet; 0 = none.
  ///
  /// A boolean could not say WHICH restart it was waiting for, and that was the
  /// whole defect: a superseded run's `playing` cleared a newer run's veil, and
  /// a newer run's `pause()` aborted an older run's `play()` whose catch
  const settle = (gen: number) => send({ type: 'restart-settled', gen })
  // The element's own state, mirrored so the custom transport can draw it.
  // Read from events rather than tracked in parallel: the video pauses for
  // reasons of its own — a stall, the end of a part — and a button that
  // disagreed with the picture would be worse than no button.
  /// How much of the timeline the hub has actually produced. Beyond it a
  /// seek restarts the pipeline, which is slow and worth showing.
  /// window: in the page column. theater: the full width of the window.
  /// full: the browser's own fullscreen, which also carries the subtitle
  /// canvases because it takes the whole stage rather than the <video>.
  // `mode` lives with the frame — see PlayerRoute. The player still decides it
  // (keys, the two buttons, leaving fullscreen), but the element it dresses is
  // outside this component, so that this component can be replaced without the
  // frame going with it.
  /// The overlay hides while the picture is playing and nobody is moving
  /// the pointer — the whole point of the screen is what is behind it.
  const [barShown, setBarShown] = useState(true)
  const barTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)
  /// The ceiling on a restart, held so it can be taken down again. It outlives
  /// this component otherwise — see `beginRestart`.
  const giveUpTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)
  /// What is being played and with which tracks — see player-tracks.ts. One
  /// machine because they change together: a switch moves a selection, the
  /// verdict and the epoch in one go.
  const [trk, sendTrack] = useReducer(tracks, session.streams, initialTracks)
  const { subs, subKey, audioList: audioTracks, audio: audioTrack } = trk
  const { videoList: videoTracks, video: videoTrack, epoch: trackEpoch, streams, vttFallback } = trk
  // `session.mode` describes pipeline ownership/container shape. The plan cost
  // says what happened to the elementary streams and changes with track picks.
  const delivery = deliveryPlan(streams?.cost ?? session.mode)
  // HUB-33: memory scope for manual track choices (series id, or the
  // item itself for movies).
  const seriesRef = useRef<string>(item.id)
  // Ordered subtitle-language wishlist; [] → subtitles stay off.
  const subsPrefRef = useRef<string[]>([])
  // Exact-track memory for THIS item (subtitle unification).
  const subTrackPrefRef = useRef<number | null>(null)
  /// The tracks a restart should ask for, readable from a closure that was
  /// made before they were resolved.
  ///
  /// HUB-33 picks the audio track asynchronously after mount, but the session
  /// effect below is keyed `[session.session_id]` and captures the FIRST
  /// render's `recover` — where both of these are still 0. So an automatic
  /// restart after a 404 asked for track 0 whatever you were listening to:
  /// an anime set to Japanese came back in English, with the selector still
  /// claiming Japanese because the remounted player re-resolves it. Same
  /// pattern as `switchBurnRef` above, for the same reason.
  // The <track> URL must shift cues when the HLS timeline starts mid-file;
  // bump on seek-restarts so the track reloads with the new shift.
  // Live stream verdicts: a track switch re-plans server-side and the
  // overlay must describe what plays NOW.

  /// What the restart paths need: which tracks were chosen, and what the hub
  /// says it is actually serving. Read from listeners and from async paths that
  /// resumed after a render, so it is a ref rather than the state itself.
  /// A remembered burn that could not be applied yet, because the viewer was
  /// already steering. Applied when they stop — see the effect below.
  const pendingBurn = useRef<number | null>(null)
  const chosenRef = useRef({ audio: 0, video: 0, streams: session.streams })
  chosenRef.current = { audio: audioTrack, video: videoTrack, streams }
  // Track initialisation finishes asynchronously. Keep its burn switch
  // pointed at the current render without making the fetch effect restart
  // whenever the seek machinery is recreated.
  const switchBurnRef = useRef<(trackId: number) => Promise<void>>(async () => {})
  /// What kind of library this is, for HUB-33. Resolved with the track list
  /// below; kept because the auto-advance has to resolve the NEXT episode's
  /// tracks and cannot ask the library again mid-transition.
  const mediaTypeRef = useRef('')

  useEffect(() => {
    // Fenced, because everything this reports goes through `playerNote`, whose
    // single module-level listener belongs to whichever player is mounted when
    // it fires. A blip that 404s the session also fails these three fetches;
    // recovery then remounts the player on a new session, and the late
    // rejection painted "Could not load the track list" over video that was
    // playing perfectly — the same shape as the give-up timer speaking after a
    // recovery that worked.
    let dead = false
    // One resolution (HUB-33), the same helper the route used to start the
    // session: prefs + streams → selector state and subtitle default.
    // Inner fallbacks, so one dead half does not cost the others — but each
    // says so now. Both used to be silent, which meant the outer report below
    // could not fire for either: prefs down came up on the wrong audio track
    // with the selector naming a different one, and libraries down skipped the
    // per-media-type language wishlist. `showNote`, not `notify`, because this
    // can happen with the picture fullscreen.
    Promise.all([
      fetchItem(item.id),
      prefsOrNone(playerNote),
      fetchLibraries().catch((e: unknown) => {
        playerNote(`Could not load the library details: ${e}`)
        return { libraries: [] }
      }),
    ])
      .then(([d, p, l]) => {
        if (dead) return null
        seriesRef.current = d.parent_id ?? item.id
        const mediaType = l.libraries.find((x) => x.id === libraryId)?.media_type ?? ''
        mediaTypeRef.current = mediaType
        const audio = d.sources_detail[0]?.streams?.audio ?? []
        sendTrack({
          type: 'lists-arrived',
          audioList: audio,
          videoList: d.sources_detail[0]?.streams?.video ?? [],
        })
        const r = resolveTracks(
          p.prefs,
          seriesRef.current,
          item.id,
          mediaType,
          d.metadata?.original_language,
          audio,
        )
        sendTrack({ type: 'audio-known', audio: r.audioTrack })
        subsPrefRef.current = r.subs
        subTrackPrefRef.current = r.subTrack
        // The full list arrived with the item: QUERY answered "what
        // would I be served", and delivery is already computed against
        // this client's capability bits. Nothing is filtered out.
        return { subtitles: d.negotiated?.subtitles ?? [] }
      })
      .then((r) => {
        if (dead || !r) return
        sendTrack({ type: 'subtitles-arrived', subs: r.subtitles })
        const pick = initialSubtitle({
          subs: r.subtitles,
          exactId: subTrackPrefRef.current,
          wishlist: subsPrefRef.current,
        })
        if (pick) {
          // Never overrides a choice already made.
          sendTrack({ type: 'subtitle-chosen', key: String(pick.id), onlyIfUnset: true })
          // Deferred, not dropped, while the pipeline is being steered. This is
          // the one `beginRestart` caller that is not a button — the seekbar
          // gates on `isFrozen`, the selectors on `disabled={frozen}` — so a
          // burn re-applied a few hundred milliseconds into playback could take
          // the generation from a seek the viewer had just made and restart at
          // `absMs()` before that seek had written `offsetRef`: the drag went
          // silently. Skipping it instead is just as silent the other way, and
          // worse in one respect: the selector above has already named the
          // track, so the picture would carry no subtitles under a selector
          // saying it does, with nothing to re-trigger it.
          if (needsBurnRestart(pick, chosenRef.current.streams ?? undefined)) {
            if (isFrozen(healthRef.current)) pendingBurn.current = pick.id
            else void switchBurnRef.current(pick.id)
          }
        }
      })
      .catch((e: unknown) => {
        // NOT emptied. The picture is playing — `Detail` already negotiated
        // this session — so a blip on the player's own track fetch says
        // nothing about what the file contains. Blanking them removed the
        // audio and subtitle selectors entirely, and the viewer's reading of
        // that is "this file has no subtitles", which is false and offers
        // nothing to press.
        // `showNote` like the two halves above: this effect re-runs on every
        // auto-advance, which happens with the picture fullscreen, and the
        // toast host is a sibling of the element that goes fullscreen.
        if (dead) return
        playerNote(`Could not load the track list: ${e}`)
      })
    return () => {
      dead = true
    }
  }, [item.id, libraryId])

  // A capability mask reaches the hub only on a NEW session — the hub
  // stores the effective profile per session and re-plans track
  // switches against it — so applying one restarts playback here.
  const restartWithCaps = async () => {
    send({ type: 'caps-restart-started' })
    try {
      const at = Math.round(absMs())
      const fresh = await startPlaybackSession(
        item,
        at,
        chosenRef.current.audio,
        chosenRef.current.video,
      )
      if (goneAway.current) return void endSession(fresh.session_id, true)
      onRestart(fresh, at) // App swaps the route's session and releases the old one
    } catch (e) {
      send({ type: 'caps-restart-failed', why: String(e) })
    }
  }

  /// The hub no longer has this session — reaped for idleness, lost to a
  /// restart, ended elsewhere. Start a fresh one where we are and hand it
  /// up: onRestart remounts this component (keyed on session id) and App
  /// releases the one it replaced, so this is the capability-restart path
  /// with a different trigger.
  ///
  /// Driven only by a 404 from the hub. Nothing here knows or guesses how
  /// long a session is allowed to idle — see recovery.ts.

  /// True once the player is gone. A restart that lands after that produced a
  /// session with nobody to play, ping or end it.
  const goneAway = useRef(false)
  useEffect(() => {
    // Reset on setup, not just on teardown: StrictMode mounts twice, and a
    // ref left true from the first pass would reject every real restart.
    goneAway.current = false
    return () => {
      goneAway.current = true
    }
  }, [])

  /// Something failed but playback is still a going concern — a seek the hub
  /// refused, a track switch that did not take.
  ///
  /// Not `notify`: the toast host is a sibling of `.videobox` in the shell,
  /// and `.videobox` is what goes fullscreen. While it is, the browser paints
  /// only its subtree, so a toast is not rendered at all — the failure was
  /// reported to nobody in precisely the mode where the picture fills the
  /// screen and the freeze is most alarming.

  /// Which pipeline restart owns the timeline.
  ///
  /// seekTo, switchTracks and switchBurn all POST a seek and then write
  /// offsetRef, partBaseRef and posMs AFTER the await, so two in flight meant
  /// the last RESPONSE won rather than the last request. The transport is
  /// disabled during a track switch but not during a seek — the seekbar stays
  /// clickable and the arrow keys are always live — so a nudge answering
  /// after a scrub left the clock, the seekbar and every subtitle path (all
  /// of which shift by -offsetRef) reading a position the pipeline was not
  /// producing.
  const seekGen = useRef(0)

  /// What the player is doing, ranked — see player-phase.ts. One value, so an
  /// overlay cannot forget a state it has to yield to.
  const phase = playerPhase({ standby, gone, restarting: awaitingGen !== 0, paused })
  /// A dialog owns the screen; nothing behind it may be pressed.
  const blocked = phase === 'standby' || phase === 'gone'
  /// The pipeline is not the viewer's to steer right now.
  const frozen = blocked || phase === 'restarting'

  /// The last thing that actually went wrong. A second recovery at the same

  /// How many times hls.js has been told to start loading again after a fatal
  /// network error, reset whenever a segment actually arrives.
  const netRestarts = useRef(0)

  /// Freeze the old run and take the timeline. Returns the generation to check
  /// after the await and to hand to `attach`.
  const beginRestart = () => {
    const mine = ++seekGen.current
    send({ type: 'timeline-taken', gen: mine })
    hlsRef.current?.stopLoad() // the restart 404s the old run's segments
    videoRef.current?.pause()
    // Armed HERE, not after the POST returns. The veil goes up and the
    // transport freezes before `seekSession` is even sent, and `api` has no
    // timeout — so a hub that accepts the connection and then wedges left the
    // spinner up for ever with every control dead, which is the exact case
    // this ceiling exists for. Arming it in `attach` covered only the window
    // after the answer, where hls.js is already a second net.
    //
    // `mine` is non-zero by construction here, so the gen-0 special case is
    // gone — both bugs in the first draft of this timer came from it.
    // Held, because the generation check does not cover the case this player
    // is REPLACED. A 404 or a 503 recovers by starting a new session, and the
    // route remounts this component on it — deliberately, and with
    // `awaitingGen` still set, since those paths keep the veil up rather than
    // settling. The old closure's `healthRef` freezes that way, so the check
    // passes 25 seconds later and `giveUp` posts "The stream did not come
    // back" through `playerNote`, whose single listener now belongs to the
    // NEW player: a stream-died message over running video, from a recovery
    // that worked.
    clearTimeout(giveUpTimer.current)
    giveUpTimer.current = setTimeout(() => {
      if (healthRef.current.awaitingGen !== mine) return
      giveUp('The stream did not come back. Press play to try again.', mine)
    }, RESTART_GIVEUP_MS)
    return mine
  }

  /// A restart is not coming: stop pretending one is, and make the play button
  /// mean something again.
  ///
  /// `beginRestart` stops the loader and pauses, so every path that abandons a
  /// restart has to undo that or the picture is simply stuck. `dead` is what
  /// `onPlay` reads to decide whether pressing play should rebuild the session
  /// rather than just un-pause a dead element.
  /// `gen` is the restart this is giving up on. Checked, because `settle`, the
  /// call this replaced, was: late means superseded means no-op. Unchecked, an
  /// older POST answering "no" pauses the picture and marks the player dead
  /// while a NEWER restart is still genuinely coming — two taps of Right past
  /// the produced edge is enough, since the keydown handler holds a closure
  /// whose `frozen` is stale for as long as the element is not advancing.
  const giveUp = (why: string, gen = healthRef.current.awaitingGen) => {
    if (healthRef.current.awaitingGen !== gen) return
    videoRef.current?.pause()
    send({ type: 'gave-up', gen })
    playerNote(why)
  }

  /// The restart itself, without the guards. Shared by the automatic path
  /// below and the viewer pressing Try again, which is not a loop and must
  /// not be treated as one.
  /// `quiet` rides through to the session start: `recover` is automatic and
  /// its failures belong to the dialog already on screen, where Try again is
  /// the viewer asking for something and should answer.
  const restartAt = async (at: number, quiet = false) => {
    try {
      const fresh = await startPlaybackSession(
        item,
        at,
        chosenRef.current.audio,
        chosenRef.current.video,
        undefined,
        quiet,
        playerNote,
      )
      if (goneAway.current) {
        void endSession(fresh.session_id, true)
        return false
      }
      onRestart(fresh, at)
      return true
    } catch (e) {
      lastFailure.current = String(e)
      // An unreachable hub is not an answer, it is the weather the stand-by
      // dialog exists for — and the retry loop below already reads it that way.
      // This branch turning it into "Playback stopped" was the disagreement.
      //
      // `busy` — the account's stream cap — deliberately does NOT come here,
      // even though it clears by itself. Standing by would tell somebody the
      // machine holding their file has stopped answering, which is false, and
      // offer them only Go home for a condition they can fix by closing
      // something. The stopped path shows the hub's own sentence instead.
      if (startRetry(e) === 'wait') {
        // Stop the picture. There is still a buffer, and left alone it plays
        // on behind the dialog — sound coming out of a screen that says the
        // file is unreachable, and a timeline running past the point anyone
        // actually watched to.
        videoRef.current?.pause()
        // So the resume position is where it STOPPED, not where it was when
        // the failed start left: a round trip's worth of buffer plays out in
        // between, and resuming at the older mark would replay it.
        send({ type: 'host-away', atMs: Math.round(absMs()) })
      } else {
        videoRef.current?.pause()
        send({ type: 'stopped', why: String(e) })
      }
      return false
    }
  }

  /// Asked for, rather than triggered. Clears the loop guard and ignores the
  /// paused check: both exist to stop the player restarting itself, and
  /// neither should stand in the way of somebody who pressed a button.
  const retryByHand = () => {
    send({ type: 'retry-by-hand' })
    forgetRecoveries()
    const at = Math.round(absMs())
    void restartAt(at)
  }

  /// `ourPause`: the caller paused the element itself for a restart, so the
  /// check below must not read that as the viewer having stopped watching. That
  /// bail is why an automatic part transition ended the film silently — the
  /// 404 handler called this, it no-op'd, and the early return skipped the note
  /// underneath, so nothing was said and nothing was offered.
  const recover = async (ourPause = false) => {
    if (!ourPause && videoRef.current?.paused) {
      send({ type: 'died-while-paused' })
      return
    }
    if (healthRef.current.recovering) return
    send({ type: 'recovery-started' })
    const at = Math.round(absMs())
    // Two restarts at the same position mean the first never played.
    if (!mayRecover(item.id, at, performance.now())) {
      videoRef.current?.pause()
      send({
        type: 'stopped',
        why: lastFailure.current
          ? // The hub's messages start lower case, so a full stop before one
            // reads like a typo. A dash joins them without pretending it is
            // a sentence.
            `It restarted once and stopped again at the same point — ${lastFailure.current}`
          : 'It restarted once and stopped again at the same point.',
      })
      send({ type: 'recovery-ended' })
      return
    }
    await restartAt(at, true)
    send({ type: 'recovery-ended' })
  }

  // Ask again until it works. Deliberately NOT through `mayRecover`: that
  // guard exists to stop a session respawning at a position it never played,
  // and this is the opposite case — we know exactly why it failed and we are
  // waiting for that to stop being true. The interval is a client's own
  // choice, not a mirror of anything the hub knows.
  useEffect(() => {
    if (standby === null) return
    let stop = false
    // One at a time. A start may take up to a minute — that ceiling is there
    // because the hub can legitimately be slow coming up — while this fires
    // every five seconds, so twelve could be outstanding at once. Each holds a
    // per-user admission slot against a cap of four, and the overflow is
    // refused "too many concurrent streams": not `SourceOffline`, not
    // `Offline`, so the branch below took it for a real failure and replaced
    // the wait, and the position it was holding, with "Playback stopped". The
    // loop talked itself out of standing by. Measured before this guard: five
    // concurrent, one every five seconds regardless.
    let inFlight = false
    const tick = async () => {
      if (inFlight) return
      inFlight = true
      try {
        const fresh = await startPlaybackSession(
          item,
          standby,
          chosenRef.current.audio,
          chosenRef.current.video,
          undefined,
          // Every five seconds for as long as the host is away.
          true,
        )
        // A session started after the player left is a session nobody will
        // ever play, ping or end. Hand it back rather than leaving a
        // transcoder slot to the reaper.
        if (stop) void endSession(fresh.session_id, true)
        else onRestart(fresh, standby)
      } catch (e) {
        // Still away: keep waiting. Anything else is a real failure and the
        // stand-by was the wrong answer to it — except an unreachable hub,
        // which is not an answer at all. `api` throws Offline for any
        // fetch-level failure, and an unhealthy network is exactly the
        // weather this dialog exists for; one DNS hiccup used to replace the
        // wait, and the position it was holding, with "Playback stopped".
        //
        // `=== 'wait'`, the same test the entry branch makes, and deliberately
        // NOT `retryable`. Both were tried.
        //
        // `busy` clears by itself, so retrying it looks right — but this
        // dialog says one specific thing ("the machine holding this file has
        // stopped answering") and offers one button (Go home). A viewer whose
        // host came back and who is now merely at the stream cap — another
        // tab, or the album player's two slots — would sit in front of a false
        // cause for ever, with no way out and nothing to press. Leaving takes
        // them to the stopped screen and the hub's own sentence, which names
        // the thing that clears it: close one first.
        //
        // The position is not what is lost by leaving. Progress is reported to
        // the hub as it plays, so coming back resumes; a permanent dialog
        // about the wrong machine is not recoverable at all.
        if (!stop && startRetry(e) !== 'wait') {
          send({ type: 'stopped', why: String(e) })
        }
      } finally {
        inFlight = false
      }
    }
    const t = setInterval(() => void tick(), STANDBY_RETRY_MS)
    return () => {
      stop = true
      clearInterval(t)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [standby])

  // Track switching is a seek-restart at the current position with the
  // new track (§6 machinery; ~2 s hiccup, same as a deep seek).
  const switchTracks = async (audio: number, video_: number) => {
    const was = { audio: audioTrack, video: videoTrack }
    sendTrack({ type: 'tracks-chosen', audio: audio, video: video_ })
    const mine = beginRestart()
    let owned = false
    try {
      const at = absMs()
      const r = await seekSession(session.session_id, at, audio, video_)
      // `goneAway` as well as the generation: `seekGen` is a ref on THIS
      // instance, so after an unmount it still matches and the code below
      // runs `attach` against a detached element — a second `hls.destroy()`,
      // a manifest fetch for the session the cleanup just ended, and a fatal
      // attach error whose handler starts a replacement session nobody
      // collects. The three restart paths that already check this get it
      // right; these three did not.
      if (mine !== seekGen.current || goneAway.current) return
      if (r.streams) sendTrack({ type: 'streams-known', streams: r.streams })
      partBaseRef.current = r.part_base_ms ?? 0
      offsetRef.current = Math.round(at)
      setPosMs(offsetRef.current)
      sendTrack({ type: 'run-moved' })
      attach(mine)
      owned = true
      // Remembered only now it is playing. Written before the switch, a
      // failed one still steers every later episode of the series towards
      // a track this one could not manage.
      //
      // Two additive layers (HUB-33). The SERIES remembers the language
      // (portable across episodes with differing track orders) — a
      // language-motivated switch keeps steering the whole series. MOVIES
      // additionally pin the exact track index: "the commentary track of
      // THIS film" has no language representation, and there is no series
      // intent to follow. Episodes deliberately do NOT pin, so one episode
      // never freezes on an old choice.
      if (audio !== was.audio) {
        const value = audioTracks[audio]?.language?.toLowerCase() ?? `#${audio}`
        void putPref(seriesRef.current, 'audio', value).catch(() => {})
        if (item.kind === 'movie') {
          void putPref(item.id, 'audio.track', `#${audio}`).catch(() => {})
        }
      }
    } catch (e) {
      // The selector must not name a track that is not playing — unless the
      // answer is "wait", where the pick is still what they asked for and the
      // stand-by resume carries it. Reverting first meant a mediahost blip
      // during a track switch silently put the old track back.
      if (startRetry(e) !== 'wait') {
        sendTrack({ type: 'tracks-chosen', audio: was.audio, video: was.video })
        // And the ref, in the same breath. It is assigned during RENDER, while
        // `recover` below runs in this microtask — so a 404 answered the
        // selector by snapping back to the old track and then opened the
        // recovery session on the new one, leaving Japanese audio playing under
        // a selector reading English. Which is the disagreement the revert is
        // here to prevent.
        chosenRef.current = { ...chosenRef.current, audio: was.audio, video: was.video }
      }
      // The session is gone: `recover` owns the outcome from here — a new
      // session and a remount, or the `gone` dialog. Keep the veil up, because
      // the element is paused on purpose and a play button over it would lie.
      if (isSessionDead(e)) {
        owned = true
        return void recover(true)
      }
      // 503, no answer, and a hub that failed are all a WAIT — see `startRetry`.
      // Whoever asked. The hub answers starts and seeks through
      // the same refusal, and a lease re-opened by a seek now reports a
      // disconnected host as `SourceOffline` — so a host vanishing mid-film and
      // noticed by a nudge or a track switch used to skip stand-by entirely for
      // the one condition stand-by exists for.
      if (startRetry(e) === 'wait') {
        owned = true
        send({ type: 'host-away', atMs: absMs() })
        return
      }
      // The same class of answer as a refused seek, and the same consequence:
      // `beginRestart` has already called `hls.stopLoad()` and paused the
      // element, nothing on this path starts the loader again, and the `settle`
      // below disarms the give-up ceiling by moving the generation on. So the
      // buffer played out and the picture froze for good, with the keepalive
      // holding the session alive so the 404 `recover` waits for never came.
      // Only leaving the player recovered it. `giveUp` hands the retry back
      // now, because the answer is already in and it was no.
      giveUp(`Could not switch track: ${e}`, mine)
    } finally {
      // Only a restart that never got going clears here. The POST returning
      // means the run has been ASKED for, not that there is a picture.
      if (!owned) settle(mine)
    }
  }

  // Burn transitions reuse the seek-restart machinery (§6): the
  // pipeline restarts at the current position with the new burn state
  // (id > 0 burns that track, 0 withdraws an explicit burn).
  const switchBurn = async (trackId: number) => {
    const video = videoRef.current
    if (!video) return
    const mine = beginRestart()
    let owned = false
    try {
      const at = absMs()
      const r = await seekSession(session.session_id, at, undefined, undefined, trackId)
      // `goneAway` as well as the generation: `seekGen` is a ref on THIS
      // instance, so after an unmount it still matches and the code below
      // runs `attach` against a detached element — a second `hls.destroy()`,
      // a manifest fetch for the session the cleanup just ended, and a fatal
      // attach error whose handler starts a replacement session nobody
      // collects. The three restart paths that already check this get it
      // right; these three did not.
      if (mine !== seekGen.current || goneAway.current) return
      if (r.streams) sendTrack({ type: 'streams-known', streams: r.streams })
      partBaseRef.current = r.part_base_ms ?? 0
      offsetRef.current = Math.round(at)
      setPosMs(offsetRef.current)
      sendTrack({ type: 'run-moved' })
      attach(mine)
      owned = true
    } catch (e) {
      // The session is gone: `recover` owns the outcome from here — a new
      // session and a remount, or the `gone` dialog. Keep the veil up, because
      // the element is paused on purpose and a play button over it would lie.
      if (isSessionDead(e)) {
        owned = true
        return void recover(true)
      }
      // 503, no answer, and a hub that failed are all a WAIT — see `startRetry`.
      // Whoever asked. The hub answers starts and seeks through
      // the same refusal, and a lease re-opened by a seek now reports a
      // disconnected host as `SourceOffline` — so a host vanishing mid-film and
      // noticed by a nudge or a track switch used to skip stand-by entirely for
      // the one condition stand-by exists for.
      if (startRetry(e) === 'wait') {
        owned = true
        send({ type: 'host-away', atMs: absMs() })
        return
      }
      // The same class of answer as a refused seek, and the same consequence:
      // `beginRestart` has already called `hls.stopLoad()` and paused the
      // element, nothing on this path starts the loader again, and the `settle`
      // below disarms the give-up ceiling by moving the generation on. So the
      // buffer played out and the picture froze for good, with the keepalive
      // holding the session alive so the 404 `recover` waits for never came.
      // Only leaving the player recovered it. `giveUp` hands the retry back
      // now, because the answer is already in and it was no.
      giveUp(`Could not change subtitles: ${e}`, mine)
    } finally {
      // Only a restart that never got going clears here. The POST returning
      // means the run has been ASKED for, not that there is a picture.
      if (!owned) settle(mine)
    }
  }
  switchBurnRef.current = switchBurn

  const selected = subs.find((s) => String(s.id) === subKey)
  // Set when the live tap yields nothing, which sends the same track down the
  // flattened .vtt path instead.
  const route = subtitleRoute(selected, { isHls, vttFallback })

  const subtitles = useSubtitleRenderers({
    videoRef,
    route,
    selected,
    subKey,
    trackEpoch,
    offsetRef,
    item,
    session,
    isHls,
    onTapEmpty: () => sendTrack({ type: 'tap-empty' }),
  })

  // Offset starts snap to the keyframe before the requested position;
  // the pipeline reports the true playlist origin in start.pos. Adopt
  // it so subtitle cues and the seekbar line up exactly.
  /// Guarded like every other post-await writer of `offsetRef` — it is the
  /// fourth, and was the only one without it. The retry loop below sleeps 700ms
  /// between attempts precisely because `start.pos` is often not written yet,
  /// so it routinely outlives the seek that asked for it: a second seek landing
  /// first was then overwritten by the first seek's origin, leaving the clock,
  /// the seekbar, the ASS offset and every later in-run seek adrift by the
  /// difference, with nothing to correct it.
  const syncOrigin = async (gen: number) => {
    if (offsetRef.current === 0) return
    const base = session.stream_url.replace(/[^/]*$/, '')
    for (let attempt = 0; attempt < 3; attempt++) {
      try {
        const r = await api(`${base}start.pos`)
        if (gen !== seekGen.current) return
        if (r.ok) {
          const local = Math.round(Number(await r.text()))
          if (gen !== seekGen.current) return
          const n = partBaseRef.current + local
          if (
            Number.isFinite(n) &&
            n !== offsetRef.current &&
            Math.abs(n - offsetRef.current) < 60000
          ) {
            const video = videoRef.current
            offsetRef.current = n
            subtitles.nudgeOffset(n)
            if (video) setPosMs(n + video.currentTime * 1000)
            sendTrack({ type: 'run-moved' })
          }
          return
        }
      } catch {
        /* retry */
      }
      await new Promise((res) => setTimeout(res, 700))
    }
  }

  /// `gen` is the restart whose `playing` this attach listens for. The mount
  /// call takes the default: 0 owns no veil, so `settle(0)` is a no-op.
  /// Literally 0 rather than `seekGen.current`, so a future bare `attach()`
  /// after a restart cannot silently adopt a live generation.
  const attach = (gen = 0) => {
    const video = videoRef.current!
    hlsRef.current?.destroy()
    if (isHls && Hls.isSupported()) {
      const hls = new Hls({
        // Media requests carry the Bearer token; the cookie is the
        // fallback for engines we don't drive ourselves.
        xhrSetup: (xhr) => {
          const t = accessToken()
          if (t) xhr.setRequestHeader('Authorization', `Bearer ${t}`)
        },
        // Our EVENT playlists are growing recordings, not live TV: the
        // pipeline paces itself a window ahead of THIS player, so the
        // default live-edge sync creates a feedback loop — hls.js chases
        // the edge, the edge moves with it, playback lives at the
        // starved frontier and buffers on every segment. Watch from the
        // beginning and never chase.
        startPosition: 0,
        liveSyncDurationCount: 1e6,
        liveMaxLatencyDurationCount: Infinity,
        maxBufferLength: 60,
      })
      // A dead session 404s every segment and playlist refresh. hls.js
      // hands us the status; without this it retries internally and then
      // stalls with nothing to explain it.
      // A segment arrived, so whatever the link was doing, it is doing it
      // again — the restart budget below is per outage, not per session.
      hls.on(Hls.Events.FRAG_BUFFERED, () => {
        netRestarts.current = 0
      })
      hls.on(Hls.Events.ERROR, (_e, data) => {
        const code = data.response?.code
        if (code === SESSION_GONE) {
          void recover()
          return
        }
        // hls.js fetches with its own XHR, so it never gets api()'s
        // refresh-and-retry. Without this an expired token stops
        // playback dead — and worse, hides a 404 behind a 401, because
        // auth runs before the handler that would have said GONE.
        if (code === 401) {
          void refreshTokens().then((ok) => ok && hls.startLoad())
          return
        }
        // Everything else used to stop here. A fatal error is precisely why a
        // restarted session produces nothing, and it is the only account of
        // it anyone gets — hls.js reports once and moves on.
        if (!data.fatal) return
        lastFailure.current = `${data.type}: ${data.details}${code ? ` (HTTP ${code})` : ''}`
        // Recording it was all this did, and hls.js does not restart itself
        // after a fatal error — so nothing fetched another segment, ever. The
        // picture froze without pausing, so no veil appeared; the ping kept
        // succeeding, so the session was never reaped and never answered 404;
        // and 404 was the only thing that called `recover`. A viewer whose
        // wifi dropped for twenty seconds sat looking at a still frame with
        // nothing on screen to say so.
        //
        // hls.js publishes the two it can be asked to retry. Anything else is
        // ours to restart.
        if (data.type === Hls.ErrorTypes.NETWORK_ERROR) {
          // Bounded, like every other self-restart here. Unbounded, a tab left
          // open against a hub that went away polls it for as long as it is
          // open. Past the budget, hand it to the viewer the way a failed
          // restart does rather than going quiet.
          // Past the budget, but only once there is nothing left to play. A
          // fatal network error does NOT stop the picture — hls.js stops its own
          // loader and the element plays the buffer out, which is why a wifi
          // drop of twenty seconds used to heal in silence. hls.js also goes
          // fatal every three or four seconds while a hub is unreachable, so a
          // flat count of five was spent inside the buffer and paused a video
          // with forty seconds in hand. While there is picture, keep asking.
          const v = videoRef.current
          const ahead = v
            ? (() => {
                for (let i = 0; i < v.buffered.length; i++) {
                  if (v.buffered.start(i) <= v.currentTime && v.currentTime <= v.buffered.end(i)) {
                    return v.buffered.end(i) - v.currentTime
                  }
                }
                return 0
              })()
            : 0
          if (netRestarts.current >= NET_RESTART_LIMIT && ahead < 2) {
            giveUp('The stream stopped and did not come back. Press play to try again.')
            return
          }
          netRestarts.current += 1
          hls.startLoad()
        } else if (data.type === Hls.ErrorTypes.MEDIA_ERROR) {
          hls.recoverMediaError()
        } else {
          void recover()
        }
      })
      hls.loadSource(session.stream_url)
      hls.attachMedia(video)
      hlsRef.current = hls
    } else {
      video.src = session.stream_url // cookie-authenticated
    }
    // The restart is over when there is a PICTURE, not when the hub answered —
    // and it is over for THIS generation only. Registered here so the
    // generation is in the closure: a persistent listener has nothing to
    // compare against, which is how a superseded run's `playing` cleared a
    // newer run's veil.
    video.addEventListener('playing', () => settle(gen), { once: true })
    // A refused autoplay is not an error to swallow: it is the whole reason
    // the viewer has to click. It also fires NO `pause` event — the element
    // never left paused, so there is no transition to report — which is why
    // the state has to be set here rather than waited for.
    //
    // An AbortError is the opposite case: a NEWER restart's pause() or load
    // interrupted this play(). That run owns the veil, and the element is not
    // paused by the VIEWER — reporting either is how a second seek dropped a
    // play button on top of a running restart.
    void video.play().catch((e: unknown) => {
      // Either way the element did not start, and the load algorithm sets
      // `paused` without firing an event, so it has to be recorded by hand or
      // nothing on screen reflects it. Only the SETTLE differs: on an abort a
      // newer restart owns the veil, and clearing it would pull that run's
      // spinner out from under it.
      setPlaying((e) => ({ ...e, paused: true }))
      if ((e as DOMException)?.name !== 'AbortError') settle(gen)
    })
    // Nothing else brings the veil down if the new run never produces a frame
    // and play() resolved anyway — hls.js reports fatals, a direct file can
    // just sit. Generation-keyed, so late means superseded means no-op: no
    // timer handle, no cleanup, nothing to get wrong on three paths.
    //
    // Dropping the veil is not enough on its own. `play()` resolves even with
    // no data, so the element reports itself as playing and the phase falls to
    // `playing`: no veil, no play button, no message, and a transport reading
    // Pause over a picture that has stopped. Measured, not assumed. So the
    // element is paused — which brings the play veil back, giving something to
    // press — and the reason is said out loud.
    // `gen` is 0 for the mount attach, which owns no veil — and 0 is also the
    // "nothing outstanding" value, so an unguarded timer fired 25 s into a
    // healthy session and paused it with a message about a stream that had not
    // gone anywhere. Only a real restart gets a ceiling.
    // Only a real restart gets a ceiling. `gen` is 0 for the mount attach, which
    // owns no veil — and 0 is also the "nothing outstanding" value, so an
    // unguarded timer fired 25 s into a healthy session and paused it with a
    // message about a stream that had not gone anywhere. Guarding the timer
    // rather than returning, because `syncOrigin` below runs for every attach.
    void syncOrigin(gen)
  }

  // Seek anywhere on the full timeline: inside the produced range it is
  // a plain element seek; beyond it the hub restarts the pipeline at the
  // target (§6) and we re-attach to the same URL.
  const seekTo = async (targetMs: number) => {
    if (isFrozen(healthRef.current)) return
    const video = videoRef.current!
    const plan = planSeek({
      targetMs,
      offsetMs: offsetRef.current,
      producedEndS: video.seekable.length > 0 ? video.seekable.end(video.seekable.length - 1) : 0,
      isHls,
    })
    // A direct file is the whole film, and a target already produced is a jump
    // the element can make on its own. Only the third answer costs a pipeline.
    if (plan.kind !== 'restart') {
      video.currentTime = plan.toS
      return
    }
    // The restart replaces the run server-side: every not-yet-fetched segment
    // of the OLD run is about to 404, so the old loader stops and the picture
    // freezes — the wait is visible instead of the player playing on while
    // spraying 404s.
    const mine = beginRestart()
    let owned = false
    try {
      const r = await seekSession(session.session_id, targetMs)
      // Superseded while this was out: the newer one owns the timeline, and
      // the hub coalesces the restarts anyway.
      // `goneAway` as well as the generation: `seekGen` is a ref on THIS
      // instance, so after an unmount it still matches and the code below
      // runs `attach` against a detached element — a second `hls.destroy()`,
      // a manifest fetch for the session the cleanup just ended, and a fatal
      // attach error whose handler starts a replacement session nobody
      // collects. The three restart paths that already check this get it
      // right; these three did not.
      if (mine !== seekGen.current || goneAway.current) return
      partBaseRef.current = r.part_base_ms ?? 0
      offsetRef.current = Math.round(targetMs)
      setPosMs(targetMs)
      sendTrack({ type: 'run-moved' })
      attach(mine)
      owned = true
    } catch (e) {
      // A 404 here is not a message, it is the recovery contract: the
      // session is gone and the answer is a new one at this position.
      // Toasting it left the picture stopped by `stopLoad`/`pause` above,
      // and the ping's own 404 then reached `recover`, which bails on a
      // paused element — so an automatic part transition simply ended the
      // film.
      // The session is gone: `recover` owns the outcome from here — a new
      // session and a remount, or the `gone` dialog. Keep the veil up, because
      // the element is paused on purpose and a play button over it would lie.
      if (isSessionDead(e)) {
        owned = true
        return void recover(true)
      }
      // 503, no answer, and a hub that failed are all a WAIT — see `startRetry`.
      // Whoever asked. The hub answers starts and seeks through
      // the same refusal, and a lease re-opened by a seek now reports a
      // disconnected host as `SourceOffline` — so a host vanishing mid-film and
      // noticed by a nudge or a track switch used to skip stand-by entirely for
      // the one condition stand-by exists for.
      if (startRetry(e) === 'wait') {
        owned = true
        // Where they asked to be, not where they were. The seekbar has already
        // moved there, and resuming at the old playhead would drop the seek
        // without saying so.
        send({ type: 'host-away', atMs: Math.round(targetMs) })
        return
      }
      // The picture is already stopped and the old run's segments are gone,
      // so a silent failure here is a player that just froze — and it did.
      // `beginRestart` calls `hls.stopLoad()`, and NOTHING starts it again on
      // this path: the give-up ceiling is the only thing that sets `dead`,
      // which is the only thing that makes pressing play retry, and the
      // `settle` below disarms the ceiling by moving the generation on. So the
      // buffer played out and the picture stalled for good, with the keepalive
      // holding the session alive so the 404 that `recover` waits for never
      // came. Hand the retry back HERE instead of in 25 seconds' time: the
      // answer is already in, and it was no.
      giveUp(`Could not seek: ${e}`, mine)
    } finally {
      // Only a restart that never got going clears here. The POST returning
      // means the run has been ASKED for, not that there is a picture.
      if (!owned) settle(mine)
    }
  }

  useEffect(() => {
    const video = videoRef.current!
    attach()

    const seekToResume = () => {
      if (!isHls && resumeMs > 0) video.currentTime = resumeMs / 1000
      positionKnown.current = true
    }
    video.addEventListener('loadedmetadata', seekToResume)

    // Produced end, in absolute time: for HLS this is what the pipeline
    // has written; for a direct file it is the whole thing.
    const producedEnd = () =>
      producedEndMs({
        offsetMs: offsetRef.current,
        seekableEndS:
          video.seekable.length > 0 ? video.seekable.end(video.seekable.length - 1) : null,
      })
    const onTime = () => {
      setPlaying((s) => ({ ...s, posMs: absMs(), producedMs: producedEnd() }))
    }
    video.addEventListener('timeupdate', onTime)
    video.addEventListener('progress', onTime)
    // `canplay` as well as the transitions: `paused` starts as a guess, and
    // on the path where autoplay is refused there is no transition to
    // correct it. Syncing once there is media makes the state agree with the
    // element whatever the reason, rather than only on the reasons foreseen.
    const syncPaused = () => setPlaying((e) => ({ ...e, paused: video.paused }))
    video.addEventListener('play', syncPaused)
    video.addEventListener('pause', syncPaused)
    video.addEventListener('canplay', syncPaused)
    video.addEventListener('volumechange', () => {
      lastHeard = { volume: video.volume, muted: video.muted }
      setPlaying((e) => ({ ...e, volume: video.volume, muted: video.muted }))
    })

    const report = (keepalive = false) => postProgress(session.session_id, absMs(video), keepalive)
    // Pings while paused too, bounded — see keepalive.ts. Guarding
    // this on `!video.paused` is what let the reaper delete a paused
    // viewer's segment directory out from under them.
    //
    // The ping doubles as the earliest death detector: it runs every
    // 10 s, so a session lost to ANY cause answers 404 here, usually
    // before the picture stalls.
    const stopPinging = keepSessionAlive(
      () => absMs(video),
      (ms) => {
        void postProgress(session.session_id, ms).then((r) => {
          if (isSessionGone(r)) void recover()
        })
      },
    )
    const onPause = () => report()
    // The gesture that makes recovery worth doing. Proactive rather than
    // waiting for the load to fail, so there is no error flash first.
    const onPlay = () => {
      if (!healthRef.current.dead) return
      send({ type: 'play-pressed' })
      void recover()
    }
    video.addEventListener('play', onPlay)
    // Direct play has no hls.js to report a status: the element just
    // fails. Ask the hub which kind of failure it was — a 404 is a dead
    // session, anything else is a real media fault and stays one.
    const onError = () => {
      void postProgress(session.session_id, absMs(video)).then((r) => {
        if (isSessionGone(r)) void recover()
      })
    }
    video.addEventListener('error', onError)
    const onEnded = () => {
      report()
      // Multi-part sources (CD1/CD2): this part's playlist ended but
      // the film hasn't — restart into the next part.
      const nextPart = nextPartSeekMs({
        absMs: absMs(video),
        durationMs,
        parts: session.parts ?? 1,
        isHls,
      })
      if (nextPart !== null) void seekTo(nextPart)
    }
    video.addEventListener('pause', onPause)
    video.addEventListener('ended', onEnded)
    // Where the viewer got to, on the way out. Releasing the session is not
    // this component's call — App owns that, because App owns the route the
    // session belongs to — and this listener runs alongside the one there.
    const onUnload = () => report(true)
    window.addEventListener('beforeunload', onUnload)

    return () => {
      stopPinging()
      video.removeEventListener('loadedmetadata', seekToResume)
      video.removeEventListener('timeupdate', onTime)
      video.removeEventListener('progress', onTime)
      video.removeEventListener('play', syncPaused)
      video.removeEventListener('pause', syncPaused)
      video.removeEventListener('canplay', syncPaused)
      video.removeEventListener('pause', onPause)
      video.removeEventListener('play', onPlay)
      video.removeEventListener('error', onError)
      video.removeEventListener('ended', onEnded)
      window.removeEventListener('beforeunload', onUnload)
      // Report, do not release: every line above this one undoes something
      // this effect did, and can therefore be done again. Ending the session
      // could not — it was the one irreversible act in a teardown that React
      // is entitled to run whenever it wants, and it is App's now.
      report(true)
      hlsRef.current?.destroy()
    }
    // Everything this effect reads is fixed for the session: durationMs
    // and isHls are derived from the `session` prop, parts/resumeMs are
    // props, and attach/seekTo close over only those plus refs. A new
    // session REMOUNTS this component (App renders it keyed on
    // session_id), so none of it can go stale here. Listing them would
    // re-run the cleanup — a progress report and a destroyed hls instance —
    // on every unrelated render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.session_id])

  // The element is the authority on volume; this only pushes changes into
  // it, and `volumechange` above brings them back.
  useEffect(() => {
    const v = videoRef.current
    if (!v) return
    v.volume = volume
    v.muted = muted
  }, [volume, muted])

  const stage = () => videoRef.current?.parentElement ?? null

  /// Keep the subtitles clear of the control bar.
  ///
  /// Three renderers, two levers. The JASSUB and image-subtitle canvases
  /// are siblings of the <video>, so the stylesheet lifts them with a
  /// transform — JASSUB rewrites their top/left/width/height on every
  /// resize but never their transform, so it does not fight this.
  ///
  /// Native <track> cues live in the video's own shadow tree, where a
  /// transform on a sibling cannot reach them; they move by `line`, which
  /// counts from the bottom when negative. The browser does this by itself
  /// for its own controls, and has no idea ours exist.
  ///
  /// The cost of a transform is that it lifts everything, including a sign
  /// positioned at the top of the frame. Dialogue is the common case and
  /// the bar is up for two and a half seconds at a time.
  useEffect(() => {
    const v = videoRef.current
    if (!v) return
    const lift = () => {
      for (const track of Array.from(v.textTracks)) {
        for (const cue of Array.from(track.cues ?? [])) {
          ;(cue as VTTCue).line = barShown || paused ? -4 : 'auto'
        }
      }
    }
    lift()
    // Cues arrive as the track loads, and a fresh one carries no line.
    for (const track of Array.from(v.textTracks)) track.addEventListener('cuechange', lift)
    return () => {
      for (const track of Array.from(v.textTracks)) track.removeEventListener('cuechange', lift)
    }
  }, [barShown, paused, subKey, trackEpoch])

  // Fullscreen is the browser's, so it can be left without asking us —
  // Escape, or the window's own control. Follow it rather than assume.
  useEffect(() => {
    const onFs = () => {
      if (!document.fullscreenElement) setMode(mode === 'full' ? 'window' : mode)
    }
    document.addEventListener('fullscreenchange', onFs)
    return () => document.removeEventListener('fullscreenchange', onFs)
  }, [mode, setMode])

  useEffect(() => {
    if (mode === 'full' && !document.fullscreenElement) void stage()?.requestFullscreen()
    if (mode !== 'full' && document.fullscreenElement) void document.exitFullscreen()
  }, [mode])

  const togglePause = () => {
    const v = videoRef.current
    // Guarded here rather than at the six callers: the keyboard, the transport
    // button, the ±10/30 buttons, the seekbar and the video's own onClick all
    // funnel through this and `seekTo`. A dialog blocks the pointer with a
    // scrim and never blocked the keyboard, so Space played the buffered tail
    // behind "the file is unreachable" — the sound `restartAt` pauses to stop.
    //
    // Through the ref, so a stale keydown closure cannot carry an old answer.
    if (isFrozen(healthRef.current) || !v) return
    if (v.paused) void v.play()
    else v.pause()
  }

  /// `durationMs` is 0 when the hub has no probed duration for the source, and
  /// clamping to it turned every nudge — both buttons and both arrow keys —
  /// into a jump to the start of the film. Only clamp when there is something
  /// to clamp to; the seekbar and up-next already gate on the same thing.
  const nudgeTime = (bySec: number) => void seekTo(nudgeTarget({ posMs, bySec, durationMs }))

  /// Reveal the overlay and start its countdown again. Kept visible while
  /// paused: a paused picture with no controls looks broken.
  const wake = () => {
    setBarShown(true)
    clearTimeout(barTimer.current)
    barTimer.current = setTimeout(() => setBarShown(false), CONTROLS_HIDE_MS)
  }
  useEffect(() => {
    // A recovered session remounts the player without moving the pointer. Arm
    // the same countdown on mount or its fresh controls stay up indefinitely.
    barTimer.current = setTimeout(() => setBarShown(false), CONTROLS_HIDE_MS)
    return () => {
      clearTimeout(barTimer.current)
      clearTimeout(giveUpTimer.current)
    }
  }, [])

  // The deferred burn, once the pipeline is the viewer's to steer again.
  useEffect(() => {
    if (frozen || pendingBurn.current === null) return
    const id = pendingBurn.current
    pendingBurn.current = null
    void switchBurnRef.current(id)
  }, [frozen])

  // Keys, because a player without them is a player you have to aim at. What
  // each one means is in `player-keys.ts` and under test; this is the half that
  // cannot be: the listener, and doing the thing.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const asked = playerIntent(e.key, {
        typing: isTypingTarget((e.target as HTMLElement | null)?.tagName),
        mode,
      })
      if (!asked) return
      if (asked.preventDefault) e.preventDefault()
      const { intent } = asked
      if (intent.kind === 'toggle-pause') togglePause()
      else if (intent.kind === 'nudge') nudgeTime(intent.seconds)
      else setMode(intent.to)
      wake()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode, posMs, durationMs])

  const pct = durationMs > 0 ? Math.min(100, (posMs / durationMs) * 100) : 0
  /// The episode after this one, fetched in full while there is time, so
  /// pressing Play now does not wait on a round trip.
  /// What follows this, and whether it is on its way. One slot: the overlay
  /// reads all three together and no two of them move independently.
  const [upNext, setUpNext] = useState<{
    next: ItemDetail | null
    dismissed: boolean
    starting: boolean
  }>({ next: null, dismissed: false, starting: false })
  const { next, dismissed: upNextOff, starting: startingNext } = upNext

  useEffect(() => {
    if (item.kind !== 'episode' || !item.parent_id) return
    let live = true
    fetchChildren(item.parent_id)
      .then(async (c) => {
        const at = c.children.findIndex((e) => e.id === item.id)
        const after = at >= 0 ? c.children[at + 1] : undefined
        if (!after || !live) return
        const full = await fetchItem(after.id)
        if (live) setUpNext((u) => ({ ...u, next: full }))
      })
      .catch(() => {})
    return () => {
      live = false
    }
  }, [item.id, item.kind, item.parent_id])

  const remainMs = durationMs > 0 ? durationMs - posMs : 0
  const upNextIn = Math.max(0, Math.ceil(remainMs / 1000))
  const upNextOn = !!next && !upNextOff && durationMs > 0 && remainMs <= UP_NEXT_S * 1000

  const playNext = async () => {
    if (!next || startingNext) return
    setUpNext((u) => ({ ...u, starting: true }))
    try {
      // Resolved for the next episode, not carried over from this one.
      //
      // Passing this episode's track NUMBER was wrong for the reason
      // switchTracks documents: the hub takes it as a raw index and clamps it
      // into range, and mux order is not stable across a series. Watching
      // [jpn, eng] on index 1 and advancing into an episode muxed [eng, jpn]
      // put on Japanese while the selector said English.
      //
      // Prefs are re-read rather than cached: a track switch during this
      // episode wrote the series' language a moment ago, and that is exactly
      // the choice the next episode should follow.
      const p = await prefsOrNone(playerNote)
      const r = resolveTracks(
        p.prefs,
        seriesRef.current,
        next.id,
        mediaTypeRef.current,
        next.metadata?.original_language,
        next.sources_detail[0]?.streams?.audio ?? [],
      )
      const s = await startPlaybackSession(next, 0, r.audioTrack, 0, p.prefs)
      // Back was pressed while the hub was answering. Handing this up would
      // navigate them into the next episode against the thing they just did,
      // replacing the history entry they made.
      if (goneAway.current) return void endSession(s.session_id, true)
      onPlayNext(next, s)
    } catch (e) {
      playerNote(`Could not start the next episode: ${e}`)
      setUpNext((u) => ({ ...u, starting: false }))
    }
  }

  // Advance on `ended`, not on the arithmetic reaching zero.
  //
  // `timeupdate` stops firing when playback finishes, so the position
  // settles up to a second short of the duration and a `remainMs <= 0`
  // test never becomes true — the countdown sat at 1 over a finished
  // episode. The element knows it ended; ask it.
  //
  // The countdown itself still runs off the position, so pausing pauses
  // it: a timer of its own would start the next episode over a picture
  // that had stopped.
  useEffect(() => {
    const v = videoRef.current
    if (!v || !next || upNextOff) return
    const onDone = () => {
      // A multi-part source (CD1/CD2) also ends here, and that is not the
      // end of the film — the effect above seeks into the next part. Only
      // advance when this really is the last of it.
      if (durationMs > 0 && durationMs - (offsetRef.current + v.currentTime * 1000) > 3000) return
      void playNext()
    }
    v.addEventListener('ended', onDone)
    return () => v.removeEventListener('ended', onDone)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [next, upNextOff, startingNext, durationMs])

  const producedPct = durationMs > 0 ? Math.min(100, (producedMs / durationMs) * 100) : 0
  // QUERY carries the geometry of the exact source negotiation chose. Shape
  // the box before the first media byte; `auto` still lets intrinsic metadata
  // take over once the browser has decoded it. Old/unprobed rows keep 16:9.
  const display = item.negotiated?.source
  const preplayRatio =
    display?.display_width && display.display_height
      ? `${display.display_width} / ${display.display_height}`
      : '16 / 9'

  return (
    <>
      <div
        className={`videobox${barShown || paused ? ' bar-up' : ''}`}
        data-ratio={preplayRatio}
        style={{ '--video-ratio': preplayRatio } as React.CSSProperties}
        onMouseMove={wake}
        onMouseLeave={() => setBarShown(false)}
      >
        <video ref={videoRef} playsInline crossOrigin="use-credentials" onClick={togglePause}>
          {/* Requesting a text form of an image track left a <track> load
              pending forever, which kept Firefox's own buffering overlay
              latched over a playing video. */}
          {route === 'vtt-track' && (
            <track
              key={`${subKey}-${trackEpoch}`}
              default
              kind="subtitles"
              src={subtitleFileUrl(item.id, `${subKey}.vtt`, -Math.round(offsetRef.current))}
            />
          )}
        </video>
        {/* Chrome will not start a video for a viewer who has not interacted
            with the page, so a reloaded player sits on its first frame
            waiting for a click it never asked for. The transport bar does
            say `paused`, in a 12-pixel glyph at the bottom of the screen;
            this says it in the middle, where the eye already is. Clicking
            the picture always worked — this is only the part that admits it.

            Shown only in the `paused` phase: standing by, stopped and
            restarting all outrank it, and each of those pauses the element
            itself — see player-phase.ts. */}
        {phase === 'paused' && (
          <button className="play-veil" onClick={togglePause} aria-label="Play">
            <Icon name="play" size={30} />
          </button>
        )}
        {/* The image-subtitle and JASSUB canvases are inserted after the
            <video> by their effects and positioned against it, so
            everything below is a later sibling and paints over them. */}
        {phase === 'restarting' && (
          <div className="seek-veil" aria-label="Restarting stream">
            <span className="seek-veil-spin">&#10227;</span>
          </div>
        )}
        {infoOpen && (
          <div className="info-overlay mono">
            <span>
              <span className="dim">session </span>
              {item.title} · <span className={delivery.tone}>{delivery.chip}</span> ·{' '}
              {session.content_type}
            </span>
            {streams && (
              <>
                <span>
                  <span className="dim">video </span>
                  {streams.video}
                </span>
                <span>
                  <span className="dim">audio </span>
                  {streams.audio}
                </span>
              </>
            )}
            {streams?.subtitles?.map((t) => (
              <span key={t.index}>
                <span className="dim">
                  {[t.language, t.format].filter(Boolean).join(' ') || 'subs'} ·{' '}
                </span>
                {t.tier}
                {t.note ? <span className="dim"> — {t.note}</span> : null}
              </span>
            ))}
            {/* The mask this session was negotiated with, always visible
                while the panel is open: a forgotten mask must never read
                as a bug in the hub. */}
            {maskedRef.current.length > 0 && (
              <span className="sand">masked {maskedRef.current.join(' ')}</span>
            )}
            {/* OPS-10, where the problem is visible: the diagnostics for
                THIS session, beside the verdict that describes it. */}
            {isAdmin() && (
              <button
                className="info-log"
                onClick={() =>
                  downloadWithAuth(adminSessionLogUrl(session.session_id)).catch((e: unknown) =>
                    notify(`Could not download the session log: ${e}`),
                  )
                }
              >
                download session log
              </button>
            )}
          </div>
        )}
        {upNextOn && next && (
          <div className={`up-next${barShown || paused ? ' lifted' : ''}`}>
            <div className="up-next-head">
              <span className="up-next-label mono">next episode</span>
              <span className="up-next-ring">
                {/* The ring empties as the count runs down; the number is
                    there because a ring alone does not say how long. */}
                <svg width="26" height="26" viewBox="0 0 36 36">
                  <circle cx="18" cy="18" r="16" fill="none" stroke="var(--line)" strokeWidth="3" />
                  <circle
                    cx="18"
                    cy="18"
                    r="16"
                    fill="none"
                    stroke="var(--teal)"
                    strokeWidth="3"
                    strokeLinecap="round"
                    strokeDasharray="100.5"
                    strokeDashoffset={100.5 * (1 - upNextIn / UP_NEXT_S)}
                  />
                </svg>
                <span className="mono">{upNextIn}</span>
              </span>
            </div>
            <button className="up-next-item" onClick={() => void playNext()}>
              <span className="up-next-thumb card-artbox">
                <img
                  src={artworkUrl(next.id, next.art_version, 'thumb')}
                  alt=""
                  onError={(e) => e.currentTarget.classList.add('art-failed')}
                />
              </span>
              <span className="up-next-text">
                <span className="mono dim">
                  {seLabel(next.season, next.episode, next.episode_end)}
                </span>
                <span className="up-next-title">{next.title}</span>
              </span>
            </button>
            <div className="up-next-acts">
              <button className="btn small" disabled={startingNext} onClick={() => void playNext()}>
                {startingNext ? 'Starting…' : 'Play now'}
              </button>
              <button
                className="btn ghost small leaving"
                onClick={() => setUpNext((u) => ({ ...u, dismissed: true }))}
              >
                Stop
              </button>
            </div>
          </div>
        )}
        <div className={`player-bar${barShown || paused ? '' : ' away'}`}>
          {durationMs > 0 && (
            <div
              className={`seekbar${frozen ? ' busy' : ''}`}
              title="Seek anywhere — beyond the produced range the hub restarts the pipeline at the target"
              onClick={(e) => {
                const r = e.currentTarget.getBoundingClientRect()
                void seekTo(((e.clientX - r.left) / r.width) * durationMs)
              }}
            >
              {/* What the hub has already produced. A seek inside it is
                  instant; past the dashed edge the pipeline restarts. */}
              {isHls && <div className="seekbar-made" style={{ width: `${producedPct}%` }} />}
              <div className="seekbar-fill" style={{ width: `${pct}%` }} />
            </div>
          )}
          <div className="transport">
            {/* `.player-bar` is z-6 and the seek veil is z-5, so during a
                restart this bar sits ABOVE the veil — a Play glyph here was
                the same offer the play veil was hidden to withdraw, two
                centimetres lower. */}
            <button
              className="tbtn"
              title={paused ? 'Play' : 'Pause'}
              disabled={frozen}
              onClick={togglePause}
            >
              <Icon name={paused ? 'play' : 'pause'} size={18} />
            </button>
            <button
              className="tbtn mono"
              title="Back 10 s"
              disabled={frozen}
              onClick={() => nudgeTime(-10)}
            >
              <Icon name="back10" size={14} />
              10
            </button>
            <button
              className="tbtn mono"
              title="Forward 30 s"
              disabled={frozen}
              onClick={() => nudgeTime(30)}
            >
              <Icon name="fwd30" size={14} />
              30
            </button>
            <span className="mono clock">
              {fmt(posMs)} / {fmt(durationMs)}
            </span>
            {phase === 'restarting' && (
              <span className="mono restarting">restarting pipeline at target…</span>
            )}
            <span className="vol">
              <button
                className="tbtn"
                title={muted ? 'Unmute' : 'Mute'}
                onClick={() => setPlaying((e) => ({ ...e, muted: !e.muted }))}
              >
                <Icon
                  name={muted || volume === 0 ? 'volumeOff' : volume < 0.5 ? 'volumeLow' : 'volume'}
                  size={14}
                />
              </button>
              <input
                type="range"
                min={0}
                max={100}
                value={Math.round((muted ? 0 : volume) * 100)}
                title="Volume"
                onChange={(e) => {
                  setPlaying((el) => ({
                    ...el,
                    volume: Number(e.target.value) / 100,
                    muted: false,
                  }))
                }}
              />
            </span>
            <span className="transport-right">
              {isHls && videoTracks.length > 1 && (
                <select
                  className="tsel mono"
                  title="Video track"
                  value={videoTrack}
                  disabled={frozen}
                  onChange={(e) => void switchTracks(audioTrack, Number(e.target.value))}
                >
                  {videoTracks.map((v, i) => (
                    <option key={i} value={i}>
                      {v.codec} {v.width}×{v.height}
                    </option>
                  ))}
                </select>
              )}
              {isHls && audioTracks.length > 1 && (
                <select
                  className="tsel mono"
                  title="Audio track"
                  value={audioTrack}
                  disabled={frozen}
                  onChange={(e) => void switchTracks(Number(e.target.value), videoTrack)}
                >
                  {audioTracks.map((a, i) => (
                    <option key={i} value={i}>
                      {a.language ?? '?'} · {a.codec} {a.channels}ch
                    </option>
                  ))}
                </select>
              )}
              {subs.length > 0 && (
                <select
                  className="tsel mono"
                  title="Subtitles"
                  // Like the audio and video selects above. This was the one
                  // control left live through a restart, and the only one that
                  // can START one: picking a burn track mid-seek bumps the
                  // generation again, so the seek already in flight bails and
                  // the hub runs two pipeline restarts for one intent. Under a
                  // stand-by dialog the mouse is blocked but the select is
                  // still focusable, so tabbing to it POSTed a seek on a
                  // session the hub had already lost.
                  disabled={frozen}
                  value={subKey}
                  onChange={(e) => {
                    const key = e.target.value
                    const prev = subs.find((x) => String(x.id) === subKey)
                    const s = subs.find((x) => String(x.id) === key)
                    sendTrack({ type: 'subtitle-chosen', key })
                    // Two memory layers (HUB-33): the series remembers the
                    // language; THIS item remembers the exact row — the only
                    // spelling that can name a downloaded/OCR track.
                    const value = key === '' ? 'off' : (s?.language ?? 'any').toLowerCase()
                    void putPref(seriesRef.current, 'subs', value).catch(() => {})
                    void putPref(item.id, 'subs.track', key).catch(() => {})
                    // Burn transitions live server-side: a track whose
                    // delivery IS burn restarts the pipeline with it; leaving
                    // one withdraws it (0 = clear). The tier comes from the
                    // ass_fallback preference, never from this list — picking
                    // says which subtitles, not how they are delivered.
                    if (s?.delivery === 'burn') void switchBurn(s.id)
                    else if (prev?.delivery === 'burn') void switchBurn(0)
                  }}
                >
                  <option value="">Subtitles off</option>
                  {subs.map((s) => (
                    <option key={s.id} value={String(s.id)} disabled={s.delivery === 'none'}>
                      {subtitleLabel(s)}
                    </option>
                  ))}
                </select>
              )}
              <button
                className={`tpill mono${infoOpen ? ' on' : ''}`}
                title="Playback info — why is this (not) transcoding"
                onClick={() => setPanel((p) => (p === 'info' ? 'none' : 'info'))}
              >
                info
              </button>
              {/* `.caps-under` is a panel below the picture, not an overlay,
                  so it cannot move inside `.videobox` — an `overflow: hidden`
                  box would clip its form. In fullscreen it is therefore
                  unreachable, and a button that does nothing is worse than no
                  button. Theater is fine: the videobox is 100vw and the panel
                  renders under it in the page. */}
              {mode !== 'full' && (
                <button
                  className={`tpill mono${showCaps ? ' on' : ''}`}
                  title="Client capabilities — mask one off and restart to see the other branch"
                  onClick={() => setPanel((p) => (p === 'caps' ? 'none' : 'caps'))}
                >
                  caps
                </button>
              )}
              {maskedRef.current.length > 0 && (
                <span className="caps-badge mono" title="This session was negotiated with a mask">
                  masked
                </span>
              )}
              <button
                className={`tbtn${mode === 'theater' ? ' on' : ''}`}
                title="Theater (t)"
                onClick={() => setMode(mode === 'theater' ? 'window' : 'theater')}
              >
                <Icon name={mode === 'theater' ? 'window' : 'theater'} size={15} />
              </button>
              <button
                className="tbtn"
                title="Fullscreen (f)"
                onClick={() => setMode(mode === 'full' ? 'window' : 'full')}
              >
                <Icon name={mode === 'full' ? 'shrink' : 'expand'} size={15} />
              </button>
            </span>
          </div>
        </div>
        <PlayerNote hidden={blocked} />
        {phase === 'gone' && (
          <div
            className="standby"
            role="alertdialog"
            aria-modal="true"
            aria-labelledby="player-stopped"
          >
            <div className="standby-box">
              <h2 id="player-stopped">Playback stopped</h2>
              <p className="dim">{gone}</p>
              <span className="failed-do">
                <button className="btn" onClick={retryByHand}>
                  Try again
                </button>
                <button className="btn ghost" onClick={onClose}>
                  Back to the item
                </button>
              </span>
            </div>
          </div>
        )}
        {/* The `standby !== null` is the type narrowing, not the condition —
            `phase` already decided. `standby` holds a resume position, so 0 is
            a real value and it cannot be tested for truthiness. */}
        {/* No `aria-live` on the dialog below: the role announces it already. */}
        {phase === 'standby' && standby !== null && (
          <div
            className="standby"
            role="alertdialog"
            aria-modal="true"
            aria-labelledby="player-standby"
          >
            <div className="standby-box">
              <h2 id="player-standby">Temporarily unavailable</h2>
              <p className="dim">
                The machine holding this file has stopped answering. Nothing is lost — playback will
                pick up where you left off as soon as it is back.
              </p>
              <p className="dim mono standby-at">standing by · resumes at {fmt(standby)}</p>
              {/* One way out on purpose. Any other button here would be a
                  second thing to reason about while waiting for a thing you
                  cannot influence. */}
              <button className="btn" onClick={onHome}>
                Go home
              </button>
            </div>
          </div>
        )}
      </div>
      {showCaps && (
        <div className="caps-under">
          <CapabilityDebug onApply={() => restartWithCaps()} applying={restarting} />
          {capsError && <div className="dim mono">restart failed: {capsError}</div>}
        </div>
      )}
      {/* Recovery is silent when it works; this is only the case where
          it could not, so the picture never just stops without a word. */}
      {/* Same shape as standing by, because it is the same situation from the
          viewer's side: the picture stopped and they did not do it. The
          difference is that this one is not going to fix itself, so it asks
          rather than waits. */}
    </>
  )
}
