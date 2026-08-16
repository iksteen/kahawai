<script setup lang="ts">
/// The head every item page opens with: artwork on the left, everything that
/// identifies the item on the right, and the actions under it.
///
/// One shape for all four kinds rather than three near-copies — only the
/// artwork's proportions differ, and they follow what the artwork IS.
import { computed, ref } from 'vue'

import { artShape } from '../domain/detail.ts'
import { artworkSrcSet, artworkUrl } from '../api/artwork.ts'
import { duration } from '../domain/source.ts'
import { watchedPct } from '../domain/label.ts'

const props = defineProps<{
  item: {
    id: string
    kind: string
    title: string
    year: number | null
    art_version: number | null
    play_count: number
    duration_ms: number | null
    resume_position_ms: number | null
    resume_duration_ms: number | null
    metadata?:
      | {
          overview?: string | null
          premiered?: string | null
          rating?: number | null
          confidence?: string | null
        }
      | null
      | undefined
  }
  /// The mono line under the title. Different per kind, so the caller writes
  /// it — a show counts episodes, an album counts tracks, a film says how long
  /// it is.
  subline: string
  /// How far through, or null when it has not been started, or absent to read
  /// it off the item. A series says nothing here: its progress is the season
  /// counts under it.
  progress?: number | null | undefined
}>()

const shape = computed(() => artShape(props.item.kind))
const broken = ref(false)
const done = computed(() =>
  props.progress === undefined ? watchedPct(props.item) : props.progress,
)
const runtime = computed(() => duration(props.item.duration_ms))
</script>

<template>
  <div class="mt-[22px] mb-1.5 flex flex-wrap items-start gap-6">
    <span
      class="art-box shrink-0 rounded-md border border-line"
      :style="{ width: `min(${shape.width}, 46vw)` }"
    >
      <img
        class="block w-full rounded-md object-cover"
        :class="broken && 'invisible'"
        :style="{ aspectRatio: shape.ratio }"
        :src="artworkUrl(props.item.id, props.item.art_version, 'card')"
        :srcset="artworkSrcSet(props.item.id, props.item.art_version)"
        alt=""
        @error="broken = true"
      />
    </span>

    <div class="min-w-[260px] flex-1">
      <h1 class="text-[22px] font-[650] tracking-[0.01em]">
        {{ props.item.title }}
        <span v-if="props.item.year" class="text-dim">({{ props.item.year }})</span>
      </h1>
      <div class="mt-1 font-mono text-[13px] text-dim">
        {{ [props.subline, runtime].filter(Boolean).join(' · ') }}
      </div>

      <span v-if="done !== null" class="mt-2 block h-[3px] max-w-[320px] rounded bg-hairline">
        <span class="block h-full rounded bg-sand" :style="{ width: `${done}%` }" />
      </span>

      <p v-if="props.item.metadata?.overview" class="mt-3 max-w-[70ch] text-prose">
        {{ props.item.metadata.overview }}
      </p>

      <!-- The facts nobody needs in a heading but everybody checks. -->
      <div class="mt-3 flex flex-wrap gap-3 font-mono text-[12px]">
        <span v-if="props.item.metadata?.premiered" class="text-dim">
          {{ props.item.metadata.premiered }}
        </span>
        <span v-if="props.item.metadata?.rating != null" class="text-sand">
          ★ {{ props.item.metadata.rating.toFixed(1) }}
        </span>
        <span
          v-if="props.item.metadata?.confidence === 'weak'"
          class="text-sand"
          title="The metadata match was not certain"
        >
          uncertain match
        </span>
        <span v-if="props.item.play_count > 0" class="text-teal">
          seen ×{{ props.item.play_count }}
        </span>
      </div>

      <!-- The BUTTONS do not shrink and their labels do not break: this column
           is narrow on a phone and beside a poster, and a shrinking flex item
           let "Play from start" fold onto two lines mid-word while there was
           still a whole row of space beside it. Buttons only — the slot also
           carries the next episode's title, which is prose and has to be free
           to wrap. -->
      <div
        class="mt-5 mb-1 flex flex-wrap items-center gap-3 [&>button]:flex-none [&>button]:whitespace-nowrap"
      >
        <slot />
      </div>
    </div>
  </div>
</template>

<style scoped>
.art-box {
  /* The swell, so a poster that is slow or missing is a mark rather than a
     hole — the same box the cards use. */
  background: var(--art-none);
  overflow: hidden;
}
</style>
