<script setup lang="ts">
/// Queue playback for a record (HUB-27): one direct-play session per track,
/// auto-advance when one ends, prev/next, and a list you can jump about in.
/// The <audio> element streams with the media cookie.
///
/// GAPLESS (HUB-19) is why there are TWO elements. Preparing the next track
/// only once the current one ends costs a session round trip plus however long
/// the element needs to buffer — audible on every track boundary, and worst
/// exactly where it matters, on a record mixed to run continuously. So the idle
/// element gets the next track's session and buffers it while the current one
/// plays, and `ended` is just a play() on something already loaded.
///
/// It renders OUTSIDE the router on purpose: the queue survives navigation,
/// which is the whole point of a queue, and this is the thing that must not be
/// unmounted when the page under it changes.
import { computed, nextTick, onBeforeUnmount, reactive, ref, useTemplateRef, watch } from 'vue'

import Icon from './Icon.vue'
import type { StartSessionResponse } from '../api/generated/model/startSessionResponse.ts'
import { endSession, postProgress, startSession } from '../api/generated/kahawai.ts'
import { isSessionGone, mayRecover, startCeiling } from '../domain/recovery.ts'
import { keepSessionAlive } from '../domain/keepalive.ts'
import { replayGainFactor } from '../domain/queue.ts'
import { sentence } from '../domain/refusal.ts'
import { useQueue } from '../composables/queue.ts'

const props = defineProps<{
  /// One pair of ears: the video player asks for silence while it has the
  /// screen, and the queue picks up where it left off afterwards.
  paused?: boolean
}>()

/// How long to wait before asking again for a track the hub could not start. A
/// client's own pacing choice, not a mirror of anything the hub knows.
const RETRY_MS = 5000

/// How long a session start may take before it counts as failed.
///
/// `fetch` has no timeout of its own, and a request that never settles used to
/// hold a slot's claim for ever — nothing rendered an element for it, so no
/// error arrived, and the claim is what stops anything else from retrying. Our
/// own pacing choice: on a LAN a lease is milliseconds, and a hub too busy to
/// answer in fifteen seconds is one the retry timer should be waiting out.
const START_TIMEOUT_MS = 15_000

/// The lead time is a compromise between two failures. Too late and the buffer
/// is not warm; too early and the hub reaps the session it belongs to, which it
/// does after about 90 seconds of nobody reading (measured 2026-08-07: started
/// 10:00:53, "ending idle session" 10:02:23). Thirty seconds is comfortably
/// inside that and long enough to fill a buffer over a LAN.
const PRELOAD_LEAD_SECONDS = 30

const queue = useQueue()
const entries = computed(() => queue.queue.value.entries)
const at = computed(() => queue.queue.value.at)

/// A warmed session, remembered by WHICH TRACK it is for.
///
/// It used to remember the index, and that was wrong the moment the queue could
/// change under it: index 5 of one record counted as a match for index 5 of the
/// next, so the bar went on playing the old album.
///
/// The KEY is the claim: a slot holds it from the moment it asks for a session
/// until something releases it, so nothing starts a second request for the same
/// track. A failed claim is KEPT, which is what stops `timeupdate` asking again
/// on the next frame; only the retry timer drops one.
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
  session: StartSessionResponse | null
  key: string | null
  trouble: 'failed' | 'refused' | 'dead' | 'lost' | null
  error: string
  /// The pending retry for THIS slot. Each failure arms its own, which is the
  /// simplification a reactive store buys: the old client re-armed a shared
  /// effect through a counter, because an identical error string is not a state
  /// change any dependency can see.
  timer: ReturnType<typeof setTimeout> | undefined
  /// Consecutive refusals that are being waited out, and the TRACK they are
  /// counted against. Both live here rather than being reset by `release`: that
  /// runs before every attempt, including the retry timer's, so zeroing it
  /// there puts the ceiling permanently out of reach and the queue back to
  /// asking for ever. A counter tied to its track needs no resetting — it
  /// simply does not apply to the next one.
  tries: number
  triesFor: string | null
}

const blank = (): Slot => ({
  session: null,
  key: null,
  trouble: null,
  error: '',
  timer: undefined,
  tries: 0,
  triesFor: null,
})
const slots = reactive<[Slot, Slot]>([blank(), blank()])
const active = ref<0 | 1>(0)
const other = computed<0 | 1>(() => (1 - active.value) as 0 | 1)

const first = useTemplateRef<HTMLAudioElement>('first')
const second = useTemplateRef<HTMLAudioElement>('second')
const elementOf = (which: 0 | 1) => (which === 0 ? first.value : second.value)

const position = ref(0)
const duration = ref(0)
const stopped = ref(false)
const listing = ref(false)

/// Where to put the playhead once a recovered session's element loads, and
/// WHICH TRACK that position belongs to. A bare number outlived the track it was
/// measured on: recover at 0:42, jump to another track before the new session
/// arrives, and the jumped-to track started 42 seconds in — or ended at once, if
/// it was shorter than that.
let resumeAt: { key: string; at: number } | null = null

function release(slot: Slot, keepalive = false) {
  clearTimeout(slot.timer)
  slot.timer = undefined
  // Stopped by hand. Both elements always exist now — which is what stops the
  // gapless handover swapping to one Vue has just created — so releasing a
  // session REMOVES the `src` attribute from an element that may still be
  // playing, and the media load algorithm is only defined to run when `src` is
  // set or changed. The reference unmounted the element, which stopped it.
  const which = slots.indexOf(slot)
  if (which !== -1) elementOf(which as 0 | 1)?.pause()
  // keepalive: the page may be closing, and an unsent DELETE leaves a session
  // for the reaper.
  if (slot.session) void endSession(slot.session.session_id, { keepalive }).catch(() => {})
  slot.session = null
  slot.key = null
  slot.trouble = null
  slot.error = ''
}

/// Give a slot the session for `want`, unless it already has it.
async function prepare(which: 0 | 1, want: number) {
  const slot = slots[which]
  const track = entries.value[want]?.track
  if (!track || slot.key === track.id) return
  release(slot)
  slot.key = track.id
  slot.trouble = null
  try {
    const session = await startSession(
      { item_id: track.id, mode: 'direct' },
      { signal: AbortSignal.timeout(START_TIMEOUT_MS) },
    )
    // The queue may have moved on while the hub answered — or another attempt
    // for this same track may have got there first, which the key alone cannot
    // tell apart: recovery drops a claim without knowing whether a request is
    // already out on it, and the loser used to overwrite `session` and leak the
    // one it replaced.
    //
    // The second half of that condition is UNREACHABLE as this component is
    // built, and stays: both death detectors need a session, and the first one
    // to act releases it, so a second request cannot be started while one is
    // out. That is an ordering between a watcher, an interval and a DOM event
    // — none of which this function controls — and the cost of being wrong is
    // a session pinged for half an hour against a per-user cap of four.
    if (slots[which].key !== track.id || slots[which].session) {
      void endSession(session.session_id).catch(() => {})
      return
    }
    slots[which].session = session
    slots[which].error = ''
    slots[which].tries = 0
    slots[which].triesFor = null
  } catch (cause) {
    if (slots[which].key !== track.id) return
    // The claim is KEPT, and the retry timer below is the only thing that drops
    // it. Giving it back here is what let `timeupdate` ask again on the very
    // next frame — four times a second while a host was away.
    //
    // Worth asking again only when the answer could change, and the STATUS says
    // which: `retry` is the app's one rule for that. The queue used to count
    // attempts and give up after three, because the hub answered 409 both for
    // "this item has no sources" and for "too many concurrent streams" — one
    // status for a permanent refusal and a self-clearing one. They are now
    // `unplayable` (409) and `session_cap` (429), so the guess is gone with the
    // ambiguity that forced it: a stream cap waits as long as it takes, and an
    // unplayable track stops asking at once.
    const slot_ = slots[which]
    if (slot_.triesFor !== track.id) {
      slot_.triesFor = track.id
      slot_.tries = 0
    }
    slot_.tries += 1
    // A ceiling that is not `null` is a condition that might be permanent, or
    // one this very queue is causing — see `startCeiling`.
    const ceiling = startCeiling(cause)
    const again = ceiling === null || slot_.tries < ceiling
    slot_.trouble = again ? 'failed' : 'refused'
    slot_.error = sentence(cause)
    if (again) arm(which)
  }
}

/// Keep asking, the way the video player waits out an absent mediahost (UI-19).
/// Without this a failed prepare was terminal for the queue: the host coming
/// back changed nothing, because nothing was still looking.
///
/// For BOTH slots. The idle one had no timer at all: its only driver was
/// `timeupdate`, so a failed preload was retried on every frame of the current
/// track and then never again once that track ended.
function arm(which: 0 | 1) {
  clearTimeout(slots[which].timer)
  slots[which].timer = setTimeout(() => {
    slots[which].timer = undefined
    // Drop the claim: `prepare` no-ops while a slot still holds one, which is
    // exactly what keeps everything else from retrying in the meantime.
    slots[which].key = null
    slots[which].trouble = null
    void prepare(which, which === active.value ? at.value : at.value + 1)
  }, RETRY_MS)
}

// The active slot must hold the current track. It usually does already, because
// `ended` swapped to the slot that was warmed; this covers the first track and
// any jump the listener makes.
watch(
  [at, active, entries],
  () => {
    const track = entries.value[at.value]?.track
    if (!track) return
    if (slots[active.value].key !== track.id) void prepare(active.value, at.value)
  },
  { immediate: true },
)

// A queue change orphans whatever the OTHER slot was warming: it is a track from
// the album you just left, and its own keepalive keeps it alive against the
// per-user session cap — four of those and a film that was playing cannot
// recover, because its restart is refused for concurrency.
//
// On the CLAIM, not the session: a request still out for a track from the record
// you just left arrives to a key that still matches, and is stored and then
// pinged for half an hour. Releasing clears the key, so the reply ends itself.
//
// `at` as well as `entries`: jumping to another track within the same queue
// leaves `entries` identical, so the warmed slot kept a session for a track
// nobody is going to play.
watch([entries, at, active], () => {
  const idle = slots[other.value]
  if (idle.key && idle.key !== entries.value[at.value + 1]?.track.id) release(idle)
})

/// A direct-play element stops fetching the moment it has the whole file, which
/// for a FLAC is a minute or two into a track — so without a ping the reaper
/// ends the session under a track that is still playing, and the progress post
/// and DELETE at `ended` both 404. Measured 2026-08-07: track 2 of an album
/// reaped 3½ minutes into being audible.
///
/// The preloaded session has finished fetching and reads nothing more, so a
/// pause while it is hot would let the reaper take it before it is ever heard
/// and the swap would land on a dead URL. A position that never moves is exactly
/// what `keepSessionAlive` already handles.
function keepAlive(which: 0 | 1) {
  const slot = slots[which]
  const session = slot.session
  if (!session) return undefined
  const audible = () => which === active.value
  return keepSessionAlive(
    () => (audible() ? (elementOf(which)?.currentTime ?? 0) * 1000 : 0),
    (ms) =>
      void postProgress(session.session_id, { position_ms: Math.round(ms) }).catch((cause) => {
        if (!isSessionGone(cause)) return
        void recover(
          which,
          audible() ? at.value : at.value + 1,
          audible() ? (elementOf(which)?.currentTime ?? 0) : 0,
          // Judged by the AUDIBLE element: warming the next track while the
          // queue sits paused is the same waste.
          elementOf(active.value)?.paused ?? false,
        )
      }),
  )
}

/// One watcher per slot, because they are two independent clocks. Rebuilding
/// both whenever either session changed reset the AUDIBLE slot's stall counter
/// every time a preload landed — so a queue paused with a retrying preload beside
/// it bought itself a fresh half hour indefinitely, which is the whole thing
/// `IDLE_LIMIT_MS` exists to stop.
const pings: [(() => void) | undefined, (() => void) | undefined] = [undefined, undefined]
for (const which of [0, 1] as const) {
  watch(
    () => slots[which].session,
    () => {
      pings[which]?.()
      pings[which] = keepAlive(which)
    },
    { immediate: true },
  )
}

/// The hub answered 404: this session is gone and no ping will bring it back. A
/// direct-play music session is cheap to rebuild — a lease, not a pipeline — so
/// take a fresh one and put the playhead back where it was. Nothing here knows
/// how long a session may idle; the 404 is the entire trigger.
///
/// Not while the queue is paused: a restart there spends a lease on audio nobody
/// is listening to, and the fresh session goes idle and is reaped in turn — a
/// paused queue would respawn one for ever. The death is remembered instead and
/// acted on when play is pressed.
async function recover(which: 0 | 1, want: number, seconds: number, isPaused: boolean) {
  if (isPaused) {
    slots[which].trouble = 'dead'
    return
  }
  const track = entries.value[want]?.track
  if (!mayRecover(track?.id ?? 'queue', seconds * 1000, performance.now())) {
    // Not released, and not retried: the slot keeps a session the hub has
    // forgotten precisely so that the claim stops anything asking again — which
    // is what `mayRecover` just refused. Marked so the handover cannot pick it
    // up, because a dead preload would otherwise become the audible track.
    slots[which].trouble = 'lost'
    slots[which].error = 'playback session ended and could not be restarted'
    return
  }
  // Only the audible slot has a position worth restoring, and a preload
  // recovering at 0 would otherwise wipe one the active slot was waiting to use.
  if (seconds > 0 && track) resumeAt = { key: track.id, at: seconds }
  // `prepare` no-ops when the slot already claims this track, and it does —
  // with a session the hub has forgotten. Drop the claim.
  slots[which].key = null
  await prepare(which, want)
}

/// ReplayGain rides in a Web Audio gain node rather than the element's volume,
/// because volume is the LISTENER's: setting it here would fight the slider on
/// every track change, and it cannot go above 1.0 for the tracks whose gain is
/// positive. Both elements feed the same node.
let audio: { ctx: AudioContext; gain: GainNode } | null = null
const wired = new WeakSet<HTMLAudioElement>()

const factor = computed(() =>
  replayGainFactor(entries.value[at.value]?.track, entries.value[at.value]?.gain ?? 'album'),
)

function level() {
  const Ctor =
    window.AudioContext ??
    (window as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext
  // No Web Audio: play unlevelled rather than not at all.
  if (!Ctor) return
  if (!audio) {
    const ctx = new Ctor()
    audio = { ctx, gain: ctx.createGain() }
    audio.gain.connect(ctx.destination)
  }
  // A source node can only ever be created ONCE per element, so each element is
  // wired the first time it plays and never again.
  for (const element of [first.value, second.value]) {
    if (element && !wired.has(element)) {
      audio.ctx.createMediaElementSource(element).connect(audio.gain)
      wired.add(element)
    }
  }
  // Autoplay policy suspends a context created before a gesture, and BOTH
  // elements feed it — so while it is suspended there is no sound at all. Tried
  // again on every position report rather than only when a session changes:
  // `resume()` is a no-op on a running context, and the alternative is a dock
  // that is silent with no way back.
  if (audio.ctx.state === 'suspended') void audio.ctx.resume().catch(() => {})
  audio.gain.gain.value = factor.value
}
watch([factor, active, () => slots[0].session, () => slots[1].session], () => level(), {
  flush: 'post',
})

onBeforeUnmount(() => {
  pings.forEach((stop) => stop?.())
  for (const slot of slots) release(slot, true)
  void audio?.ctx.close()
})

/// Where the queue goes when a track ends, or when Next is pressed. Past the
/// last track it stops rather than wrapping: a record that has finished has
/// finished.
function move(by: 1 | -1) {
  if (!queue.step(by)) queue.clear()
}

const rows = useTemplateRef<HTMLElement>('rows')

/// UI-2, and where the focus goes afterwards.
///
/// The row holding the focused button is unmounted, so without this the focus
/// falls to `body`: a keyboard user is returned to the top of the document and
/// a screen reader is told nothing at all. It moves to whatever takes that
/// row's place, or to the last row when the last one goes.
///
/// Read out of the DOM rather than from a `v-for` ref array, which Vue does not
/// promise is in source order — and being one out here means removing the wrong
/// row next.
async function drop(index: number) {
  const had = entries.value.length
  queue.remove(index)
  await nextTick()
  if (had === 1) return
  const left = rows.value?.querySelectorAll<HTMLElement>('[data-remove]')
  left?.[Math.min(index, left.length - 1)]?.focus()
}

function togglePause() {
  const element = elementOf(active.value)
  if (!element) return
  if (element.paused) void element.play()
  else element.pause()
}

// Silence on request, and back to whatever it was doing afterwards — resuming
// something the listener had paused themselves would be the player deciding for
// them.
let wasPlaying = false
watch([() => props.paused, active], () => {
  const element = elementOf(active.value)
  if (!element) return
  if (props.paused) {
    wasPlaying = !element.paused
    element.pause()
  } else if (wasPlaying) {
    wasPlaying = false
    void element.play()
  }
})

/// The current track is nearly over: warm the other slot.
function onTime(which: 0 | 1) {
  if (which !== active.value) return
  const element = elementOf(which)
  if (!element) return
  position.value = element.currentTime
  duration.value = Number.isFinite(element.duration) ? element.duration : 0
  level()
  if (!Number.isFinite(element.duration)) return
  if (element.duration - element.currentTime > PRELOAD_LEAD_SECONDS) return
  const next = at.value + 1
  if (next < entries.value.length) void prepare(other.value, next)
}

/// Hand over to the slot that has been buffering. No session start, no load: the
/// next track is already there.
function onEnded(which: 0 | 1) {
  if (which !== active.value) return
  const finished = slots[which]
  const element = elementOf(which)
  if (finished.session && element) {
    void postProgress(finished.session.session_id, {
      position_ms: Math.round(element.duration * 1000),
    }).catch(() => {})
  }
  const next = at.value + 1
  const warmed = other.value
  if (next >= entries.value.length) {
    queue.clear()
    return
  }
  release(finished)
  // The warmed slot is the one already holding the next TRACK — with a session.
  // Keeping a failed claim made "claimed but unplayable" reachable for the
  // first time, and matching on the key alone handed playback to a slot with no
  // element at all: no audio, and the watcher above declining to help because
  // the key it wanted was already claimed.
  //
  // The KEY comparison restates what the orphan watcher above maintains, so no
  // test can reach it: by the time this runs, that watcher has already released
  // any slot not holding `at + 1`. It stays because it is the invariant this
  // line depends on, and the watcher's flush order relative to a DOM event is
  // not something this function owns.
  if (
    slots[warmed].session &&
    !slots[warmed].trouble &&
    slots[warmed].key === entries.value[next]?.track.id
  ) {
    active.value = warmed
    queue.jump(next)
    void elementOf(warmed)?.play()
    return
  }
  // The warm-up did not happen (a very short track, or a slow hub): fall back to
  // loading in place, which is what this used to do.
  queue.jump(next)
}

/// Either slot can have died while the queue sat paused, and which one it was
/// matters. One shared flag rebuilt the ACTIVE session whatever had actually
/// gone, so a dead preload stayed dead and the handover at `ended` landed on a
/// URL the hub had forgotten.
function onPlay(which: 0 | 1) {
  if (which !== active.value) return
  stopped.value = false
  level()
  for (const w of [0, 1] as const) {
    if (slots[w].trouble !== 'dead') continue
    const audible = w === active.value
    void recover(
      w,
      audible ? at.value : at.value + 1,
      audible ? (elementOf(w)?.currentTime ?? 0) : 0,
      false,
    )
  }
}

/// The element reports a failure with no status of its own, so ask the hub what
/// kind it was — 404 means the session went away, anything else is a real media
/// fault.
function onError(which: 0 | 1) {
  const session = slots[which].session
  if (!session) return
  void postProgress(session.session_id, { position_ms: 0 }).catch((cause) => {
    if (!isSessionGone(cause)) return
    const audible = which === active.value
    void recover(
      which,
      audible ? at.value : at.value + 1,
      audible ? (elementOf(which)?.currentTime ?? 0) : 0,
      elementOf(active.value)?.paused ?? false,
    )
  })
}

/// A recovered session streams the same file from the top; put the playhead back
/// where the dead one left off — and only onto the track it was measured on.
function onLoaded(which: 0 | 1) {
  if (which !== active.value || !resumeAt) return
  if (slots[which].key !== resumeAt.key) {
    // Dropped, not kept. Left standing it outlived the track it was measured
    // on: choosing that track from the list later started it 42 seconds in.
    resumeAt = null
    return
  }
  const element = elementOf(which)
  if (element) element.currentTime = resumeAt.at
  resumeAt = null
}

// The audible slot's failure, and only that one.
const failure = computed(() => slots[active.value].error)
const track = computed(() => entries.value[at.value]?.track)
const pct = computed(() =>
  duration.value > 0 ? Math.min(100, (position.value / duration.value) * 100) : 0,
)
/// Music always plays direct (HUB-19): no pipeline, just the file.
const how = computed(() => {
  const type = slots[active.value].session?.content_type?.split('/').pop()
  return type ? `direct · ${type}` : 'direct'
})
const sub = computed(() =>
  [track.value?.artist, track.value?.parent_title].filter(Boolean).join(' · '),
)
</script>

<template>
  <aside
    class="fixed right-0 bottom-0 left-0 z-20 border-t border-line bg-surface"
    aria-label="Playback queue"
  >
    <!-- The list, above the bar rather than over it: it is what the bar is
         about, and a popover would cover the transport that opened it. -->
    <div
      v-if="listing"
      id="queue-list"
      class="max-h-[40vh] overflow-y-auto border-b border-hairline"
    >
      <div class="flex items-center gap-3 px-4 py-2">
        <span id="queue-list-head" class="font-mono text-[11px] text-dim">
          QUEUE · {{ entries.length }}
        </span>
        <button
          class="ml-auto cursor-pointer font-mono text-[11px] text-dim hover:text-warn"
          type="button"
          @click="queue.clear()"
        >
          clear
        </button>
      </div>
      <ul ref="rows" role="list" aria-labelledby="queue-list-head">
        <li
          v-for="(entry, index) in entries"
          :key="`${entry.track.id}-${index}`"
          class="flex items-center hover:bg-hover"
        >
          <button
            class="flex flex-1 cursor-pointer items-center gap-3 py-1.5 pl-4 text-left"
            :class="index === at && 'text-teal'"
            type="button"
            :aria-current="index === at ? 'true' : undefined"
            :title="`Play from ${entry.track.title}`"
            @click="queue.jump(index)"
          >
            <!-- The playing row is marked rather than numbered: which one it is
                 matters more than where it sits. -->
            <span class="w-6 font-mono text-[11px] text-dim">
              {{ index === at ? '▶' : index + 1 }}
            </span>
            <span class="flex-1 truncate">{{ entry.track.title }}</span>
            <span class="font-mono text-[11px] text-dim">{{ entry.track.artist ?? '' }}</span>
          </button>
          <!-- UI-2. Taking out the track that is PLAYING moves to whatever
               takes its place, which is the next one; taking out one before it
               changes nothing you can hear. -->
          <button
            data-remove
            class="cursor-pointer px-4 py-1.5 text-dim hover:text-warn"
            type="button"
            :aria-label="`Remove ${entry.track.title} from the queue`"
            title="Remove from the queue"
            @click="drop(index)"
          >
            ✕
          </button>
        </li>
      </ul>
    </div>

    <div
      class="mx-auto flex max-w-[1200px] flex-wrap items-center gap-3 px-[clamp(14px,4vw,32px)] py-2"
    >
      <button
        class="cursor-pointer p-1 text-dim hover:text-text disabled:opacity-40"
        type="button"
        title="Previous"
        aria-label="Previous track"
        :disabled="at === 0"
        @click="move(-1)"
      >
        <Icon name="prev" />
      </button>
      <button
        class="cursor-pointer rounded-full border border-line p-2 hover:border-teal-dim"
        type="button"
        :title="stopped ? 'Play' : 'Pause'"
        :aria-label="stopped ? 'Play' : 'Pause'"
        @click="togglePause"
      >
        <Icon :name="stopped ? 'play' : 'pause'" />
      </button>
      <button
        class="cursor-pointer p-1 text-dim hover:text-text"
        type="button"
        title="Next"
        aria-label="Next track"
        @click="move(1)"
      >
        <Icon name="next" />
      </button>

      <!-- The one thing on this bar that changes without anybody pressing
           anything: a record advances a track every few minutes, silently. -->
      <span class="flex min-w-[160px] flex-col" role="status" aria-live="polite">
        <span class="truncate text-[14px] font-semibold">{{ track?.title ?? '' }}</span>
        <span class="truncate font-mono text-[11px] text-dim">{{ sub }}</span>
      </span>

      <!-- Deliberately no `role="progressbar"`. It cannot be seeked, so the
           role offers nothing to do — and its `aria-valuenow` would change
           about a hundred times a track, which NVDA reports with a beep each
           time. A record would play to a beep every couple of seconds. -->
      <span class="h-1 flex-1 overflow-hidden rounded bg-line" aria-hidden="true">
        <span class="block h-full bg-teal" :style="{ width: `${pct}%` }" />
      </span>

      <span class="font-mono text-[11px] text-dim">{{ how }}</span>
      <button
        class="cursor-pointer rounded border px-2 py-0.5 font-mono text-[11px]"
        :class="listing ? 'border-teal text-teal' : 'border-line text-dim'"
        type="button"
        :aria-expanded="listing"
        aria-controls="queue-list"
        @click="listing = !listing"
      >
        queue {{ entries.length }}
      </button>
      <button
        class="cursor-pointer p-1 text-dim hover:text-warn"
        type="button"
        title="Stop and clear"
        aria-label="Stop and clear the queue"
        @click="queue.clear()"
      >
        ✕
      </button>

      <!-- Both elements always exist. Rendering only the one with a session
           made the pair's identity depend on which of them had one, so the
           gapless handover swapped to an element that had just been created and
           had loaded nothing. -->
      <audio
        ref="first"
        :src="slots[0].session?.stream_url"
        :autoplay="active === 0 && !props.paused"
        preload="auto"
        :hidden="active !== 0"
        @play="onPlay(0)"
        @playing="stopped = false"
        @pause="stopped = true"
        @timeupdate="onTime(0)"
        @ended="onEnded(0)"
        @loadedmetadata="onLoaded(0)"
        @error="onError(0)"
      />
      <audio
        ref="second"
        :src="slots[1].session?.stream_url"
        :autoplay="active === 1 && !props.paused"
        preload="auto"
        :hidden="active !== 1"
        @play="onPlay(1)"
        @playing="stopped = false"
        @pause="stopped = true"
        @timeupdate="onTime(1)"
        @ended="onEnded(1)"
        @loadedmetadata="onLoaded(1)"
        @error="onError(1)"
      />
    </div>

    <!-- Under the bar, not in it: the bar is a row of controls, and an error
         wedged between them pushed the transport around and got truncated. -->
    <p
      class="mx-auto max-w-[1200px] px-[clamp(14px,4vw,32px)] pb-2 text-warn empty:pb-0"
      role="alert"
    >
      {{ failure }}
    </p>
  </aside>
</template>
