<script setup lang="ts">
/// The player as a page: everything between a `/play` URL and a picture.
///
/// Acquiring a session belongs HERE rather than to the item page, which is what
/// makes `/play` an address of its own rather than an instruction to that page.
/// Deep-linking used to show the details for a second — Play button and all —
/// before swapping; browser-back landed on the home screen because no item
/// entry ever existed; and the same refusal rendered two ways depending on
/// which button you pressed. All three were one cause: a route carrying objects
/// no URL can reconstruct.
///
/// The session lives here rather than in the picture so that a restart still
/// REMOUNTS the picture — it keeps a run's worth of state and expects a fresh
/// mount per session — while the route stays the same page.
import { computed, onBeforeUnmount, ref, watch } from 'vue'
import { useQueryClient } from '@tanstack/vue-query'
import { useRoute, useRouter } from 'vue-router'

import Btn from '../components/Btn.vue'
import Failed from '../components/Failed.vue'
import Picture from '../components/Picture.vue'
import type { ItemQueryResponse } from '../api/generated/model/itemQueryResponse.ts'
import type { CarriedTracks } from '../domain/player-tracks.ts'
import type { PlayerMode } from '../domain/player-keys.ts'
import type { Preference } from '../api/generated/model/preference.ts'
import type { StartSessionResponse } from '../api/generated/model/startSessionResponse.ts'
import { buildProfile } from '../api/capabilities.ts'
import { endSession, getPrefs, itemQuery, listLibraries } from '../api/generated/kahawai.ts'
import { notify } from '../composables/notices.ts'
import { resolveTracks } from '../domain/tracks.ts'
import { sentence } from '../domain/refusal.ts'
import { isSourceOffline } from '../domain/recovery.ts'
import { itemName } from '../domain/titles.ts'
import { useScreenName } from '../composables/title.ts'
import { startPlaybackSession } from '../api/playback.ts'

const route = useRoute()
const router = useRouter()
const cache = useQueryClient()

const id = computed(() => String(route.params.id ?? ''))
const library = computed(() => String(route.params.library ?? ''))
/// Where to start, in milliseconds, instead of resuming. A hint from what was
/// pressed — "Play from start" is zero and a chapter is its own position — and
/// a bare URL always resumes, which is the safe default. Anything that is not
/// a position (a stale link, a typo) resumes rather than jumping somewhere
/// nobody asked for.
const startAt = computed(() => {
  // Digits only: Number('') is 0, so a truncated ?start= would silently
  // mean "from the beginning" where the stated rule is "anything that is
  // not a position resumes".
  const asked = route.query.start
  return typeof asked === 'string' && /^\d+$/.test(asked) ? Number(asked) : null
})

const item = ref<ItemQueryResponse | null>(null)
/// Read once, on the way in, and handed to the picture: the preferences that
/// chose the audio track and the media type that shaped that choice. The
/// picture used to fetch both again to draw its selectors with, which is the
/// same two answers a moment later.
const prefs = ref<Preference[]>([])
const mediaType = ref('')
const session = ref<StartSessionResponse | null>(null)
const resumeMs = ref(0)
/// The track choice live at the moment of the last restart, handed back to
/// the remounted picture so a recovery does not revert a mid-episode pick to
/// the prefs snapshot above. Null on a first mount and cleared on any change
/// of item: a choice carries across restarts of one viewing, never across
/// episodes.
const carried = ref<CarriedTracks | null>(null)
const failure = ref('')
const attempt = ref(0)

/// The item THIS address is for. `item` outlives a change of id — it is a
/// plain ref that `start` overwrites a round trip later — and browser-back
/// between two `/play` entries left the heading and the tab strip naming the
/// episode you had just left, which is a worse answer to "where am I" than no
/// answer at all.
const naming = computed(() => (item.value?.id === id.value ? item.value : null))

/// UI-17. What this screen is called, for the heading and for the tab strip —
/// which is also the only thing that tells a screen reader the screen changed.
/// Never blank: a heading is the answer to "where am I", and "" is not one.
const heading = computed(() => {
  if (naming.value) return itemName(naming.value)
  return failure.value ? 'Could not start playback' : 'Starting playback'
})
/// Not the heading: "Starting playback" is a state, not a name, and publishing
/// it would spend this screen's one announcement before there is anything to
/// announce. A failure is not a state it grows out of, so that one is published.
useScreenName(
  computed(() => {
    if (naming.value) return itemName(naming.value)
    return failure.value ? 'Could not start playback' : null
  }),
)

/// The frame's own state, because the frame is the thing that persists. The
/// picture decides it and is replaced whenever the session is; this element is
/// not, which is the whole point — the window, and the way out of it, must not
/// blink when what is behind them is rebuilt.
const mode = ref<PlayerMode>('window')

/// A session started after the viewer left is one nobody will play, ping or
/// end.
let left = false
onBeforeUnmount(() => {
  left = true
  const open = session.value
  if (open) void release(open.session_id)
  // Whatever the watcher below still owed: its queued post-flush job is
  // SKIPPED once the watcher is stopped, and a navigation landing in the
  // same flush unmounts this component — and stops the watcher — before
  // the job runs. Without this drain those sessions held one of the
  // account's four slots each until the idle reaper.
  for (const id of owed) void release(id)
  owed.clear()
})

const release = (id: string) => endSession(id, { keepalive: true }).catch(() => {})

/// Sessions retired but not yet released: the release rides on the
/// post-flush watcher below, and this is the ledger that survives the one
/// case where that job never runs (unmount in the same flush).
const owed = new Set<string>()

/// The ONLY writer of `session`: records what the assignment retires, so
/// the watcher — or, failing that, unmount — releases it.
function retire(next: StartSessionResponse | null) {
  const open = session.value
  if (open && open.session_id !== next?.session_id) owed.add(open.session_id)
  session.value = next
}

/// A RETIRED session is released here, after the picture has let go of it.
///
/// Three call sites used to do it by hand and a fourth — `start`, reached by a
/// route-param change — did not: Back and Forward across two `/play` entries
/// reuse this component with a new id, so each pass overwrote `session` and
/// left a live one nobody could reach. Four of those and the account is at its
/// per-user cap, and the fifth start is refused.
///
/// After the picture has gone, too. The picture posts the final position in its
/// teardown, and the hand-written releases ran BEFORE the reassignment that
/// unmounts it — so the report went to a session the route had just ended.
/// `post`, so the picture holding the old session has already been torn down —
/// and with it the final progress report, which is the picture's job. Pre-flush
/// is the default, and it beat the unmount: the report went to a session the
/// route had ended a moment earlier.
watch(
  session,
  (fresh, old) => {
    if (old && old.session_id !== fresh?.session_id) {
      owed.delete(old.session_id)
      void release(old.session_id)
    }
    // Two retires in one flush coalesce to a single watcher run whose `old`
    // is only the first of them; whatever the ledger still holds beyond the
    // session now on screen was retired mid-flush and is nobody's.
    for (const id of owed) {
      if (id !== fresh?.session_id) {
        owed.delete(id)
        void release(id)
      }
    }
  },
  { flush: 'post' },
)

async function start() {
  // Cleared BEFORE the guard, or Try again cannot clear a failure that was not
  // the session's.
  failure.value = ''
  // Already playing this item — the next-episode handover sets both at once,
  // and the URL catching up must not start a second session for it. The bump
  // is not optional: a start for a route the viewer has already Backed out
  // of can still be in flight, and without it that start finishes with
  // `mine === attempt` and puts the other item's stream under this URL.
  if (session.value && item.value?.id === id.value) {
    attempt.value++
    return
  }
  const mine = ++attempt.value
  try {
    const detail = await itemQuery(id.value, { profile: buildProfile() })
    if (mine !== attempt.value || left) return
    // The item and the session render as a PAIR. A session still alive here
    // belongs to the item this route just left (Back/Forward reuse this
    // component); assigning the new item beside it rendered one item's
    // metadata around another's stream for as long as the start took.
    retire(null)
    item.value = detail
    // Range-checked against the file, not only shape-checked: a stale or
    // hand-edited position past the end is not a position, so it resumes.
    const asked =
      startAt.value !== null && (detail.duration_ms == null || startAt.value < detail.duration_ms)
        ? startAt.value
        : null
    const at = asked ?? detail.resume_position_ms ?? 0
    const audio = detail.sources[0]?.streams?.audio ?? []
    let audioTrack = 0
    prefs.value = []
    carried.value = null
    try {
      const [preferences, libraries] = await Promise.all([
        // NOT from the query cache, unlike the library list below: `putPref`
        // writes through without invalidating it, and the write that matters
        // most here is the one this player just made — pick Japanese on
        // episode one, start episode two, and a cached list still says English.
        getPrefs(),
        // Whatever screen you came from already read this — every route into
        // the player is one of them — and libraries change when an admin edits
        // one, not while somebody is starting an episode. Read, not
        // `ensureQueryData`: that would put this call under the cache's retry
        // policy, and a failing one would hold up the session start for as
        // long as it retried.
        //
        // Guarded: `prefs` is assigned after this await, so a rejection here
        // left it `[]` — and `[]` is not nullish, so the preferences that DID
        // arrive were replaced by nothing. That drops the bandwidth cap
        // silently and starts on track 0, which is the anime-in-English bug.
        // The media type is the only thing actually at stake.
        cache.getQueryData<{ libraries: { id: string; media_type: string }[] }>(['libraries']) ??
          listLibraries().catch((cause: unknown) => {
            notify(`Could not load the library details: ${sentence(cause)}`)
            return { libraries: [] }
          }),
      ])
      prefs.value = preferences.prefs
      mediaType.value = libraries.libraries.find((l) => l.id === library.value)?.media_type ?? ''
      audioTrack = resolveTracks(
        prefs.value,
        detail.parent_id ?? detail.id,
        detail.id,
        mediaType.value,
        detail.metadata?.original_language,
        audio,
      ).audioTrack
    } catch (cause) {
      // Both halves report and fall back, so this is `resolveTracks` itself — a
      // bug rather than an outage. Said out loud, because the track it failed
      // to pick is the one about to play.
      notify(`Could not resolve the audio track: ${sentence(cause)}`)
    }
    const fresh = await startPlaybackSession(detail, at, audioTrack, 0, prefs.value)
    if (mine !== attempt.value || left) {
      void release(fresh.session_id)
      return
    }
    resumeMs.value = at
    retire(fresh)
    // The start position is spent only NOW, with the session up: an hour in,
    // a reload must resume from progress rather than jump back to the
    // chapter that opened the session — but a start that FAILED must keep
    // the ask, or Try again after a transient 503 silently resumed mid-film
    // instead of at the chapter that was pressed. replace(), so Back does
    // not walk through the parameter either.
    if (asked !== null) {
      void router.replace({ query: {} })
    }
  } catch (cause) {
    if (mine !== attempt.value || left) return
    // Whatever session this route was holding, it is not the one on screen any
    // more: the guard above reads `session && item?.id === id`, so a failure
    // that leaves the OLD session beside the NEW item makes Try again return
    // early and hand the picture one item's metadata over another's stream.
    retire(null)
    // 503 and nothing else. `startCeiling` also returns `null` for a request
    // that got no answer at all and for a gateway status, and telling somebody
    // whose wifi is off that the machine holding the file is not answering
    // points at the wrong machine.
    failure.value = isSourceOffline(cause)
      ? 'The machine holding this file is not answering. Try again in a moment.'
      : sentence(cause)
  }
}

watch(id, () => void start(), { immediate: true })

function leave() {
  void router.push({ name: 'detail', params: { library: library.value, id: id.value } })
}

/// QUERY carries the geometry of the exact source negotiation chose, so the
/// box is the right shape before the first media byte.
const ratio = computed(() => {
  const source = item.value?.negotiated?.source
  return source?.display_width && source.display_height
    ? `${source.display_width} / ${source.display_height}`
    : '16 / 9'
})

/// A restart replaces the session in place: same page, same frame, new picture.
/// The watcher above releases the one it replaced, after the picture holding it
/// has reported where the viewer got to.
function restarted(from: string, fresh: StartSessionResponse, at: number, choice: CarriedTracks) {
  // A picture the route has already left behind can finish its restart late;
  // adopting that session would put the previous episode's stream under this
  // item's page. Release it instead — nobody else holds it. BOTH checks:
  // Back/Forward moves `id` before `item` catches up, while an autoplay
  // advance moves `item` before the route commits — a stale restart landing
  // in either gap matches the one that has not moved yet.
  if (from !== id.value || from !== item.value?.id) {
    void release(fresh.session_id)
    return
  }
  // Any start() still in flight is now about a session nobody wants twice.
  attempt.value++
  resumeMs.value = at
  carried.value = choice
  retire(fresh)
}

/// The next episode: the URL follows it WITHOUT this component remounting and
/// throwing away the session it has already started.
function advanced(
  from: string,
  nextItem: ItemQueryResponse,
  fresh: StartSessionResponse,
  nextPrefs: Preference[],
) {
  // Same stale-picture guard as `restarted`.
  if (from !== id.value || from !== item.value?.id) {
    void release(fresh.session_id)
    return
  }
  attempt.value++
  item.value = nextItem
  // The picture read these to resolve the next episode's tracks; adopting them
  // keeps the remounted one from drawing its selectors off a staler set.
  prefs.value = nextPrefs
  carried.value = null
  resumeMs.value = 0
  retire(fresh)
  // Replaces the entry rather than stacking one: browser-back should leave the
  // player, not walk back through an evening's autoplay.
  void router.replace({
    name: 'player',
    params: { library: library.value, id: nextItem.id },
  })
}
</script>

<template>
  <!-- Outside the branch, because both branches are this screen. Every other
       screen has a visible heading; this one cannot, because the only thing on
       it is the picture. Heading navigation is how a screen reader user asks
       where they are, and the answer here was nothing at all — and putting it
       inside `main` took it away again the moment playback refused. -->
  <h1 class="sr-only">{{ heading }}</h1>

  <Failed
    v-if="failure"
    what="Could not start playback."
    :message="failure"
    away="Back to the item"
    @retry="start"
    @away="leave"
  />

  <!-- One frame for the whole visit: the window, and the way out of it. What
       goes inside changes — a veil while the session is being started, then the
       picture, then a different picture each time a restart replaces the
       session — and none of those swaps touches this element, so nothing about
       the page around the picture ever blinks. -->
  <!-- `tabindex="-1"` so the focus has somewhere to land: the picture is keyed
       on the session id, so every restart and every next episode destroys the
       element the focus was on and drops it to `<body>`. -->
  <main v-else :class="mode === 'theater' ? 'theater' : ''" tabindex="-1">
    <Btn v-if="mode === 'window'" ghost small class="mb-[18px]" @click="leave">← Back</Btn>

    <!-- The item's own geometry, not 16:9: the box that appears while the
         session is being started is usually the shape the picture will be, and
         the alternative is a visible jump when the video arrives. -->
    <div
      v-if="!item || !session"
      class="starting flex w-full items-center justify-center rounded-md bg-black"
      :style="{ '--video-ratio': ratio }"
      role="status"
    >
      <span class="animate-spin text-[28px] text-teal" aria-hidden="true">↻</span>
      <span class="sr-only">Starting playback</span>
    </div>
    <Picture
      v-else
      :key="session.session_id"
      :item="item"
      :session="session"
      :resume-ms="resumeMs"
      :library-id="library"
      :prefs="prefs"
      :media-type="mediaType"
      :carried="carried"
      :mode="mode"
      @mode="mode = $event"
      @close="leave"
      @home="router.push({ name: 'libraries' })"
      @restart="restarted"
      @play-next="advanced"
    />
  </main>
</template>

<style scoped>
.starting {
  aspect-ratio: var(--video-ratio, 16 / 9);
  /* The same floor the picture has, for the same reason: a failure inside this
     box is a dialog, and a dialog in a short `overflow: hidden` box turns its
     only button into a scroll region. */
  min-height: min(20rem, 60vh);
}
/* Theater is the full width of the window, which the page column is not. */
.theater {
  width: 100vw;
  max-width: 100vw;
  margin-left: calc(50% - 50vw);
}
</style>
