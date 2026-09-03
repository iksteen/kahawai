<script setup lang="ts">
/// One library, as a grid whose whole height is reserved from the first
/// answer. Only the rows on screen exist in the DOM.
///
/// Deliberately no early return for loading or for an error. Every one of them
/// swaps the whole tree, and swapping the tree destroys the search box
/// somebody is typing into. The chrome is always mounted; only what hangs
/// below it changes.
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useQuery } from '@tanstack/vue-query'

import Btn from '../components/Btn.vue'
import Card from '../components/Card.vue'
import ArtistCard from '../components/ArtistCard.vue'
import MatchDialog from '../components/MatchDialog.vue'
import PagedGrid from '../components/PagedGrid.vue'
import {
  cellsIn,
  changed,
  CHUNK,
  chunksFor,
  countLine,
  GAP,
  type Metric,
  reservedHeight,
  shapeOf,
  visibleRows,
} from '../domain/virtual.ts'
import type { ItemRowI64 } from '../api/generated/model/itemRowI64.ts'
import { listLibraries } from '../api/generated/kahawai.ts'
import { notify } from '../composables/notices.ts'
import { sentence } from '../domain/refusal.ts'
import { targetOf } from '../domain/label.ts'
import { useLibraryItems } from '../composables/library.ts'
import { useArtists } from '../composables/music.ts'
import { useScreenName } from '../composables/title.ts'
import { useSearchBox, useSearchQuery } from '../composables/search.ts'
import { whoAmI } from '../api/session.ts'

const route = useRoute()
const router = useRouter()

const library = computed(() => String(route.params.library ?? ''))
/// The header's box filters this library in place — see `useSearch`.
const query = useSearchQuery()
const sort = ref('title')
const artistSort = ref('name')

/// Resolve the media type before choosing the browse shape. Music starts at
/// artists; every other library retains the virtual item grid.
const details = useQuery({
  queryKey: ['libraries'],
  queryFn: () => listLibraries(),
  select: (r) => r.libraries,
})
watch(
  () => details.isError.value,
  (failed) =>
    failed && notify(`Could not load the library details: ${sentence(details.error.value)}`),
)
const self = computed(() => details.data.value?.find((l) => l.id === library.value))
const music = computed(() => self.value?.media_type === 'music')
const name = computed(() => self.value?.name ?? 'Library')
useScreenName(computed(() => self.value?.name ?? (details.isError.value ? name.value : null)))

const itemsEnabled = computed(
  () =>
    details.isError.value ||
    (details.data.value !== undefined && (!self.value || !music.value || query.value !== '')),
)

const { loaded, total, libraryTotal, failure, need, refresh, retry } = useLibraryItems(
  library,
  query,
  sort,
  itemsEnabled,
)
const artists = useArtists(library, query, artistSort, music)

/// HUB-8 hand-matching, from the grid: an operator finds a wrong cover by
/// LOOKING at the covers, so the affordance belongs where they are looking.
/// Only for a work — an episode inherits its show's identity — and only for an
/// admin, who is the only one the endpoint answers.
const me = whoAmI()
const matchable = (item: ItemRowI64) => me.admin && (item.kind === 'movie' || item.kind === 'show')

const matching = ref<{ item: ItemRowI64; at: number } | null>(null)

/// Pressing the library's name drops the filter, which is the other half of
/// the ✕ in the box — the heading is where somebody looks when the page is
/// showing twelve of two thousand.
const search = useSearchBox()
function clearFilter() {
  search?.clear()
}

/// Measured, never assumed: the card art's aspect ratio is applied to a fluid
/// grid column, so a row's height is a function of the window width.
const metric = ref<Metric | null>(null)
const rows = ref({ start: 0, end: 0 })
const wrap = ref<HTMLElement | null>(null)
const grid = ref<HTMLElement | null>(null)

function measure() {
  const el = grid.value
  const cell = el?.firstElementChild as HTMLElement | null
  if (!el || !cell) return
  const cols = getComputedStyle(el).gridTemplateColumns.split(' ').filter(Boolean).length
  const rowH = cell.getBoundingClientRect().height + GAP
  if (!cols || rowH <= GAP) return
  const now = { cols, rowH }
  if (changed(metric.value, now)) metric.value = now
}

function recompute() {
  if (!wrap.value || !metric.value || total.value === null) return
  const at = visibleRows(
    {
      wrapTop: wrap.value.getBoundingClientRect().top + window.scrollY,
      scrollY: window.scrollY,
      height: window.innerHeight,
    },
    metric.value,
    total.value,
  )
  if (at.start !== rows.value.start || at.end !== rows.value.end) rows.value = at
}

/// A ResizeObserver on the grid itself rather than a window resize listener.
///
/// The measurement is a function of the grid's WIDTH, and the window is not
/// the only thing that changes it: the vertical scrollbar cannot exist before
/// the first measurement — the wrapper has no height and the list is out of
/// flow, so the page does not scroll — and it appears the instant the reserved
/// height lands, narrowing every column after the only measurement that would
/// otherwise ever be taken.
let watching: ResizeObserver | undefined
function remeasure() {
  measure()
  recompute()
}
onMounted(() => {
  window.addEventListener('scroll', recompute, { passive: true })
  void nextTick(() => {
    // Once by hand, for the environment with no ResizeObserver in it. Where
    // there is one, `observe` fires it immediately and this is the same
    // measurement twice — which costs a `getComputedStyle` and is why the
    // guard below decides whether anything changed.
    remeasure()
    if (typeof ResizeObserver === 'undefined' || !grid.value) return
    watching = new ResizeObserver(remeasure)
    watching.observe(grid.value)
  })
})
onBeforeUnmount(() => {
  window.removeEventListener('scroll', recompute)
  watching?.disconnect()
})

/// Measure once there is a card to measure, and again when anything that can
/// change the answer changes. Deliberately not on every render: measuring
/// writes state, so a render-driven measurement is a cycle.
///
/// The media type is one of those inputs. It arrives a round trip after the
/// first cards, and without it a music library keeps the row pitch it measured
/// while its square sleeves were still poster-shaped — every row then
/// reserving a poster's height for a sleeve, so the reserved total ran long
/// and the last screenful was empty space.
watch([total, () => loaded.value.size > 0, () => self.value?.media_type], () =>
  nextTick(() => {
    measure()
    recompute()
  }),
)
watch([metric, total], recompute)
/// Fetch whatever the visible rows need and do not have.
watch(
  [rows, metric, total],
  () => {
    if (!metric.value || total.value === null) return
    need(chunksFor(rows.value, metric.value, total.value))
  },
  { immediate: true },
)

// A different result set: back to the top, because a scroll position in the
// old one means nothing in the new one — and then work out which rows that is.
//
// Without the recompute a re-sort left ten cards inside a full-height
// container for ever: scrolling to the top when already there fires no scroll
// event, and a re-sort changes neither the total nor the metric, so every
// other path that would have recomputed was watching something that had not
// moved.
watch([library, query, sort, artistSort], () => {
  rows.value = { start: 0, end: 0 }
  window.scrollTo({ top: 0 })
  void nextTick(recompute)
})

/// Before the first measurement there is nothing to reserve against, so the
/// first chunk is rendered plainly and the measurement is taken off it.
const cells = computed(() => {
  const count = total.value ?? 0
  if (!metric.value) return Array.from({ length: Math.min(CHUNK, count) }, (_, n) => n)
  return cellsIn(rows.value, metric.value, count)
})

const height = computed(() =>
  metric.value && total.value !== null ? reservedHeight(total.value, metric.value) : undefined,
)
const offset = computed(() =>
  metric.value ? `translateY(${rows.value.start * metric.value.rowH}px)` : undefined,
)

function open(item: Parameters<typeof targetOf>[0]) {
  void router.push({
    name: 'detail',
    params: { library: library.value, id: targetOf(item) },
  })
}

function openArtist(key: string) {
  void router.push({
    name: 'artist',
    params: { library: library.value, artist: key },
  })
}

const count = computed(() => {
  if (!music.value) return countLine(total.value, libraryTotal.value, query.value !== '')
  const artistCount = artists.total.value
  if (!query.value) return artistCount === null ? '' : `${artistCount} artists`
  const itemCount = total.value
  return [
    artistCount === null ? '' : `${artistCount} artists`,
    itemCount === null ? '' : `${itemCount} albums and songs`,
  ]
    .filter(Boolean)
    .join(' · ')
})

const pageFailure = computed(() => {
  if (!music.value) return failure.value
  return [artists.failure.value, query.value ? failure.value : ''].filter(Boolean).join(' ')
})

function retryPage() {
  if (music.value) {
    if (artists.failure.value) artists.retry()
    if (query.value && failure.value) retry()
  } else {
    retry()
  }
}
</script>

<template>
  <main>
    <!-- The wordmark opens the jump menu now, so home needs saying
         somewhere. -->
    <Btn ghost small class="mb-[18px]" @click="router.push({ name: 'libraries' })">← Home</Btn>
    <div class="mb-4 flex items-baseline gap-3">
      <!-- Pressable when there is a filter to drop, and inert when there is
           not: a heading that looks clickable and does nothing is worse than
           one that does not. -->
      <!-- Pressable when there is a filter to drop, and inert when there is
           not: a heading that looks clickable and does nothing is worse than
           one that does not. A BUTTON when it is pressable, because a heading
           with a click handler is unreachable from the keyboard — the ✕ in the
           search box is the other half of the same gesture and always was
           reachable, so this was a mouse-only shortcut. -->
      <h1 class="text-[22px] font-[650] tracking-[0.01em]">
        <button
          v-if="query !== ''"
          class="cursor-pointer hover:text-teal"
          type="button"
          :title="`Show all of ${name}`"
          @click="clearFilter"
        >
          {{ name }}
        </button>
        <template v-else>{{ name }}</template>
      </h1>
      <select
        v-if="music && query === ''"
        v-model="artistSort"
        class="ml-auto rounded-md border border-line bg-surface px-2 py-1 text-[13px]"
        aria-label="Sort"
      >
        <option value="name">Artist A–Z</option>
        <option value="-name">Artist Z–A</option>
      </select>
      <select
        v-else
        v-model="sort"
        class="ml-auto rounded-md border border-line bg-surface px-2 py-1 text-[13px]"
        aria-label="Sort"
      >
        <option value="title">Title A–Z</option>
        <option value="-title">Title Z–A</option>
        <option value="-added">Recently added</option>
        <option value="added">Oldest added</option>
        <option value="-year">Newest first</option>
        <option value="year">Oldest first</option>
      </select>
      <!-- The only feedback somebody who cannot see the grid gets that a
           filter or a sort did anything. -->
      <span class="font-mono text-dim" role="status">
        {{ count }}
        <span v-if="!music" class="sr-only">items</span>
      </span>
    </div>

    <!-- Always in the document, even empty: a live region inserted together
         with its text is commonly announced by nothing at all. `alert`, not
         `status`, because a chunk failing under a grid that looks full is not
         polite news — and the button is outside the region, or its label is
         read as part of the message every time the message changes. -->
    <div class="mb-3 flex items-center gap-3">
      <p class="m-0 text-warn" role="alert">{{ pageFailure }}</p>
      <Btn v-if="pageFailure" ghost small @click="retryPage">Try again</Btn>
    </div>

    <p v-if="music && !query && artists.total.value === 0" class="text-dim">
      Nothing here yet. Attach a collection to this library and its scan will fill this page.
    </p>
    <p v-else-if="music && query && artists.total.value === 0 && total === 0" class="text-dim">
      Nothing matches “{{ query }}”.
    </p>
    <p v-else-if="!music && total === 0" class="text-dim">
      {{
        query
          ? `Nothing matches “${query}”.`
          : 'Nothing here yet. Attach a collection to this library and its scan will fill this page.'
      }}
    </p>

    <section v-if="music && (artists.total.value ?? 0) > 0" class="artist-grid">
      <h2 v-if="query" class="mb-3 text-[17px] font-[650]">Artists</h2>
      <PagedGrid :total="artists.total.value" min-width="150px" @need="artists.need">
        <template #default="{ at }">
          <ArtistCard
            :artist="artists.loaded.value.get(at)"
            :library="library"
            @open="(artist) => openArtist(artist.key)"
          />
        </template>
      </PagedGrid>
      <p class="mt-4 text-[12px] text-dim">
        Artist portraits from
        <a class="text-teal hover:underline" href="https://fanart.tv/" rel="noreferrer"
          >fanart.tv</a
        >
        and
        <a class="text-teal hover:underline" href="https://www.theaudiodb.com/" rel="noreferrer"
          >TheAudioDB</a
        >; album-cover collages come from your library.
      </p>
    </section>

    <h2 v-if="music && query && total !== 0" class="mt-7 mb-3 text-[17px] font-[650]">
      Albums and songs
    </h2>

    <!-- The whole library's height, reserved before a single card past the
         fold has been fetched. That is the difference from infinite scroll,
         where the page grows as you go and the scrollbar jumps under the thumb
         every time it does. -->
    <div
      v-if="!music || query"
      ref="wrap"
      class="relative"
      :style="height !== undefined ? { height: `${height}px` } : {}"
    >
      <!-- `role="list"`, because the reset that comes with Tailwind strips
           list semantics in Safari. `aria-setsize` because this list is
           virtualised: without it a 2242-item library announces as the ninety
           cells that happen to be mounted.
           The gap comes from the same constant the row pitch is measured
           against — it is the one number the component cannot measure, and it
           lived here as a literal while `GAP` lived in the module. -->
      <ul
        ref="grid"
        class="grid absolute top-0 right-0 left-0 will-change-transform"
        role="list"
        :style="{
          ...shapeOf(self?.media_type ?? ''),
          gap: `${GAP}px`,
          ...(offset ? { transform: offset } : {}),
        }"
      >
        <li v-for="at in cells" :key="at" :aria-setsize="total ?? -1" :aria-posinset="at + 1">
          <Card
            :item="loaded.get(at)"
            :matchable="!!loaded.get(at) && matchable(loaded.get(at)!)"
            @open="open"
            @match="matching = { item: loaded.get(at)!, at }"
          />
        </li>
      </ul>
    </div>

    <MatchDialog
      v-if="matching"
      :item="matching.item"
      @close="matching = null"
      @applied="refresh(matching!.at)"
    />
  </main>
</template>

<style scoped>
.grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(var(--card-min, 140px), 1fr));
}

.artist-grid {
  --card-ratio: 1;
}
</style>
