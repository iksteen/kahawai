<script lang="ts">
/// How loud, across every session in this visit.
///
/// In a PLAIN script block, which is the only module scope an SFC has: the body
/// of `<script setup>` is the setup function, so a `let` there starts over with
/// every instance. The route renders this component keyed on the session id, so
/// a restart, a stand-by resume, a capability change and every next episode
/// replace it — and per-instance state started over at 1.0 and UNMUTED, writing
/// that into the fresh element rather than merely failing to restore it. An
/// evening of a series at 15% became an episode at 100% at every boundary.
///
/// Not persisted across reloads; a stored volume that outlives a change of
/// headphones has its own surprise.
export let lastHeard = { volume: 1, muted: false }

export function heard(volume: number, muted: boolean) {
  lastHeard = { volume, muted }
}
</script>

<script setup lang="ts">
/// One session, on screen: the element, the pipeline it is attached to, the
/// transport, and the four overlays that can own the picture.
///
/// Replaced whenever the SESSION is — the route keys it on the session id — so
/// everything here is per-run and nothing has to survive a restart. The frame
/// around it does not blink, which is why the window and the way out of it live
/// in the route rather than here.
import { computed, onBeforeUnmount, onMounted, reactive, ref, useTemplateRef, watch } from 'vue'
import Hls from 'hls.js'

import Art from './Art.vue'
import Btn from './Btn.vue'
import CapabilityDebug from './CapabilityDebug.vue'
import Icon from './Icon.vue'
import PlayerNote from './PlayerNote.vue'
import type { ItemQueryResponse } from '../api/generated/model/itemQueryResponse.ts'
import type { CarriedTracks } from '../domain/player-tracks.ts'
import type { PlayerMode } from '../domain/player-keys.ts'
import type { Preference } from '../api/generated/model/preference.ts'
import type { StartSessionResponse } from '../api/generated/model/startSessionResponse.ts'
import {
  absoluteMs,
  nextPartSeekMs,
  nudgeTarget,
  planSeek,
  producedEndMs,
} from '../domain/player-time.ts'
import { accessToken, refreshTokens, whoAmI } from '../api/session.ts'
import {
  adminSessionLog,
  endSession,
  getPrefs,
  itemChildren,
  itemQuery,
  postProgress,
} from '../api/generated/kahawai.ts'
import { buildProfile, loadMask } from '../api/capabilities.ts'
import { deliveryPlan } from '../domain/source.ts'
import { forgetRecoveries, isSessionGone, mayRecover, startCeiling } from '../domain/recovery.ts'
import {
  initialHealth,
  isFrozen,
  sessionHealth,
  type SessionEvent,
} from '../domain/player-session.ts'
import { initialSubtitle, needsBurnRestart } from '../domain/track-choice.ts'
import { chapterTicks } from '../domain/chapters.ts'
import { hms } from '../domain/label.ts'
import { skipLabel, skipTarget, skippable } from '../domain/segments.ts'
import { initialTracks, tracks as reduceTracks, type TrackEvent } from '../domain/player-tracks.ts'
import { isTypingTarget, playerIntent } from '../domain/player-keys.ts'
import { keepSessionAlive } from '../domain/keepalive.ts'
import { maskSummary } from '../domain/capability-mask.ts'
import { notify } from '../composables/notices.ts'
import { playerNote } from '../composables/player-note.ts'
import { putPref as writePref } from '../composables/prefs.ts'
import { playerPhase } from '../domain/player-phase.ts'
import { resolveTracks, subtitleLabel } from '../domain/tracks.ts'
import { saveAs } from '../api/download.ts'
import { seekSession, startPlaybackSession, subtitleFileUrl } from '../api/playback.ts'
import { seLabel } from '../domain/label.ts'
import { sentence } from '../domain/refusal.ts'
import { subtitleRoute } from '../domain/subtitle-route.ts'
import { useSubtitleRenderers } from '../composables/subtitles.ts'

const props = defineProps<{
  item: ItemQueryResponse
  session: StartSessionResponse
  resumeMs: number
  libraryId: string
  /// The viewer's preferences and this library's media type, as the page that
  /// started the session already read them. Passed rather than fetched: the
  /// same QUERY, prefs and library list used to arrive twice per playback —
  /// once to choose the audio track and start the session, once here to draw
  /// the selectors with — and the second set said the same thing as the first.
  prefs: Preference[]
  mediaType: string
  /// The track choice live at the moment of the restart that produced this
  /// mount, if this mount IS a restart. Outranks the prefs snapshot: the
  /// viewer's mid-episode pick is newer than anything the page fetched at
  /// session start.
  carried?: CarriedTracks | null
  /// How big the picture is. Owned by the frame around this component, which
  /// outlives it: a restart replaces the player, and the window it sits in must
  /// not blink while that happens.
  mode: PlayerMode
}>()

const emit = defineEmits<{
  mode: [PlayerMode]
  close: []
  home: []
  /// Play again from `at` on a freshly negotiated session, carrying the
  /// track choice that was live when the old one died.
  /// `from` names the item this picture was playing: the page may already be
  /// on another item by the time an async restart lands, and the handler must
  /// be able to tell a stale picture's session from its own.
  restart: [from: string, session: StartSessionResponse, at: number, carried: CarriedTracks]
  /// Another item on its own session — the next episode, whether the countdown
  /// ran out or it was asked for. Carries the preferences it resolved that
  /// episode's tracks from: they were read a moment ago, and the page holding
  /// them would otherwise hand the remounted player a staler set.
  playNext: [
    from: string,
    item: ItemQueryResponse,
    session: StartSessionResponse,
    prefs: Preference[],
  ]
}>()

/// Seconds of lead-in before the next episode starts by itself. Long enough to
/// read what is coming and stop it, short enough that nobody sits through it.
const UP_NEXT_S = 9
/// How long the controls stay up after a fresh mount or a pointer move.
const CONTROLS_HIDE_MS = 2600
/// How long a restart gets to produce a frame before the veil comes down. Not a
/// guess at the hub's speed — a ceiling on how long a spinner is allowed to be
/// the whole story.
const RESTART_GIVEUP_MS = 25_000
/// How many times a fatal hls.js network error may be answered with
/// `startLoad()` before the viewer is asked instead. hls.js paces its own
/// retries but never stops asking, so unbounded this polls a hub that has gone
/// away for as long as the tab is open.
const NET_RESTART_LIMIT = 5
/// The same, for a fatal MEDIA error. `recoverMediaError()` tears the
/// MediaSource down and builds a new one, so a stream hls.js cannot append at
/// all is a loop that re-attaches several times a second, for ever: no
/// picture, no message, and a control bar that rebuilds itself under the
/// pointer. Three is enough for the case the call is FOR — a decoder that
/// wedged once on a bad splice — and short enough that a stream which will
/// never append says so.
const MEDIA_RECOVER_LIMIT = 3
/// And the budget refills on TIME, not on a buffered segment.
///
/// The network budget refills on `FRAG_BUFFERED`, which is right for it: a
/// segment arriving means the link is back. It is wrong here. A stream whose
/// first segment can never be appended still buffers the ones after it, so
/// every failure was followed by a success that put the budget back — three
/// recoveries, refill, three more, for as long as the tab stayed open. What
/// distinguishes a decoder that wedged once from one that cannot play this
/// stream at all is not whether anything buffered, it is whether the failures
/// keep coming.
const MEDIA_RECOVER_WINDOW_MS = 30_000
/// How often to ask whether the host is back.
const STANDBY_RETRY_MS = 5000

const video = useTemplateRef<HTMLVideoElement>('video')
const box = useTemplateRef<HTMLElement>('box')

/// The health machine. One ref, and listeners read `.value` — the React
/// original needed a ref shadowing a reducer because a listener sees whatever
/// the render that created it captured, and a Vue ref has no such problem.
const health = ref(initialHealth())
const send = (event: SessionEvent) => (health.value = sessionHealth(health.value, event))
const settle = (gen: number) => send({ type: 'restart-settled', gen })

/// What is being played and with which tracks — see `domain/player-tracks.ts`.
const trk = ref(initialTracks(props.session.streams))
const sendTrack = (event: TrackEvent) => (trk.value = reduceTracks(trk.value, event))

const masked = maskSummary(loadMask())
const isHls = computed(() => props.session.stream_url.endsWith('.m3u8'))
let hls: Hls | null = null

/// Where this run begins, absolutely. For an HLS session the pipeline itself
/// starts at `resumeMs`, so the playlist's t=0 IS that offset; a direct session
/// plays the real file from 0.
const offset = ref(isHls.value ? props.resumeMs : 0)
/// Multi-part sources: the pipeline's start.pos is local to its part, and the
/// absolute origin is partBase + start.pos.
let partBase = props.session.part_base_ms ?? 0
const durationMs = computed(() => props.session.duration_ms ?? 0)

/// Whether the element's clock means anything yet.
///
/// A direct session applies its resume by seeking once `loadedmetadata`
/// arrives — until then `currentTime` is 0 and `offset` is 0, and reporting
/// that writes the beginning of the film over the position the viewer left at.
/// Opening a film and pressing Back within the second was enough to lose it.
let positionKnown = isHls.value || props.resumeMs === 0

/// `element` defaults to the live ref, but the callers that outlive it pass the
/// one they captured: the teardown's final progress report runs after the ref
/// has been detached, and reading it there would post the position the viewer
/// STARTED at, discarding the whole sitting.
const absMs = (element: HTMLVideoElement | null = video.value) =>
  absoluteMs({
    known: positionKnown && !!element,
    offsetMs: offset.value,
    currentTimeS: element?.currentTime ?? 0,
    resumeMs: props.resumeMs,
  })

/// What the element is doing, as the transport needs to draw it. Read from
/// events rather than tracked in parallel: the element pauses for reasons of
/// its own, and a button drawn from a guess disagrees with it.
const playing = reactive({
  // The resume point, not `offset` (0 for direct play): until the first
  // timeupdate, `absMs` answers `resumeMs` for an unknown position, and
  // this initial value is the one reading that bypassed it — a direct-play
  // resume then sat at "0" long enough for the skip offer to render and
  // announce for a recap forty minutes behind the viewer.
  posMs: positionKnown ? offset.value : props.resumeMs,
  producedMs: 0,
  paused: false,
  ...lastHeard,
})

const panel = ref<'none' | 'caps' | 'info'>('none')
const barShown = ref(true)
let barTimer: ReturnType<typeof setTimeout> | undefined
let giveUpTimer: ReturnType<typeof setTimeout> | undefined

/// Which pipeline restart owns the timeline. Every restart path writes `offset`
/// and `partBase` AFTER an await, so two in flight meant the last RESPONSE won
/// rather than the last request — and the seekbar and the arrow keys stay live
/// during a seek, so a nudge answering after a scrub left the clock and every
/// subtitle path reading a position the pipeline was not producing.
let seekGen = 0
/// How many times hls.js has been told to start loading again after a fatal
/// network error, reset whenever a segment actually arrives.
let netRestarts = 0
let mediaRecoveries = 0
let lastMediaRecovery = 0
/// Why the last start failed, quoted if the next one fails at the same point.
let lastFailure = ''
/// True once this player is gone: a restart that lands after that produced a
/// session with nobody to play, ping or end it.
let goneAway = false

/// HUB-33 memory scope, and the wishlists the opening choice came from.
const seriesId = props.item.parent_id ?? props.item.id
let subsWish: string[] = []
let subTrackWish: number | null = null
/// A remembered burn that could not be applied yet, because the viewer was
/// already steering.
let pendingBurn: number | null = null

const phase = computed(() =>
  playerPhase({
    standby: health.value.standby,
    gone: health.value.gone,
    restarting: health.value.awaitingGen !== 0,
    paused: playing.paused,
  }),
)
/// A dialog owns the screen; nothing behind it may be pressed.
const blocked = computed(() => phase.value === 'standby' || phase.value === 'gone')
/// The pipeline is not the viewer's to steer right now.
const frozen = computed(() => blocked.value || phase.value === 'restarting')

const selected = computed(() => trk.value.subs.find((s) => String(s.id) === trk.value.subKey))
const route = computed(() =>
  subtitleRoute(selected.value, { isHls: isHls.value, vttFallback: trk.value.vttFallback }),
)

const subtitles = useSubtitleRenderers({
  video,
  route,
  selected,
  subKey: computed(() => trk.value.subKey),
  epoch: computed(() => trk.value.epoch),
  offset,
  itemId: computed(() => props.item.id),
  session: computed(() => props.session),
  isHls,
  onTapEmpty: () => sendTrack({ type: 'tap-empty' }),
})

// ---- what is being played, and with which tracks -------------------------

/// One resolution (HUB-33): prefs plus the announced streams give the selector
/// state and the subtitle default. Everything it needs arrived with the props —
/// the QUERY that chose this source, the preferences that chose its audio
/// track, and the library's media type — so this asks the hub for nothing.
function resolve() {
  try {
    const detail = props.item
    const audio = detail.sources[0]?.streams?.audio ?? []
    sendTrack({
      type: 'lists-arrived',
      audioList: audio,
      videoList: detail.sources[0]?.streams?.video ?? [],
    })
    const resolved = resolveTracks(
      props.prefs,
      seriesId,
      props.item.id,
      props.mediaType,
      detail.metadata?.original_language,
      audio,
    )
    // A restart carries BOTH axes: the session was started on the carried
    // video track, and a selector left reading zero would hand track 0 back
    // to the NEXT restart — the same silent revert this prop exists to stop,
    // on the other axis.
    if (props.carried) {
      sendTrack({
        type: 'tracks-chosen',
        audio: props.carried.audio,
        video: props.carried.video,
      })
    } else {
      sendTrack({ type: 'audio-known', audio: resolved.audioTrack })
    }
    subsWish = resolved.subs
    subTrackWish = resolved.subTrack

    // The full list arrived with the item: QUERY answered "what would I be
    // served", and delivery is already computed against this client's bits.
    const subs = detail.negotiated?.subtitles ?? []
    sendTrack({ type: 'subtitles-arrived', subs })
    // A restart puts the viewer back exactly where they were — including
    // "subtitles off", which is as much a choice as any track. The prefs
    // pick below is for a FIRST mount, resolved from a snapshot that
    // predates anything chosen mid-episode.
    if (props.carried?.subKey === '') return
    // A carried key the new session's list no longer resolves (nothing does
    // this today — the item is not refetched on restart — but ids are only
    // as stable as that stays true) falls back to the wishlist rather than
    // silently landing on subtitles-off.
    const carried = props.carried
      ? subs.find((s) => String(s.id) === props.carried?.subKey)
      : undefined
    const pick = carried ?? initialSubtitle({ subs, exactId: subTrackWish, wishlist: subsWish })
    if (!pick) return
    // Never overrides a choice already made.
    sendTrack({ type: 'subtitle-chosen', key: String(pick.id), onlyIfUnset: true })
    if (!needsBurnRestart(pick, trk.value.streams)) return
    // Deferred, not dropped, while the pipeline is being steered. This is the
    // one restart caller that is not a button, so a burn re-applied a few
    // hundred milliseconds into playback could take the generation from a seek
    // the viewer had just made and restart before that seek had written
    // `offset` — the drag went silently.
    if (isFrozen(health.value)) pendingBurn = pick.id
    else void switchBurn(pick.id)
  } catch (cause) {
    // NOT emptied. The picture is playing — this session was negotiated — so a
    // fault in the player's own track resolution says nothing about what the
    // file contains. Blanking them removed the selectors entirely, and the
    // viewer's reading of that is "this file has no subtitles", which is false
    // and offers nothing to press.
    playerNote(`Could not work out the track list: ${sentence(cause)}`)
  }
}

// ---- restarting the pipeline ---------------------------------------------

/// Freeze the old run and take the timeline. Returns the generation to check
/// after the await and to hand to `attach`.
function beginRestart(): number {
  const mine = ++seekGen
  send({ type: 'timeline-taken', gen: mine })
  hls?.stopLoad() // the restart 404s the old run's segments
  video.value?.pause()
  // Armed HERE, not after the POST returns: the veil goes up and the transport
  // freezes before the seek is even sent, and the transport has no timeout — so
  // a hub that accepts the connection and then wedges left the spinner up for
  // ever with every control dead.
  clearTimeout(giveUpTimer)
  giveUpTimer = setTimeout(() => {
    if (health.value.awaitingGen !== mine) return
    giveUp('The stream did not come back. Press play to try again.', mine)
  }, RESTART_GIVEUP_MS)
  return mine
}

/// A restart is not coming: stop pretending one is, and make the play button
/// mean something again.
///
/// `beginRestart` stops the loader and pauses, so every path that abandons a
/// restart has to undo that or the picture is simply stuck. Checked against the
/// generation, because late means superseded means no-op: unchecked, an older
/// POST answering "no" pauses the picture and marks the player dead while a
/// newer restart is still genuinely coming.
function giveUp(why: string, gen = health.value.awaitingGen) {
  if (health.value.awaitingGen !== gen) return
  video.value?.pause()
  send({ type: 'gave-up', gen })
  playerNote(why)
}

/// What the viewer is actually watching with right now, for the restart to
/// hand back to the next mount.
function liveChoice(): CarriedTracks {
  return { audio: trk.value.audio, video: trk.value.video, subKey: trk.value.subKey }
}

/// The restart itself, without the guards. Shared by the automatic path and by
/// the viewer pressing Try again, which is not a loop and must not be treated
/// as one.
async function restartAt(at: number): Promise<boolean> {
  try {
    const fresh = await startPlaybackSession(props.item, at, trk.value.audio, trk.value.video)
    if (goneAway) {
      void endSession(fresh.session_id, { keepalive: true }).catch(() => {})
      return false
    }
    emit('restart', props.item.id, fresh, at, liveChoice())
    return true
  } catch (cause) {
    lastFailure = sentence(cause)
    // A ceiling of `null` is weather: nothing is wrong with the item and the
    // condition clears itself, which is what the stand-by dialog is for.
    if (startCeiling(cause) === null) {
      // Stop the picture. There is still a buffer, and left alone it plays on
      // behind the dialog — sound coming out of a screen that says the file is
      // unreachable, and a timeline running past what anyone watched.
      video.value?.pause()
      // So the resume position is where it STOPPED, not where it was when the
      // failed start left: a round trip's worth of buffer plays out in between.
      send({ type: 'host-away', atMs: Math.round(absMs()) })
    } else {
      video.value?.pause()
      send({ type: 'stopped', why: sentence(cause) })
    }
    return false
  }
}

/// Asked for, rather than triggered. Clears the loop guard and ignores the
/// paused check: both exist to stop the player restarting itself, and neither
/// should stand in the way of somebody who pressed a button.
function retryByHand() {
  send({ type: 'retry-by-hand' })
  forgetRecoveries()
  void restartAt(Math.round(absMs()))
}

/// The hub no longer has this session — reaped for idleness, lost to a restart,
/// ended elsewhere. Start a fresh one where we are and hand it up.
///
/// Driven only by a 404. Nothing here knows or guesses how long a session may
/// idle; see `domain/recovery.ts`.
///
/// `ourPause` says the caller paused the element itself for a restart, so the
/// check below must not read that as the viewer having stopped watching.
async function recover(ourPause = false) {
  if (!ourPause && video.value?.paused) {
    send({ type: 'died-while-paused' })
    return
  }
  if (health.value.recovering) return
  send({ type: 'recovery-started' })
  const at = Math.round(absMs())
  // Two restarts at the same position mean the first never played.
  if (!mayRecover(props.item.id, at, performance.now())) {
    video.value?.pause()
    send({
      type: 'stopped',
      // The hub's messages start lower case, so a full stop before one reads
      // like a typo. A dash joins them without pretending it is a sentence.
      why: lastFailure
        ? `It restarted once and stopped again at the same point — ${lastFailure}`
        : 'It restarted once and stopped again at the same point.',
    })
    send({ type: 'recovery-ended' })
    return
  }
  await restartAt(at)
  send({ type: 'recovery-ended' })
}

/// A capability mask reaches the hub only on a NEW session — it stores the
/// effective profile per session and re-plans track switches against it — so
/// applying one restarts playback.
async function restartWithCaps() {
  send({ type: 'caps-restart-started' })
  try {
    const at = Math.round(absMs())
    const fresh = await startPlaybackSession(props.item, at, trk.value.audio, trk.value.video)
    if (goneAway) {
      void endSession(fresh.session_id, { keepalive: true }).catch(() => {})
      return
    }
    emit('restart', props.item.id, fresh, at, liveChoice())
  } catch (cause) {
    send({ type: 'caps-restart-failed', why: sentence(cause) })
  }
}

/// What a seek-shaped restart did.
///
/// A boolean was not enough. The track switch has to know the difference
/// between "it is playing" — the only answer that may write a preference — and
/// every other one, two of which must put the selector back: a 404 answered by
/// snapping to the old track and then recovering onto the NEW one left Japanese
/// audio playing under a selector reading English.
type Steered = 'played' | 'gone' | 'waiting' | 'refused' | 'superseded'

/// What every seek-shaped restart does with its answer, and with its failures.
/// Three callers — a seek, a track switch and a burn transition — and the three
/// of them used to disagree about which failures meant what.
async function steer(
  mine: number,
  what: () => Promise<{ part_base_ms: number; streams?: unknown }>,
  at: number,
  said: {
    refused: (why: string) => void
    /// Put the control back before anything else happens. It runs BEFORE the
    /// 404 branch on purpose: `recover` opens its session on whatever the
    /// selectors currently say, so reverting afterwards snapped the selector
    /// back to English and then started the recovery on Japanese.
    revert?: () => void
  },
): Promise<Steered> {
  try {
    const answer = await what()
    // `goneAway` as well as the generation: `seekGen` belongs to THIS instance,
    // so after an unmount it still matches and the code below would attach to a
    // detached element — a second destroy, a manifest fetch for a session the
    // teardown just ended, and a fatal error whose handler starts a replacement
    // session nobody collects.
    if (mine !== seekGen || goneAway) return 'superseded'
    if (answer.streams) {
      sendTrack({ type: 'streams-known', streams: answer.streams as never })
    }
    partBase = answer.part_base_ms ?? 0
    offset.value = Math.round(at)
    playing.posMs = offset.value
    sendTrack({ type: 'run-moved' })
    attach(mine)
    return 'played'
  } catch (cause) {
    // Anything but a wait: the pick is not what is playing. A wait keeps it,
    // because the stand-by resume carries the choice.
    if (startCeiling(cause) !== null || isSessionGone(cause)) said.revert?.()
    // The session is gone: `recover` owns the outcome from here — a new session
    // and a remount, or the stopped dialog. Keep the veil up, because the
    // element is paused on purpose and a play button over it would lie.
    if (isSessionGone(cause)) {
      void recover(true)
      return 'gone'
    }
    // 503, no answer and a hub that failed are all a WAIT. The hub answers
    // starts and seeks through the same refusal, so a host vanishing mid-film
    // and noticed by a nudge used to skip stand-by entirely — for the one
    // condition stand-by exists for.
    if (startCeiling(cause) === null) {
      send({ type: 'host-away', atMs: Math.round(at) })
      return 'waiting'
    }
    // `beginRestart` has already stopped the loader and paused the element, and
    // nothing on this path starts it again — so a silent failure here is a
    // picture that froze for good, with the keepalive holding the session alive
    // so the 404 `recover` waits for never came. Hand the retry back now: the
    // answer is already in, and it was no.
    said.refused(sentence(cause))
    return 'refused'
  }
}

/// Only a run that never got going settles here: the POST returning means the
/// run has been ASKED for, not that there is a picture. A superseded one is
/// somebody else's to settle, and the reducer ignores it anyway.
const owned = (outcome: Steered) => outcome !== 'refused'

/// Seek anywhere on the full timeline: inside the produced range it is a plain
/// element seek; beyond it the hub restarts the pipeline at the target.
async function seekTo(targetMs: number) {
  if (isFrozen(health.value)) return
  const element = video.value
  if (!element) return
  const plan = planSeek({
    targetMs,
    offsetMs: offset.value,
    producedEndS:
      element.seekable.length > 0 ? element.seekable.end(element.seekable.length - 1) : 0,
    isHls: isHls.value,
  })
  // A direct file is the whole film, and a target already produced is a jump
  // the element can make on its own. Only the third answer costs a pipeline.
  if (plan.kind !== 'restart') {
    element.currentTime = plan.toS
    return
  }
  const mine = beginRestart()
  const outcome = await steer(
    mine,
    () => seekSession(props.session.session_id, targetMs),
    targetMs,
    { refused: (why) => giveUp(`Could not seek: ${why}`, mine) },
  )
  if (!owned(outcome)) settle(mine)
}

/// Track switching is a seek-restart at the current position with the new
/// track: the same ~2 s hiccup as a deep seek.
async function switchTracks(audio: number, videoTrack: number) {
  const was = { audio: trk.value.audio, video: trk.value.video }
  sendTrack({ type: 'tracks-chosen', audio, video: videoTrack })
  const mine = beginRestart()
  const at = absMs()
  const outcome = await steer(
    mine,
    () => seekSession(props.session.session_id, at, audio, videoTrack),
    at,
    {
      refused: (why) => giveUp(`Could not switch track: ${why}`, mine),
      // The selector must not name a track that is not playing — unless the
      // answer was "wait", where the pick is still what they asked for.
      revert: () => sendTrack({ type: 'tracks-chosen', audio: was.audio, video: was.video }),
    },
  )
  if (outcome === 'played' && audio !== was.audio) {
    // Remembered only now it is playing. Written before the switch, a failed
    // one still steers every later episode of the series towards a track this
    // one could not manage.
    //
    // Two additive layers (HUB-33). The SERIES remembers the language, which is
    // portable across episodes with differing track orders. MOVIES additionally
    // pin the exact index: "the commentary track of THIS film" has no language
    // representation, and there is no series intent to follow. Episodes
    // deliberately do NOT pin, so one episode never freezes on an old choice.
    const value = trk.value.audioList[audio]?.language?.toLowerCase() ?? `#${audio}`
    remember(seriesId, 'audio', value)
    if (props.item.kind === 'movie') remember(props.item.id, 'audio.track', `#${audio}`)
  }
  if (!owned(outcome)) settle(mine)
}

/// Burn transitions reuse the same machinery: the pipeline restarts at the
/// current position with the new burn state — an id burns that track, 0
/// withdraws an explicit burn.
async function switchBurn(trackId: number) {
  if (!video.value) return
  const mine = beginRestart()
  const at = absMs()
  const outcome = await steer(
    mine,
    () => seekSession(props.session.session_id, at, undefined, undefined, trackId),
    at,
    { refused: (why) => giveUp(`Could not change subtitles: ${why}`, mine) },
  )
  if (!owned(outcome)) settle(mine)
}

/// Offset starts snap to the keyframe before the requested position, and the
/// pipeline reports the true playlist origin in `start.pos`. Adopt it so
/// subtitle cues and the seekbar line up exactly.
///
/// Guarded like every other post-await writer of `offset`: the retry sleeps
/// between attempts precisely because `start.pos` is often not written yet, so
/// it routinely outlives the seek that asked for it.
async function syncOrigin(gen: number) {
  if (offset.value === 0) return
  const base = props.session.stream_url.replace(/[^/]*$/, '')
  // TWO generation checks, and either one alone is enough — a mutation of one
  // is masked by the other, which is worth knowing before deleting either. The
  // one before the write is the load-bearing one; the earlier is an early-out
  // that saves reading a body nobody will use.
  for (let attempt = 0; attempt < 3; attempt++) {
    try {
      const response = await fetch(`${base}start.pos`)
      if (gen !== seekGen) return
      if (response.ok) {
        const local = Math.round(Number(await response.text()))
        if (gen !== seekGen) return
        const origin = partBase + local
        if (
          Number.isFinite(origin) &&
          origin !== offset.value &&
          Math.abs(origin - offset.value) < 60_000
        ) {
          offset.value = origin
          subtitles.nudgeOffset(origin)
          if (video.value) playing.posMs = origin + video.value.currentTime * 1000
          sendTrack({ type: 'run-moved' })
        }
        return
      }
    } catch {
      // Retry.
    }
    await new Promise((resolve) => setTimeout(resolve, 700))
  }
}

/// `gen` is the restart whose picture this attach waits for. The mount call
/// takes the default: 0 owns no veil, so settling it is a no-op.
function attach(gen = 0) {
  const element = video.value
  if (!element) return
  hls?.destroy()
  hls = null
  if (isHls.value && Hls.isSupported()) {
    const engine = new Hls({
      // Media requests carry the bearer; the cookie is the fallback for engines
      // we do not drive ourselves.
      xhrSetup: (xhr) => {
        const token = accessToken()
        if (token) xhr.setRequestHeader('Authorization', `Bearer ${token}`)
      },
      // Our EVENT playlists are growing recordings, not live TV: the pipeline
      // paces itself a window ahead of THIS player, so the default live-edge
      // sync creates a feedback loop — hls.js chases the edge, the edge moves
      // with it, playback lives at the starved frontier and buffers on every
      // segment. Watch from the beginning and never chase.
      startPosition: 0,
      liveSyncDurationCount: 1e6,
      liveMaxLatencyDurationCount: Infinity,
      maxBufferLength: 60,
    })
    // A segment arrived, so whatever the link was doing it is doing it again:
    // the restart budget is per outage, not per session.
    engine.on(Hls.Events.FRAG_BUFFERED, () => (netRestarts = 0))
    engine.on(Hls.Events.ERROR, (_event, data) => {
      const code = data.response?.code
      // A dead session 404s every segment and playlist refresh.
      if (code === 404) {
        void recover()
        return
      }
      // hls.js fetches with its own XHR, so it never gets the transport's
      // refresh-and-retry. Without this an expired token stops playback dead —
      // and hides a 404 behind a 401, because auth runs first.
      if (code === 401) {
        void refreshTokens().then((ok) => ok && engine.startLoad())
        return
      }
      if (!data.fatal) return
      lastFailure = `${data.type}: ${data.details}${code ? ` (HTTP ${code})` : ''}`
      // hls.js does not restart itself after a fatal error, so without this
      // nothing fetched another segment, ever: the picture froze without
      // pausing, so no veil appeared; the ping kept succeeding, so the session
      // was never reaped and never answered 404; and 404 was the only thing
      // that called `recover`. A viewer whose wifi dropped for twenty seconds
      // sat looking at a still frame with nothing on screen to say so.
      if (data.type === Hls.ErrorTypes.NETWORK_ERROR) {
        // Past the budget, but only once there is nothing left to play. A fatal
        // network error does NOT stop the picture — hls.js stops its own loader
        // and the element plays the buffer out — and it goes fatal every three
        // or four seconds while a hub is unreachable, so a flat count of five
        // was spent inside the buffer and paused a video with forty seconds in
        // hand. While there is picture, keep asking.
        const ahead = bufferedAhead(element)
        if (netRestarts >= NET_RESTART_LIMIT && ahead < 2) {
          giveUp('The stream stopped and did not come back. Press play to try again.')
          return
        }
        netRestarts += 1
        engine.startLoad()
      } else if (data.type === Hls.ErrorTypes.MEDIA_ERROR) {
        // Budgeted, like the network case above and for the same reason:
        // hls.js never stops asking. A stream it cannot append — one whose
        // first segment carries no parameter sets, say — answered every
        // failure with a fresh MediaSource and never told anybody.
        const now = performance.now()
        if (now - lastMediaRecovery > MEDIA_RECOVER_WINDOW_MS) mediaRecoveries = 0
        lastMediaRecovery = now
        if (mediaRecoveries >= MEDIA_RECOVER_LIMIT) {
          giveUp(`This will not play in the browser — ${data.details}.`)
          return
        }
        mediaRecoveries += 1
        engine.recoverMediaError()
      } else {
        void recover()
      }
    })
    engine.loadSource(props.session.stream_url)
    engine.attachMedia(element)
    hls = engine
  } else {
    element.src = props.session.stream_url // cookie-authenticated
  }
  // The restart is over when there is a PICTURE, not when the hub answered —
  // and it is over for THIS generation only. Registered here so the generation
  // is in the closure: a persistent listener has nothing to compare against,
  // which is how a superseded run's `playing` cleared a newer run's veil.
  element.addEventListener('playing', () => settle(gen), { once: true })
  // A refused autoplay is not an error to swallow: it is the whole reason the
  // viewer has to click, and it fires NO `pause` event, so the state has to be
  // set here rather than waited for. An AbortError is the opposite case — a
  // NEWER restart interrupted this play, and that run owns the veil.
  void element.play().catch((cause: unknown) => {
    playing.paused = true
    if ((cause as DOMException | null)?.name !== 'AbortError') settle(gen)
  })
  void syncOrigin(gen)
}

/// How much is buffered ahead of where the element is.
function bufferedAhead(element: HTMLVideoElement): number {
  for (let i = 0; i < element.buffered.length; i++) {
    if (
      element.buffered.start(i) <= element.currentTime &&
      element.currentTime <= element.buffered.end(i)
    ) {
      return element.buffered.end(i) - element.currentTime
    }
  }
  return 0
}

// ---- the element's own lifetime ------------------------------------------

let stopPinging: (() => void) | undefined

onMounted(() => {
  const element = video.value
  if (!element) return
  attach()
  resolve()
  // The setting carried in from the last session, pushed by hand: the watcher
  // below only sees CHANGES, and an `immediate` one runs during setup, where
  // there is no element yet. Without this every restart came back at full
  // volume — which is the whole reason `lastHeard` is at module scope.
  element.volume = playing.volume
  element.muted = playing.muted

  const seekToResume = () => {
    if (!isHls.value && props.resumeMs > 0) element.currentTime = props.resumeMs / 1000
    positionKnown = true
  }
  const onTime = () => {
    playing.posMs = absMs()
    playing.producedMs = producedEndMs({
      offsetMs: offset.value,
      seekableEndS:
        element.seekable.length > 0 ? element.seekable.end(element.seekable.length - 1) : null,
    })
  }
  // `canplay` as well as the transitions: `paused` starts as a guess, and on
  // the path where autoplay is refused there is no transition to correct it.
  const syncPaused = () => (playing.paused = element.paused)
  const onVolume = () => {
    heard(element.volume, element.muted)
    playing.volume = element.volume
    playing.muted = element.muted
  }
  // The gesture that makes recovery worth doing. Proactive rather than waiting
  // for the load to fail, so there is no error flash first.
  const onPlay = () => {
    if (!health.value.dead) return
    send({ type: 'play-pressed' })
    void recover()
  }
  // Direct play has no hls.js to report a status: the element just fails. Ask
  // the hub which kind of failure it was — a 404 is a dead session, anything
  // else is a real media fault and stays one.
  const onError = () => {
    void postProgress(props.session.session_id, {
      position_ms: Math.round(absMs(element)),
    }).catch((cause) => {
      if (isSessionGone(cause)) void recover()
    })
  }
  const report = (keepalive = false) =>
    postProgress(
      props.session.session_id,
      { position_ms: Math.round(absMs(element)) },
      { keepalive },
    ).catch(() => {})
  const onPause = () => void report()
  const onEnded = () => {
    void report()
    // Multi-part sources (CD1/CD2): this part's playlist ended but the film has
    // not — restart into the next part.
    const nextPart = nextPartSeekMs({
      absMs: absMs(element),
      durationMs: durationMs.value,
      parts: props.session.parts ?? 1,
      isHls: isHls.value,
    })
    if (nextPart !== null) void seekTo(nextPart)
  }
  // Where the viewer got to, on the way out.
  const onUnload = () => void report(true)

  element.addEventListener('loadedmetadata', seekToResume)
  element.addEventListener('timeupdate', onTime)
  element.addEventListener('progress', onTime)
  element.addEventListener('play', syncPaused)
  element.addEventListener('pause', syncPaused)
  element.addEventListener('canplay', syncPaused)
  element.addEventListener('volumechange', onVolume)
  element.addEventListener('play', onPlay)
  element.addEventListener('error', onError)
  element.addEventListener('pause', onPause)
  element.addEventListener('ended', onEnded)
  window.addEventListener('beforeunload', onUnload)

  // Pings while paused too, bounded — see `domain/keepalive.ts`. Guarding this
  // on `!paused` is what let the reaper delete a paused viewer's segment
  // directory out from under them. The ping doubles as the earliest death
  // detector: a session lost to ANY cause answers 404 here, usually before the
  // picture stalls.
  stopPinging = keepSessionAlive(
    () => absMs(element),
    (ms) => {
      void postProgress(props.session.session_id, { position_ms: Math.round(ms) }).catch(
        (cause) => {
          if (isSessionGone(cause)) void recover()
        },
      )
    },
  )

  // A recovered session remounts the player without moving the pointer. Arm the
  // same countdown here or its fresh controls stay up indefinitely.
  barTimer = setTimeout(() => (barShown.value = false), CONTROLS_HIDE_MS)

  onBeforeUnmount(() => {
    goneAway = true
    stopPinging?.()
    clearTimeout(barTimer)
    clearTimeout(giveUpTimer)
    element.removeEventListener('loadedmetadata', seekToResume)
    element.removeEventListener('timeupdate', onTime)
    element.removeEventListener('progress', onTime)
    element.removeEventListener('play', syncPaused)
    element.removeEventListener('pause', syncPaused)
    element.removeEventListener('canplay', syncPaused)
    element.removeEventListener('volumechange', onVolume)
    element.removeEventListener('play', onPlay)
    element.removeEventListener('error', onError)
    element.removeEventListener('pause', onPause)
    element.removeEventListener('ended', onEnded)
    window.removeEventListener('beforeunload', onUnload)
    // Report, do not release: everything else here undoes something this
    // component did, and can be done again. Ending the session could not — the
    // route owns that, because the route owns the session.
    void report(true)
    hls?.destroy()
    hls = null
  })
})

/// The element is the authority on volume; this pushes changes into it, and
/// `volumechange` brings back the ones it makes on its own — a media key, or
/// the system mixer.
///
/// `lastHeard` is written HERE as well as in that listener, and not only there:
/// the listener is what carries the setting across the next session, and an
/// element that accepts a volume without announcing it would drop the whole
/// arrangement on the floor.
watch(
  () => [playing.volume, playing.muted],
  () => {
    const element = video.value
    if (!element) return
    element.volume = playing.volume
    element.muted = playing.muted
    heard(playing.volume, playing.muted)
  },
)

/// Ask again until it works. Deliberately NOT through `mayRecover`: that guard
/// exists to stop a session respawning at a position it never played, and this
/// is the opposite case — we know exactly why it failed and we are waiting for
/// that to stop being true.
watch(
  () => health.value.standby,
  (standby, _was, onCleanup) => {
    if (standby === null) return
    let stop = false
    // One at a time. A start may take up to a minute, while this fires every
    // five seconds, so twelve could be outstanding at once — each holding a
    // per-user admission slot, with the overflow refused for concurrency. The
    // loop talked itself out of standing by.
    let inFlight = false
    const tick = async () => {
      if (inFlight) return
      inFlight = true
      try {
        const fresh = await startPlaybackSession(
          props.item,
          standby,
          trk.value.audio,
          trk.value.video,
        )
        // A session started after the player left is one nobody will ever play,
        // ping or end.
        if (stop) void endSession(fresh.session_id, { keepalive: true }).catch(() => {})
        else emit('restart', props.item.id, fresh, standby, liveChoice())
      } catch (cause) {
        // Still away: keep waiting. Anything else is a real failure and the
        // stand-by was the wrong answer to it.
        if (!stop && startCeiling(cause) !== null) send({ type: 'stopped', why: sentence(cause) })
      } finally {
        inFlight = false
      }
    }
    const timer = setInterval(() => void tick(), STANDBY_RETRY_MS)
    onCleanup(() => {
      stop = true
      clearInterval(timer)
    })
  },
)

/// The deferred burn, once the pipeline is the viewer's to steer again.
watch(frozen, (busy) => {
  if (busy || pendingBurn === null) return
  const id = pendingBurn
  pendingBurn = null
  void switchBurn(id)
})

// ---- the transport --------------------------------------------------------

function togglePause() {
  const element = video.value
  // Guarded here rather than at the six callers: the keyboard, the transport
  // button, the two nudges, the seekbar and the picture's own click all funnel
  // through this and `seekTo`. A dialog blocks the pointer with a scrim and
  // never blocked the keyboard, so Space played the buffered tail behind "the
  // file is unreachable" — the sound `restartAt` pauses to stop.
  if (isFrozen(health.value) || !element) return
  if (element.paused) void element.play()
  else element.pause()
}

/// `durationMs` is 0 when the hub has no probed duration, and clamping to it
/// turned every nudge into a jump to the start of the film.
const nudge = (bySec: number) =>
  void seekTo(nudgeTarget({ posMs: playing.posMs, bySec, durationMs: durationMs.value }))

/// Reveal the overlay and start its countdown again. Kept visible while paused:
/// a paused picture with no controls looks broken.
function wake() {
  barShown.value = true
  clearTimeout(barTimer)
  barTimer = setTimeout(() => (barShown.value = false), CONTROLS_HIDE_MS)
}

/// The pointer left the picture. The timer goes with the bar, or a countdown
/// armed before it left fires under a later `wake` and hides the controls a
/// second after the pointer came back.
function away() {
  barShown.value = false
  clearTimeout(barTimer)
}

function onKey(event: KeyboardEvent) {
  const asked = playerIntent(event.key, {
    typing: isTypingTarget((event.target as HTMLElement | null)?.tagName),
    mode: props.mode,
  })
  if (!asked) return
  if (asked.preventDefault) event.preventDefault()
  const { intent } = asked
  if (intent.kind === 'toggle-pause') togglePause()
  else if (intent.kind === 'nudge') nudge(intent.seconds)
  else emit('mode', intent.to)
  wake()
}
onMounted(() => window.addEventListener('keydown', onKey))
onBeforeUnmount(() => window.removeEventListener('keydown', onKey))

// Fullscreen is the browser's, so it can be left without asking us — Escape, or
// the window's own control. Follow it rather than assume.
function onFullscreen() {
  if (!document.fullscreenElement && props.mode === 'full') emit('mode', 'window')
}
onMounted(() => document.addEventListener('fullscreenchange', onFullscreen))
onBeforeUnmount(() => document.removeEventListener('fullscreenchange', onFullscreen))

watch(
  () => props.mode,
  (mode) => {
    if (mode === 'full' && !document.fullscreenElement) void box.value?.requestFullscreen?.()
    if (mode !== 'full' && document.fullscreenElement) void document.exitFullscreen?.()
  },
)

/// A player that has given up must let go of its session.
///
/// The keepalive pings for as long as the picture is mounted, and that ping is
/// what holds the session: giving up paused the picture and said so, and then
/// went on telling the hub the session was in use. Nothing was watching it and
/// nothing ever would be — the way out is the play button, and `onPlay`
/// answers that with `recover`, which starts a FRESH session. So four failed
/// attempts filled a viewer's whole allowance and the fifth was refused for
/// concurrency, with four abandoned pipelines still pacing behind it.
///
/// `keepalive: true` because this can also run as the tab goes.
watch(
  () => health.value.dead,
  (dead) => {
    if (!dead) return
    stopPinging?.()
    stopPinging = undefined
    void endSession(props.session.session_id, { keepalive: true }).catch(() => {})
  },
)

/// Keep the subtitles clear of the control bar.
///
/// Native `<track>` cues live in the video's own shadow tree, where a transform
/// on a sibling cannot reach them; they move by `line`, which counts from the
/// bottom when negative. The browser does this for its own controls and has no
/// idea ours exist.
watch(
  [barShown, () => playing.paused, () => trk.value.subKey, () => trk.value.epoch],
  () => {
    const element = video.value
    if (!element) return
    const lift = () => {
      for (const track of Array.from(element.textTracks)) {
        for (const cue of Array.from(track.cues ?? [])) {
          ;(cue as VTTCue).line = barShown.value || playing.paused ? -4 : 'auto'
        }
      }
    }
    lift()
    // Cues arrive as the track loads, and a fresh one carries no line.
    for (const track of Array.from(element.textTracks)) {
      track.removeEventListener('cuechange', lift)
      track.addEventListener('cuechange', lift)
    }
  },
  { flush: 'post' },
)

// ---- skipping the recap, the opening and the credits ----------------------

/// HUB-37. What the hub found in this episode, as the QUERY that chose this
/// source reported it — the same call that carries the subtitle listing, so
/// there is no second round trip on the way into playback and the next
/// episode's boundaries arrive with the next episode. Empty when nothing was
/// found, and when nothing has been analysed: the difference is not one a
/// player can act on.
const skipping = computed(() => skippable(props.item.segments ?? [], playing.posMs))
const skipText = computed(() => skipLabel(skipping.value))

function skip() {
  const segment = skipping.value
  if (segment) void seekTo(skipTarget(segment, durationMs.value))
}

/// The file's own chapters, drawn on the bar — and each mark is an 11px
/// button that seeks to its chapter (pointer events gated on the bar being
/// visible; see the template). The container itself takes no pointer, so
/// the transport underneath keeps every press between the marks.
const ticks = computed(() => chapterTicks(props.item.chapters ?? [], durationMs.value))

/// Which way a mark's label hangs. Centred in the middle third, hung inward
/// from the tick in the outer thirds: a label centred on a mark near either
/// end runs past the videobox's overflow and loses its leading timestamp.
/// Thirds rather than a narrow edge band because the thresholds are in bar
/// percent while the label's width is in pixels — on a narrow window-mode
/// player the old 15/85 band left centred labels at 16% clipping.
// ponytail: percent thresholds + a 240px cap, not pixel-aware measurement;
// measure the bar if long titles on very narrow players ever matter.
function labelSide(pct: number) {
  if (pct > 67) return 'right-0'
  if (pct < 33) return 'left-0'
  return 'left-1/2 -translate-x-1/2'
}

// ---- the next episode -----------------------------------------------------

const next = ref<ItemQueryResponse | null>(null)
const upNextOff = ref(false)
const startingNext = ref(false)

onMounted(async () => {
  if (props.item.kind !== 'episode' || !props.item.parent_id) return
  try {
    const siblings = await itemChildren(props.item.parent_id)
    const at = siblings.children.findIndex((e) => e.id === props.item.id)
    const after = at >= 0 ? siblings.children[at + 1] : undefined
    if (!after || goneAway) return
    const full = await itemQuery(after.id, { profile: buildProfile() })
    if (!goneAway) next.value = full
  } catch {
    // No next episode to offer is not a failure worth a message.
  }
})

const remainMs = computed(() => (durationMs.value > 0 ? durationMs.value - playing.posMs : 0))
const upNextIn = computed(() => Math.max(0, Math.ceil(remainMs.value / 1000)))
const upNextOn = computed(
  () =>
    !!next.value && !upNextOff.value && durationMs.value > 0 && remainMs.value <= UP_NEXT_S * 1000,
)

async function playNext() {
  const after = next.value
  if (!after || startingNext.value) return
  startingNext.value = true
  try {
    // Resolved for the NEXT episode, not carried over from this one: the hub
    // takes a track number as a raw index and clamps it into range, and mux
    // order is not stable across a series. Watching [jpn, eng] on index 1 and
    // advancing into an episode muxed [eng, jpn] put on Japanese while the
    // selector said English.
    //
    // Prefs are re-read rather than cached: a track switch during this episode
    // wrote the series' language a moment ago, and that is exactly the choice
    // the next episode should follow.
    // A failed re-read falls back to the set THIS episode resolved from, not
    // to nothing: `[]` dropped the bandwidth cap and the series' language on
    // the next episode without a word.
    const prefs = await getPrefs().catch((cause: unknown) => {
      playerNote(`Could not re-read your preferences: ${sentence(cause)}`)
      return { prefs: props.prefs }
    })
    const resolved = resolveTracks(
      prefs.prefs,
      seriesId,
      after.id,
      props.mediaType,
      after.metadata?.original_language,
      after.sources[0]?.streams?.audio ?? [],
    )
    const fresh = await startPlaybackSession(after, 0, resolved.audioTrack, 0, prefs.prefs)
    // Back was pressed while the hub was answering. Handing this up would
    // navigate them into the next episode against the thing they just did.
    if (goneAway) {
      void endSession(fresh.session_id, { keepalive: true }).catch(() => {})
      return
    }
    emit('playNext', props.item.id, after, fresh, prefs.prefs)
  } catch (cause) {
    playerNote(`Could not start the next episode: ${sentence(cause)}`)
    startingNext.value = false
  }
}

/// Advance on `ended`, not on the arithmetic reaching zero: `timeupdate` stops
/// firing when playback finishes, so the position settles up to a second short
/// of the duration and a `remain <= 0` test never becomes true. The countdown
/// itself still runs off the position, so pausing pauses it.
onMounted(() => {
  const element = video.value
  if (!element) return
  const onDone = () => {
    if (!next.value || upNextOff.value) return
    // A multi-part source also ends here, and that is not the end of the film.
    if (
      durationMs.value > 0 &&
      durationMs.value - (offset.value + element.currentTime * 1000) > 3000
    ) {
      return
    }
    void playNext()
  }
  element.addEventListener('ended', onDone)
  onBeforeUnmount(() => element.removeEventListener('ended', onDone))
})

// ---- what is drawn --------------------------------------------------------

const fmt = hms

const pct = computed(() =>
  durationMs.value > 0 ? Math.min(100, (playing.posMs / durationMs.value) * 100) : 0,
)
const producedPct = computed(() =>
  durationMs.value > 0 ? Math.min(100, (playing.producedMs / durationMs.value) * 100) : 0,
)
/// `session.mode` describes pipeline ownership and container shape; the plan's
/// cost says what happened to the elementary streams, and it follows a track
/// switch.
const delivery = computed(() => deliveryPlan(trk.value.streams?.cost ?? props.session.mode))

/// QUERY carries the geometry of the exact source negotiation chose. Shape the
/// box before the first media byte; `auto` still lets intrinsic metadata take
/// over once the browser has decoded it.
const ratio = computed(() => {
  const source = props.item.negotiated?.source
  return source?.display_width && source.display_height
    ? `${source.display_width} / ${source.display_height}`
    : '16 / 9'
})

const me = whoAmI()

async function sessionLog() {
  try {
    saveAs(
      `session-${props.session.session_id}.log`,
      await adminSessionLog(props.session.session_id),
    )
  } catch (cause) {
    notify(`Could not download the session log: ${sentence(cause)}`)
  }
}

/// A named handler, not two statements in the template: a multi-statement
/// inline handler does not compile, and only the real build says so.
function setVolume(value: string) {
  playing.volume = Number(value) / 100
  playing.muted = false
}

function chooseSubtitle(key: string) {
  const previous = trk.value.subs.find((s) => String(s.id) === trk.value.subKey)
  const picked = trk.value.subs.find((s) => String(s.id) === key)
  sendTrack({ type: 'subtitle-chosen', key })
  // Two memory layers (HUB-33): the series remembers the language, and THIS
  // item remembers the exact row — the only spelling that can name a downloaded
  // or OCR track.
  remember(seriesId, 'subs', key === '' ? 'off' : (picked?.language ?? 'any').toLowerCase())
  remember(props.item.id, 'subs.track', key)
  // Burn transitions live server-side: a track whose delivery IS burn restarts
  // the pipeline with it, and leaving one withdraws it. The tier comes from the
  // ass_fallback preference, never from this list — picking says WHICH
  // subtitles, not how they are delivered.
  if (picked?.delivery === 'burn') void switchBurn(picked.id)
  else if (previous?.delivery === 'burn') void switchBurn(0)
}

/// Preference writes are fire-and-forget: a remembered choice that did not save
/// is not worth interrupting a film for. Ordered per key by `SerialQueue`, so
/// two picks in quick succession commit in the order they were made.
const remember = (scope: string, key: string, value: string) =>
  void writePref(scope, key, value).catch(() => {})
</script>

<template>
  <div
    ref="box"
    class="videobox"
    :class="(barShown || playing.paused) && 'bar-up'"
    :style="{ '--video-ratio': ratio }"
    @mousemove="wake"
    @mouseleave="away"
  >
    <video ref="video" playsinline crossorigin="use-credentials" @click="togglePause">
      <!-- Requesting a text form of an image track left a <track> load pending
           for ever, which kept Firefox's own buffering overlay latched over a
           playing video. -->
      <track
        v-if="route === 'vtt-track'"
        :key="`${trk.subKey}-${trk.epoch}`"
        default
        kind="subtitles"
        :src="subtitleFileUrl(props.item.id, `${trk.subKey}.vtt`, -Math.round(offset))"
      />
    </video>

    <!-- Chrome will not start a video for a viewer who has not interacted with
         the page, so a reloaded player sits on its first frame waiting for a
         click it never asked for. The transport does say `paused`, in a
         twelve-pixel glyph at the bottom; this says it in the middle, where the
         eye already is. Only in the `paused` phase: standing by, stopped and
         restarting all outrank it, and each of those pauses the element. -->
    <button
      v-if="phase === 'paused'"
      class="play-veil absolute inset-0 z-5 flex cursor-pointer items-center justify-center bg-black/25"
      type="button"
      aria-label="Play"
      @click="togglePause"
    >
      <span class="rounded-full bg-bg/80 p-5"><Icon name="play" :size="30" /></span>
    </button>

    <div
      v-if="phase === 'restarting'"
      class="absolute inset-0 z-5 flex items-center justify-center bg-black/45"
      role="status"
    >
      <span class="animate-spin text-[28px] text-teal" aria-hidden="true">↻</span>
      <!-- The words, not an `aria-label`: a live region whose NAME changes and
           whose contents stay empty announces nothing in most readers. -->
      <span class="sr-only">Restarting stream</span>
    </div>

    <div
      v-if="panel === 'info'"
      class="absolute top-3 right-3 z-8 flex max-w-[46ch] flex-col gap-1 rounded-md bg-bg/90 p-3 font-mono text-[11px]"
    >
      <span>
        <span class="text-dim">session </span>{{ props.item.title }} ·
        <span :class="`text-${delivery.tone}`">{{ delivery.chip }}</span> ·
        {{ props.session.content_type }}
      </span>
      <template v-if="trk.streams">
        <span><span class="text-dim">video </span>{{ trk.streams.video }}</span>
        <span><span class="text-dim">audio </span>{{ trk.streams.audio }}</span>
      </template>
      <span v-for="verdict in trk.streams?.subtitles ?? []" :key="verdict.index">
        <span class="text-dim">
          {{ [verdict.language, verdict.format].filter(Boolean).join(' ') || 'subs' }} ·
        </span>
        {{ verdict.tier }}
        <span v-if="verdict.note" class="text-dim"> — {{ verdict.note }}</span>
      </span>
      <!-- The mask this session was negotiated with, always visible while the
           panel is open: a forgotten mask must never read as a bug in the
           hub. -->
      <span v-if="masked.length" class="text-sand">masked {{ masked.join(' ') }}</span>
      <!-- OPS-10, where the problem is visible: the diagnostics for THIS
           session, beside the verdict that describes it. -->
      <button
        v-if="me.admin"
        class="cursor-pointer text-left text-teal underline"
        type="button"
        @click="sessionLog"
      >
        download session log
      </button>
    </div>

    <!-- HUB-37. Bottom right, clear of the transport, and it rides up with the
         bar so it is never under it. Announced once when it appears: a viewer
         who cannot see it still gets the offer, and it is only an offer —
         nothing is skipped unless it is pressed. Hidden while the next-episode
         card is up, which owns the same corner and is the more urgent of the
         two. -->
    <!-- The live region is ALWAYS mounted and only its text changes: a
         region inserted together with its content is the pattern several
         screen readers do not announce, and this announcement is the whole
         reason the sr-only text exists. -->
    <p class="sr-only" role="status" aria-live="polite">
      {{ skipping && !upNextOn ? `${skipText} available` : '' }}
    </p>
    <div
      v-if="skipping && !upNextOn"
      class="animate-rise absolute right-4 z-8"
      :class="barShown || playing.paused ? 'bottom-24' : 'bottom-8'"
    >
      <Btn small :disabled="frozen" @click="skip">{{ skipText }}</Btn>
    </div>

    <!-- Announced, because it takes over on its own: nine seconds is not long
         enough to discover an overlay you were not told about. -->
    <div
      v-if="upNextOn && next"
      class="animate-rise absolute right-4 z-8 flex w-[320px] flex-col gap-2 rounded-md border border-line bg-bg/95 p-3"
      :class="barShown || playing.paused ? 'bottom-24' : 'bottom-8'"
      role="region"
      aria-label="Up next"
    >
      <p class="sr-only" role="status" aria-live="polite">
        Next episode in {{ upNextIn }} seconds: {{ next.title }}
      </p>
      <div class="flex items-center gap-2">
        <span class="font-mono text-[11px] text-dim">next episode</span>
        <span class="ml-auto flex items-center gap-1">
          <!-- The ring empties as the count runs down; the number is there
               because a ring alone does not say how long. -->
          <svg width="26" height="26" viewBox="0 0 36 36" aria-hidden="true">
            <circle
              cx="18"
              cy="18"
              r="16"
              fill="none"
              stroke="var(--color-line)"
              stroke-width="3"
            />
            <circle
              cx="18"
              cy="18"
              r="16"
              fill="none"
              stroke="var(--color-teal)"
              stroke-width="3"
              stroke-linecap="round"
              stroke-dasharray="100.5"
              :stroke-dashoffset="100.5 * (1 - upNextIn / UP_NEXT_S)"
            />
          </svg>
          <span class="font-mono text-[12px]">{{ upNextIn }}</span>
        </span>
      </div>
      <button
        class="flex cursor-pointer items-center gap-2 text-left"
        type="button"
        @click="playNext"
      >
        <!-- Through `Art`, which is what everything else on the site uses: a
             thumbnail the browser cannot fetch is the swell rather than the
             broken-image glyph. -->
        <Art :item="next" size="thumb" class="w-20 shrink-0" :progress="false" />
        <span class="flex flex-col">
          <span class="font-mono text-[11px] text-dim">
            {{ seLabel(next.season, next.episode, next.episode_end) }}
          </span>
          <span class="line-clamp-2 text-[13px] font-semibold">{{ next.title }}</span>
        </span>
      </button>
      <div class="flex gap-2">
        <Btn small :disabled="startingNext" @click="playNext">
          {{ startingNext ? 'Starting…' : 'Play now' }}
        </Btn>
        <Btn ghost small @click="upNextOff = true">Stop</Btn>
      </div>
    </div>

    <div
      class="absolute right-0 bottom-0 left-0 z-6 flex flex-col gap-1 bg-gradient-to-t from-black/85 to-transparent px-3 pt-6 pb-2 transition-opacity"
      :class="barShown || playing.paused ? 'opacity-100' : 'pointer-events-none opacity-0'"
      @focusin="wake"
    >
      <!-- A real range input, not a div with a click handler. A div is not
           focusable, exposes no value and answers no key, so the only transport
           left to a keyboard user was ±10/+30: reaching the middle of a
           two-hour film is a hundred and twenty presses, and "skip the recap"
           is not reachable at all. The volume control two elements down was
           already a range; this is the same answer to the same question, and
           it brings Arrow, Home, End and Page along with it.
           The two-layer fill — what the pipeline has produced, and where the
           viewer is — is a gradient on the track, because a range input has no
           children to position. -->
      <div v-if="durationMs > 0" class="relative flex items-center">
        <input
          class="seekbar"
          type="range"
          min="0"
          :max="durationMs"
          step="1000"
          :value="Math.round(playing.posMs)"
          :disabled="frozen"
          :aria-label="`Seek — ${isHls ? 'beyond what the hub has produced this restarts the pipeline at the target' : 'anywhere in the file'}`"
          :aria-valuetext="`${fmt(playing.posMs)} of ${fmt(durationMs)}`"
          :style="{
            '--played': `${pct}%`,
            '--made': `${isHls ? producedPct : 100}%`,
          }"
          @input="seekTo(Number(($event.target as HTMLInputElement).value))"
        />
        <!-- Chapter marks, an overlay rather than more gradient stops on the
             track — a range input has no children to position, and a chapter
             list is not a fixed number of stops.
             A mark is one pixel wide and nobody can hit one pixel, so what
             takes the pointer is an 11px button around it: five pixels of
             slack either side, and the press lands on the chapter rather than
             wherever the cursor actually was. It costs those pixels from the
             bar underneath, which is a fair trade — a press on a chapter mark
             means that chapter.
             Out of the reading order deliberately: fifteen tab stops in front
             of Play would be a worse transport for a keyboard, which reaches
             every one of these positions with the arrow keys and finds them
             named, in order, on the item's own page. -->
        <div v-if="ticks.length" class="pointer-events-none absolute inset-x-0" aria-hidden="true">
          <!-- pointer-events only while the bar is SHOWING. An explicit
               `auto` on a descendant defeats the hidden bar's
               pointer-events-none — hit-testing descends — so a faded-out
               bar left invisible seek targets floating over the picture,
               and a click meant to pause jumped to a chapter instead. -->
          <button
            v-for="(tick, nth) in ticks"
            :key="`${tick.startMs}-${nth}`"
            class="group absolute top-1/2 h-3 w-[11px] -translate-x-1/2 -translate-y-1/2 cursor-pointer border-0 bg-transparent p-0 disabled:cursor-default"
            :class="barShown || playing.paused ? 'pointer-events-auto' : 'pointer-events-none'"
            type="button"
            tabindex="-1"
            :disabled="frozen"
            :style="{ left: `${tick.pct}%` }"
            @click="seekTo(tick.startMs)"
          >
            <span
              class="absolute top-1/2 left-1/2 h-1.5 w-px -translate-x-1/2 -translate-y-1/2 bg-black/50 group-hover:h-2.5 group-hover:w-[2px] group-hover:bg-sand"
            />
            <span
              class="pointer-events-none absolute bottom-[calc(100%+3px)] hidden max-w-[240px] truncate rounded-sm border border-line bg-bg/95 px-1.5 py-0.5 font-mono text-[11px] text-text group-hover:block"
              :class="labelSide(tick.pct)"
            >
              {{ hms(tick.startMs) }} · {{ tick.title }}
            </span>
          </button>
        </div>
      </div>

      <div class="flex flex-wrap items-center gap-2">
        <!-- This bar sits ABOVE the restart veil, so a Play glyph here would be
             the same offer the play veil is hidden to withdraw, two centimetres
             lower. -->
        <button
          class="tbtn"
          type="button"
          :title="playing.paused ? 'Play' : 'Pause'"
          :aria-label="playing.paused ? 'Play' : 'Pause'"
          :disabled="frozen"
          @click="togglePause"
        >
          <Icon :name="playing.paused ? 'play' : 'pause'" :size="18" />
        </button>
        <button
          class="tbtn font-mono"
          type="button"
          title="Back 10 s"
          aria-label="Back 10 seconds"
          :disabled="frozen"
          @click="nudge(-10)"
        >
          <Icon name="back10" :size="14" />10
        </button>
        <button
          class="tbtn font-mono"
          type="button"
          title="Forward 30 s"
          aria-label="Forward 30 seconds"
          :disabled="frozen"
          @click="nudge(30)"
        >
          <Icon name="fwd30" :size="14" />30
        </button>
        <span class="font-mono text-[12px]">{{ fmt(playing.posMs) }} / {{ fmt(durationMs) }}</span>
        <span v-if="phase === 'restarting'" class="font-mono text-[11px] text-dim">
          restarting pipeline at target…
        </span>

        <span class="flex items-center gap-1">
          <button
            class="tbtn"
            type="button"
            :title="playing.muted ? 'Unmute' : 'Mute'"
            :aria-label="playing.muted ? 'Unmute' : 'Mute'"
            :disabled="blocked"
            @click="playing.muted = !playing.muted"
          >
            <Icon
              :name="
                playing.muted || playing.volume === 0
                  ? 'volumeOff'
                  : playing.volume < 0.5
                    ? 'volumeLow'
                    : 'volume'
              "
              :size="14"
            />
          </button>
          <input
            class="w-20"
            type="range"
            min="0"
            max="100"
            aria-label="Volume"
            :aria-valuetext="`${Math.round((playing.muted ? 0 : playing.volume) * 100)} percent`"
            :disabled="blocked"
            :value="Math.round((playing.muted ? 0 : playing.volume) * 100)"
            @input="setVolume(($event.target as HTMLInputElement).value)"
          />
        </span>

        <span class="ml-auto flex flex-wrap items-center gap-2">
          <select
            v-if="isHls && trk.videoList.length > 1"
            class="tsel"
            title="Video track"
            aria-label="Video track"
            :value="trk.video"
            :disabled="frozen"
            @change="switchTracks(trk.audio, Number(($event.target as HTMLSelectElement).value))"
          >
            <option v-for="(v, at) in trk.videoList" :key="at" :value="at">
              {{ v.codec }} {{ v.width }}×{{ v.height }}
            </option>
          </select>
          <select
            v-if="isHls && trk.audioList.length > 1"
            class="tsel"
            title="Audio track"
            aria-label="Audio track"
            :value="trk.audio"
            :disabled="frozen"
            @change="switchTracks(Number(($event.target as HTMLSelectElement).value), trk.video)"
          >
            <option v-for="(a, at) in trk.audioList" :key="at" :value="at">
              {{ a.language ?? '?' }} · {{ a.codec }} {{ a.channels }}ch
            </option>
          </select>
          <!-- Disabled during a restart like the two above. This was the one
               control left live, and the only one that can START a restart:
               picking a burn track mid-seek bumps the generation again, so the
               seek already in flight bails and the hub runs two pipeline
               restarts for one intent. -->
          <select
            v-if="trk.subs.length"
            class="tsel"
            title="Subtitles"
            aria-label="Subtitles"
            :value="trk.subKey"
            :disabled="frozen"
            @change="chooseSubtitle(($event.target as HTMLSelectElement).value)"
          >
            <option value="">Subtitles off</option>
            <option
              v-for="s in trk.subs"
              :key="s.id"
              :value="String(s.id)"
              :disabled="s.delivery === 'none'"
            >
              {{ subtitleLabel(s) }}
            </option>
          </select>
          <button
            class="tpill"
            type="button"
            title="Playback info — why is this (not) transcoding"
            :disabled="blocked"
            :aria-pressed="panel === 'info'"
            :class="panel === 'info' && 'border-teal text-teal'"
            @click="panel = panel === 'info' ? 'none' : 'info'"
          >
            info
          </button>
          <!-- The capability panel is BELOW the picture, not an overlay, so it
               cannot move inside an overflow-hidden box. In fullscreen it is
               therefore unreachable, and a button that does nothing is worse
               than no button. -->
          <button
            v-if="props.mode !== 'full'"
            class="tpill"
            type="button"
            title="Client capabilities — mask one off and restart to see the other branch"
            :disabled="blocked"
            :aria-pressed="panel === 'caps'"
            :class="panel === 'caps' && 'border-teal text-teal'"
            @click="panel = panel === 'caps' ? 'none' : 'caps'"
          >
            caps
          </button>
          <span
            v-if="masked.length"
            class="font-mono text-[11px] text-sand"
            title="This session was negotiated with a mask"
          >
            masked
          </span>
          <button
            class="tbtn"
            type="button"
            title="Theater (t)"
            aria-label="Theater"
            :disabled="blocked"
            :class="props.mode === 'theater' && 'text-teal'"
            @click="emit('mode', props.mode === 'theater' ? 'window' : 'theater')"
          >
            <Icon :name="props.mode === 'theater' ? 'window' : 'theater'" :size="15" />
          </button>
          <button
            class="tbtn"
            type="button"
            title="Fullscreen (f)"
            aria-label="Fullscreen"
            :disabled="blocked"
            @click="emit('mode', props.mode === 'full' ? 'window' : 'full')"
          >
            <Icon :name="props.mode === 'full' ? 'shrink' : 'expand'" :size="15" />
          </button>
        </span>
      </div>
    </div>

    <PlayerNote :hidden="blocked" />

    <div
      v-if="phase === 'gone'"
      ref="dialog"
      class="absolute inset-0 z-9 flex items-center justify-center bg-black/70"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="player-stopped"
    >
      <div class="max-w-[46ch] rounded-md border border-line bg-surface p-4">
        <h2 id="player-stopped" class="mb-2 text-[17px] font-[650]">Playback stopped</h2>
        <p class="mb-3 text-dim">{{ health.gone }}</p>
        <span class="flex gap-2">
          <Btn @click="retryByHand">Try again</Btn>
          <Btn ghost @click="emit('close')">Back to the item</Btn>
        </span>
      </div>
    </div>

    <!-- `standby !== null` is the type narrowing, not the condition — `phase`
         already decided. It holds a resume position, so 0 is a real value and
         it cannot be tested for truthiness. -->
    <div
      v-if="phase === 'standby' && health.standby !== null"
      ref="dialog"
      class="absolute inset-0 z-9 flex items-center justify-center bg-black/70"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="player-standby"
    >
      <div class="max-w-[46ch] rounded-md border border-line bg-surface p-4">
        <h2 id="player-standby" class="mb-2 text-[17px] font-[650]">Temporarily unavailable</h2>
        <p class="mb-2 text-dim">
          The machine holding this file has stopped answering. Nothing is lost — playback will pick
          up where you left off as soon as it is back.
        </p>
        <p class="mb-3 font-mono text-[12px] text-dim">
          standing by · resumes at {{ fmt(health.standby) }}
        </p>
        <!-- One way out on purpose. Any other button here would be a second
             thing to reason about while waiting for something you cannot
             influence. -->
        <Btn @click="emit('home')">Go home</Btn>
      </div>
    </div>
  </div>

  <div v-if="panel === 'caps'" class="mt-3">
    <CapabilityDebug :applying="health.restarting" :on-apply="restartWithCaps" />
    <p v-if="health.capsError" class="mt-1 font-mono text-[12px] text-warn">
      restart failed: {{ health.capsError }}
    </p>
  </div>
</template>

<style scoped>
@reference '../theme.css';

/* A black stage with everything drawn over it. The stage is the element that
   goes fullscreen, because the subtitle canvases are its children and taking
   only the <video> would strand them. */
.videobox {
  position: relative;
  /* A stacking context, so the overlays inside stay inside. `z-9` on a dialog
     is below the shell's menus (z-20) and its notices (z-30) without one — and
     with only `position: relative` the theater transform made one in that mode
     and not in the other, so the veil escaped in window mode and painted over
     whatever it happened to overlap. The number is arbitrary; having one at
     all is the point. */
  isolation: isolate;
  background: #000;
  border-radius: 6px;
  overflow: hidden;
}
/* A dead stream leaves the <video> with no intrinsic size, so it falls back to
   the 300x150 default and the player becomes a strip — survivable while the
   dialogs lived outside this box, and not now that they are inside it: a
   190px dialog in a 150px `overflow: hidden` box turns its only button into a
   scroll region. Room only when a dialog needs it. */
.videobox:has([role='alertdialog']),
/* Starting: the veil is absolutely positioned, so with no <video> yet the box
   has no height at all and `overflow: hidden` clips the spinner to nothing. */
.videobox:not(:has(video)) {
  aspect-ratio: var(--video-ratio, 16 / 9);
  /* A dialog's body is a hub error string of unbounded length, so the shape is
     a floor rather than a cage: it may grow past the ratio to fit one. */
  min-height: min(20rem, 60vh);
}
.videobox video {
  width: 100%;
  display: block;
  background: #000;
  cursor: pointer;
  /* `auto` FIRST: the intrinsic ratio wins the moment metadata lands, and the
     hub's probed geometry only decides the shape before it. Pinning the box
     instead letterboxes for the whole film when the probe and the file
     disagree, with nothing to correct it. */
  aspect-ratio: auto var(--video-ratio, 16 / 9);
  /* At 1080p in the page column the transport bar started below the fold. */
  max-height: 78vh;
}
.videobox:not(.bar-up) video {
  cursor: none;
}
.videobox:fullscreen {
  border-radius: 0;
}
.videobox:fullscreen video {
  width: 100%;
  height: 100%;
  max-height: none;
}
/* Lift the subtitle canvases clear of the bar while it is up. JASSUB and the
   image-subtitle renderer both insert their canvas next to the <video> and
   rewrite its box on every resize — but never its transform, so this survives
   them. Native cues live in the video's own shadow tree where a transform on a
   sibling cannot reach them, and move by `line` instead: three renderers, two
   levers, and the port had only one of them. */
.videobox canvas {
  transition: transform 160ms ease-out;
}
.videobox.bar-up canvas {
  transform: translateY(-46px);
}

.tbtn {
  @apply flex cursor-pointer items-center gap-0.5 rounded px-1.5 py-1 text-[12px] text-text hover:text-teal disabled:cursor-default disabled:opacity-40;
}
.tpill {
  @apply cursor-pointer rounded border border-line px-2 py-0.5 font-mono text-[11px] text-dim hover:text-text;
}
.tsel {
  @apply max-w-[190px] rounded border border-line bg-bg px-1 py-0.5 font-mono text-[11px] disabled:opacity-50;
}
/* Two layers on one track: what the pipeline has written, and where the viewer
   is inside it. A seek past the second edge restarts the pipeline, which is
   slow and worth showing. */
.seekbar {
  @apply h-1.5 w-full cursor-pointer appearance-none rounded bg-transparent disabled:cursor-default disabled:opacity-50;
  background-image: linear-gradient(
    to right,
    var(--color-teal) var(--played),
    rgb(255 255 255 / 0.25) var(--played) var(--made),
    rgb(255 255 255 / 0.18) var(--made)
  );
}
.seekbar::-webkit-slider-thumb {
  @apply h-3 w-3 appearance-none rounded-full bg-teal;
}
.seekbar::-moz-range-thumb {
  @apply h-3 w-3 rounded-full border-0 bg-teal;
}
</style>
