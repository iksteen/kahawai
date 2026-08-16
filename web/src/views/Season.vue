<script setup lang="ts">
/// One season, browsed by its stills.
///
/// The show page lists episodes as rows, which is the right shape for picking
/// a number. This is the other question — "which one was that again" — so the
/// episodes are pictures, and the one you land on opens underneath rather than
/// on a page of its own.
import { computed, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'

import Btn from '../components/Btn.vue'
import Failed from '../components/Failed.vue'
import Icon from '../components/Icon.vue'
import Art from '../components/Art.vue'
import Lane from '../components/Lane.vue'
import {
  episodeOf,
  projecting,
  resumeMs,
  seasonLabel,
  seasonOf,
  seLabel,
  watchedPct,
} from '../domain/label.ts'
import { parseSeason } from '../domain/routes.ts'
import { sentence } from '../domain/refusal.ts'
import { notify } from '../composables/notices.ts'
import { useChildrenOf, useItem, useWatched } from '../composables/item.ts'
import { useScreenName } from '../composables/title.ts'

/// How much of the strip one press of an arrow moves: two and a bit cards, so
/// something new is always in view and something old still is.
const STEP = 220 * 2.5

const route = useRoute()
const router = useRouter()

const showId = computed(() => String(route.params.id ?? ''))
const library = computed(() => String(route.params.library ?? ''))
/// Null is absolute numbering, not "unknown" — see `seasonLabel`.
const season = computed(() => parseSeason(String(route.params.season ?? 'all')))

/// The episodes ARE this page, so they are asked for by the id in the URL —
/// not behind the item, which supplies only the title on the back button. In
/// series they cost a round trip before a single still appears; and which of
/// the two failures the viewer saw depended on which settled last.
const children = useChildrenOf(showId)
const show = useItem(showId)
const { mark, busy } = useWatched()

/// The show's own details failing is a notice: the page underneath is the
/// episodes, and they are fine.
watch(
  () => show.isError.value,
  (failed) => failed && notify(`Could not load the show's details: ${sentence(show.error.value)}`),
)

const projected = computed(() => projecting('seasons', children.data.value ?? []))

/// UI-17. The show first: "Season 2" is what every one of these screens is
/// called, and a tab strip full of them names nothing. A failure panel is
/// still a screen, and one that never publishes is never announced.
///
/// The two failures are not the same failure, and the condition here is the
/// template's — the SHOW's details going missing is a notice over a page full
/// of working episodes, and calling that "could not load this season" tells
/// both the tab strip and the screen reader something the screen contradicts.
useScreenName(
  'season',
  computed(() => {
    if (children.isError.value && !children.data.value) return 'Could not load this season'
    const label = seasonLabel(season.value, projected.value)
    const series = show.data.value?.title?.trim()
    if (series) return `${series} · ${label}`
    return show.isError.value ? label : null
  }),
)
/// `undefined` until the children answer. An empty array meant either
/// "loading" or "this show has no episodes", so the empty-season explanation
/// was suppressed for the case it was written for and the page read as broken.
const mine = computed(() =>
  children.data.value?.filter((e) => seasonOf(e, projected.value) === season.value),
)
const watched = computed(() => mine.value?.filter((e) => e.played).length ?? 0)
const allSeen = computed(
  () => (mine.value?.length ?? 0) > 0 && watched.value === mine.value?.length,
)

/// Which still is open underneath the strip. The episode itself is fetched per
/// selection, because the strip only holds rows.
///
/// Opened on the first thing you have not finished — the reason you came —
/// and dropped whenever the season or the series changes, or the panel under
/// season 2 goes on showing an episode of season 1.
const pickedId = ref<string | null>(null)
watch([showId, season], () => (pickedId.value = null))
watch(
  mine,
  (episodes) => {
    if (pickedId.value !== null || !episodes?.length) return
    pickedId.value = (episodes.find((e) => !e.played) ?? episodes[0])!.id
  },
  { immediate: true },
)
const picked = useItem(computed(() => pickedId.value ?? ''))
watch(
  () => picked.isError.value,
  (failed) => {
    if (!failed) return
    // Not an error inside the panel this failure prevents: the card took the
    // highlight, nothing opened, and clicking it again was a no-op because
    // the selection had not changed. A dead click, for good.
    pickedId.value = null
  },
)

/// The number as the STRIP has it, and whether the file is there to play.
///
/// Only browse carries the projection, so asking the item for itself gets a
/// null projected season — and this line printed E10 under a card badged
/// S01E10: the same episode, numbered two ways, a centimetre apart.
const pickedNumber = computed(() => {
  const open = picked.data.value
  if (!open) return ''
  const row = mine.value?.find((e) => e.id === open.id)
  return row
    ? seLabel(seasonOf(row, projected.value), episodeOf(row, projected.value), row.episode_end)
    : seLabel(open.season, open.episode, open.episode_end)
})
const pickedPlayable = computed(() => picked.data.value?.sources?.[0]?.available ?? false)

function goUp() {
  void router.push({ name: 'detail', params: { library: library.value, id: showId.value } })
}
function play(id: string, fromStart = false) {
  void router.push({
    name: 'player',
    params: { library: library.value, id },
    ...(fromStart ? { query: { start: '0' } } : {}),
  })
}
</script>

<template>
  <!-- The EPISODES are the page. Keyed on them having nothing to show, so a
       background re-ask that fails — every mark invalidates this — cannot
       replace a season somebody is looking at. -->
  <Failed
    v-if="children.isError.value && !children.data.value"
    what="Could not load this season."
    :message="sentence(children.error.value)"
    away="Back to the show"
    @retry="children.refetch()"
    @away="goUp"
  />

  <main v-else>
    <Btn ghost small class="mb-4" @click="goUp">← {{ show.data.value?.title ?? 'Series' }}</Btn>

    <div class="mb-4 flex flex-wrap items-baseline gap-3">
      <h1 class="text-[22px] font-[650] tracking-[0.01em]">
        {{ seasonLabel(season, projected) }}
      </h1>
      <span class="font-mono text-dim">
        {{ mine === undefined ? '' : `${mine.length} episodes · ${watched} watched` }}
      </span>
      <!-- One press for a season somebody has already watched elsewhere.
           WHICH episodes are in it is decided here, because the season a
           viewer sees can be a projection of absolute numbering. -->
      <Btn
        ghost
        small
        :disabled="busy.has(showId) || !mine?.length"
        :title="
          mine === undefined
            ? 'Still loading the episodes'
            : mine.length === 0
              ? 'This season has no episodes'
              : undefined
        "
        @click="mark(showId, !allSeen, mine?.map((e) => e.id) ?? [])"
      >
        <Icon name="check" :size="13" />
        {{ allSeen ? 'Mark none watched' : 'Mark all watched' }}
      </Btn>
    </div>

    <!-- A hand-typed or stale season number renders a heading, an empty strip
         and two dead arrows, which reads as broken rather than as empty. -->
    <p v-if="mine?.length === 0" class="text-dim">
      No episodes in {{ seasonLabel(season, projected).toLowerCase() }}.
      <button class="cursor-pointer underline" type="button" @click="goUp">All episodes</button>
    </p>

    <Lane v-else :step="STEP" :label="seasonLabel(season, projected)">
      <button
        v-for="episode in mine ?? []"
        :key="episode.id"
        class="w-[220px] shrink-0 cursor-pointer text-left"
        :class="episode.id === pickedId && 'text-teal'"
        type="button"
        :aria-pressed="episode.id === pickedId"
        @click="pickedId = episode.id"
      >
        <!-- Through `Art`, which owns the swell behind a still, the hiding of
             one that will not load, and the seen and progress marks. Hand
             rolled here, a still with no artwork painted the browser's own
             broken-image mark over the swell. -->
        <span class="relative block" style="--card-ratio: 16 / 9">
          <Art :item="episode" size="card" />
          <span class="badge bottom-1.5 left-1.5 font-mono text-[11px]">
            {{
              seLabel(
                seasonOf(episode, projected),
                episodeOf(episode, projected),
                episode.episode_end,
              )
            }}
          </span>
        </span>
        <span class="mt-1 block truncate text-[13px]">{{ episode.title }}</span>
        <span class="block truncate font-mono text-[11px] text-dim">
          {{
            episode.played
              ? 'seen'
              : watchedPct(episode) !== null
                ? `${Math.round(watchedPct(episode) ?? 0)}% in`
                : ''
          }}
        </span>
      </button>
    </Lane>

    <!-- The one you land on opens here rather than on a page of its own. -->
    <section
      v-if="picked.data.value && pickedId"
      class="mt-6 flex flex-wrap gap-5 rounded-md border border-line bg-surface p-4"
    >
      <span class="block w-[320px] max-w-[46vw] shrink-0" style="--card-ratio: 16 / 9">
        <Art :item="picked.data.value" size="card" :progress="false" />
      </span>
      <div class="min-w-[240px] flex-1">
        <div class="font-mono text-[13px] text-dim">{{ pickedNumber }}</div>
        <h2 class="text-[17px] font-[650]">{{ picked.data.value.title }}</h2>
        <div v-if="picked.data.value.metadata?.premiered" class="font-mono text-[13px] text-dim">
          {{ picked.data.value.metadata.premiered }}
        </div>
        <p v-if="picked.data.value.metadata?.overview" class="mt-2 max-w-[70ch] text-prose">
          {{ picked.data.value.metadata.overview }}
        </p>
        <div class="mt-3 flex flex-wrap items-center gap-3">
          <Btn
            :disabled="busy.has(picked.data.value.id) || !pickedPlayable"
            :title="pickedPlayable ? undefined : 'The machine holding this file is not answering'"
            @click="play(picked.data.value.id)"
          >
            ▶ {{ resumeMs(picked.data.value) ? 'Resume' : 'Play' }}
          </Btn>
          <Btn
            v-if="resumeMs(picked.data.value) > 0"
            ghost
            :disabled="busy.has(picked.data.value.id)"
            @click="play(picked.data.value.id, true)"
          >
            Play from start
          </Btn>
          <Btn
            ghost
            small
            :disabled="busy.has(picked.data.value.id)"
            @click="mark(picked.data.value.id, !picked.data.value.played)"
          >
            <Icon name="check" />
            {{ picked.data.value.played ? 'Watched' : 'Mark watched' }}
          </Btn>
        </div>
      </div>
    </section>
  </main>
</template>

<style scoped>
@reference '../theme.css';

.still-box {
  @apply relative block overflow-hidden rounded;
  background: var(--art-none);
}
.badge {
  @apply absolute inline-flex rounded p-1 text-teal;
  background: rgba(10, 16, 18, 0.78);
  line-height: 0;
}
</style>
