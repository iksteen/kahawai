<script setup lang="ts">
/// Cached title artwork frames the details; music keeps its compact sleeve.
import { computed, ref, watch } from 'vue'
import type { Description } from '../api/generated/model/description.ts'

import { artShape } from '../domain/detail.ts'
import { artworkSrcSet, artworkUrl } from '../api/artwork.ts'
import { duration } from '../domain/source.ts'
import { watchedPct } from '../domain/label.ts'

const props = defineProps<{
  item: {
    library_id?: string | null
    id: string
    kind: string
    title: string
    year: number | null
    art_version: number | null
    duration_ms: number | null
    resume_position_ms: number | null
    resume_duration_ms: number | null
    match_confidence?: string | null
    metadata?: Description | null | undefined
  }
  /// The mono line under the title. Different per kind, so the caller writes
  /// it — a show counts episodes, an album counts tracks, a film says how long
  /// it is.
  subline: string
  /// How far through, or null when it has not been started, or absent to read
  /// it off the item. A series says nothing here: its progress is the season
  /// counts under it.
  progress?: number | null | undefined
  season?: number | null
  heading?: string
}>()

const video = computed(() => ['movie', 'series', 'episode'].includes(props.item.kind))
const shape = computed(() => artShape(props.item.kind))
const broken = ref(false)
const selection = computed(() => ({
  ...(props.season === undefined ? {} : { season: props.season }),
  image: props.item.metadata?.artwork?.[0] ?? null,
}))
const source = computed(() =>
  artworkUrl(props.item.id, props.item.art_version, 'card', props.item.library_id, selection.value),
)
watch(source, () => (broken.value = false))
// Keep the browser cache. Only a changed selected image resets a failed load;
// watched-state refreshes keep the same URL and must not remount images.
const backgroundKind = computed(() =>
  props.item.metadata?.backdrop ? 'backdrop' : props.item.metadata?.banner ? 'banner' : null,
)
const background = computed(() =>
  video.value && backgroundKind.value
    ? artworkUrl(props.item.id, null, undefined, props.item.library_id, {
        kind: backgroundKind.value,
        image: props.item.metadata?.[backgroundKind.value] ?? null,
      })
    : '',
)
const backgroundBroken = ref(false)
watch(background, () => (backgroundBroken.value = false))
const logo = computed(() =>
  video.value && props.item.metadata?.logo && props.item.kind !== 'episode'
    ? artworkUrl(props.item.id, null, undefined, props.item.library_id, {
        kind: 'logo',
        image: props.item.metadata.logo,
      })
    : '',
)
const logoLoaded = ref(false)
const logoBroken = ref(false)
watch(logo, () => {
  logoLoaded.value = false
  logoBroken.value = false
})
const done = computed(() =>
  props.progress === undefined ? watchedPct(props.item) : props.progress,
)
const runtime = computed(() => duration(props.item.duration_ms))
</script>

<template>
  <div class="detail-head mt-[22px] mb-1.5" :class="video && 'video-head'">
    <img
      v-if="background && !backgroundBroken"
      :key="background"
      class="detail-backdrop"
      :src="background"
      alt=""
      aria-hidden="true"
      @error="backgroundBroken = true"
    />
    <div class="detail-content flex flex-wrap items-start gap-6">
      <span
        class="art-box shrink-0 rounded-md border border-line"
        :style="{ width: `min(${shape.width}, 46vw)` }"
      >
        <img
          :key="source"
          class="block w-full rounded-md object-cover"
          :class="broken && 'invisible'"
          :style="{ aspectRatio: shape.ratio }"
          :src="source"
          :srcset="
            artworkSrcSet(props.item.id, props.item.art_version, props.item.library_id, selection)
          "
          alt=""
          @error="broken = true"
        />
      </span>

      <div class="min-w-0 flex-1 basis-[260px]">
        <div v-if="logo && !logoBroken" class="title-logo" :class="!logoLoaded && 'hidden'">
          <img
            :key="logo"
            :src="logo"
            alt=""
            @load="logoLoaded = true"
            @error="logoBroken = true"
          />
        </div>
        <h1
          class="text-[22px] font-[650] tracking-[0.01em]"
          :class="logoLoaded && !logoBroken && !heading && 'sr-only'"
        >
          {{ heading ?? props.item.title }}
          <span v-if="props.item.year && !heading" class="text-dim">({{ props.item.year }})</span>
        </h1>
        <p
          v-if="logoLoaded && !logoBroken && props.item.year && !heading"
          class="mt-2 font-mono text-[13px] text-dim"
        >
          {{ props.item.year }}
        </p>
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
          <span v-if="props.item.metadata?.release_date" class="text-dim">
            {{ props.item.metadata.release_date }}
          </span>
          <span v-if="props.item.metadata?.rating != null" class="text-sand">
            ★ {{ props.item.metadata.rating.toFixed(1) }}
          </span>
          <span
            v-if="props.item.match_confidence === 'weak'"
            class="text-sand"
            title="The metadata match was not certain"
          >
            uncertain match
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
  </div>
</template>

<style scoped>
.art-box {
  /* The swell, so a poster that is slow or missing is a mark rather than a
     hole — the same box the cards use. */
  background: var(--art-none);
  overflow: hidden;
}
.video-head {
  position: relative;
  isolation: isolate;
  overflow: hidden;
  border: 1px solid var(--color-line);
  border-radius: 12px;
  background: var(--color-bg);
  margin-bottom: 28px;
}
.detail-backdrop {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  object-fit: cover;
  object-position: center 30%;
  z-index: -2;
}
.video-head::before {
  content: '';
  position: absolute;
  inset: 0;
  z-index: -1;
  background:
    linear-gradient(90deg, rgba(11, 20, 22, 0.78), rgba(11, 20, 22, 0.88) 50%),
    linear-gradient(0deg, var(--color-bg), transparent 70%);
}
.video-head .detail-content {
  padding: clamp(18px, 3vw, 36px);
  min-height: 330px;
  align-items: center;
}
.video-head .text-dim {
  color: var(--color-prose);
}
.title-logo img {
  display: block;
  width: min(100%, 380px);
  height: 110px;
  object-fit: contain;
  object-position: left center;
}
@media (max-width: 540px) {
  .video-head .art-box {
    display: none;
  }
  .video-head .detail-content {
    min-height: 300px;
  }
  .title-logo img {
    height: 90px;
  }
}
</style>
