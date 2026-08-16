<script setup lang="ts">
/// Artwork, with the kahawai swell on the box behind it — so a poster that is
/// slow shows the swell rather than a white flash, and one that never arrives
/// simply keeps it.
///
/// UI-22 is three states and they are drawn differently on purpose:
///
/// - **not arrived yet** — the `<img>`'s own opaque background, half-strength
///   slate, covering the swell. The browser paints it until the content lands,
///   so this one costs nothing.
/// - **there is no poster** — the swell at full strength, revealed by hiding
///   the image on `error`.
/// - **the row itself has not arrived** — the caller's ghost, which is a
///   different component again.
///
/// Before UI-22 every unpainted image showed the swell, so a slow page looked
/// like a library with no artwork at all.
import { computed, ref } from 'vue'

import Icon, { type IconName } from './Icon.vue'
import { type ArtSize, artworkSrcSet, artworkUrl } from '../api/artwork.ts'
import { watchedPct } from '../domain/label.ts'

const props = withDefaults(
  defineProps<{
    item: {
      id: string
      kind: string
      played: boolean
      art_version: number | null
      resume_position_ms: number | null
      resume_duration_ms: number | null
    }
    size: ArtSize
    /// Draws the resume bar across the bottom of the art. A shelf card wants
    /// it there — the art is all it has. A continue-watching card does not:
    /// its whole text column ends in a progress bar, and drawing it twice on
    /// the same card says it twice.
    progress?: boolean
    /// Show somebody else's artwork: an episode's own is a landscape still,
    /// and in a row of portrait posters it is the one thing that does not
    /// belong. Its show's poster is the same shape as everything beside it.
    ///
    /// No `art_version` travels with it — that number describes THIS item's
    /// artwork, and pinning the parent's URL with the child's version would be
    /// a cache key that lies. The cost is that a re-matched show keeps its old
    /// poster here until the browser's copy expires.
    posterOf?: string | null
  }>(),
  { progress: true, posterOf: null },
)

/// A poster that will not load is hidden, revealing the swell on the box
/// behind it. `visibility`, not `display`: the box keeps the height the layout
/// measured, and an `<img>` with no source still gets the browser's own
/// broken-artwork mark.
const broken = ref(false)

const artId = computed(() => props.posterOf ?? props.item.id)
const artVersion = computed(() => (props.posterOf ? undefined : props.item.art_version))
const done = computed(() => (props.progress ? watchedPct(props.item) : null))

function kindGlyph(kind: string): IconName | null {
  if (kind === 'movie') return 'movie'
  if (kind === 'show' || kind === 'episode') return 'show'
  if (kind === 'album' || kind === 'track') return 'album'
  return null
}
const glyph = computed(() => kindGlyph(props.item.kind))
</script>

<template>
  <!-- The box carries the swell and the aspect ratio. The ratio is what gives
       it a definite height — without it the image's own intrinsic height drove
       the card, so a row of posters and landscape stills came out ragged and
       every card grew as its picture arrived. -->
  <span class="art-box">
    <img
      class="art"
      :class="broken && 'invisible'"
      :src="artworkUrl(artId, artVersion, props.size)"
      :srcset="props.size === 'card' ? artworkSrcSet(artId, artVersion) : undefined"
      loading="lazy"
      alt=""
      @error="broken = true"
    />
    <span
      v-if="glyph"
      class="badge left-1.5 top-1.5"
      :title="props.item.kind === 'show' ? 'series' : props.item.kind"
    >
      <Icon :name="glyph" />
    </span>
    <span v-if="props.item.played" class="badge right-1.5 bottom-1.5" title="seen">
      <Icon name="check" />
    </span>
    <span
      v-if="done !== null && !props.item.played"
      class="absolute right-0 bottom-0 left-0 block h-[3px] overflow-hidden rounded-b bg-[rgba(10,16,18,0.6)]"
    >
      <!-- Sand, which is "how far through" everywhere in this app. Teal is the
           interactive accent and means something else. -->
      <span class="block h-full bg-sand" :style="{ width: `${done}%` }" />
    </span>
  </span>
</template>

<style scoped>
@reference '../theme.css';

.art-box {
  @apply relative block overflow-hidden rounded;
  background: var(--art-none);
}
.art {
  @apply block w-full rounded object-cover;
  /* A poster unless the view says otherwise. The shelf sets this per library:
     a sleeve is square and a poster is two by three. */
  aspect-ratio: var(--card-ratio, 2 / 3);
  /* Opaque, so it covers the swell while the picture is in flight — the same
     blank as a card that has not arrived at all, because it says the same
     thing. */
  background: color-mix(in srgb, var(--color-line), var(--color-surface));
}
.badge {
  @apply absolute inline-flex rounded p-1 text-teal;
  background: rgba(10, 16, 18, 0.78);
  line-height: 0;
}
</style>
