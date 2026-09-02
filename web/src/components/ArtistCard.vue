<script setup lang="ts">
import { ref, watch } from 'vue'

import type { ArtistSummary } from '../api/generated/model/artistSummary.ts'
import { artistArtworkSrcSet, artistArtworkUrl } from '../api/artwork.ts'
import CardFrame from './CardFrame.vue'

const props = defineProps<{
  artist?: ArtistSummary | undefined
  library: string
}>()
const emit = defineEmits<{ open: [artist: ArtistSummary] }>()
const broken = ref(false)
watch([() => props.artist?.key, () => props.artist?.art_version], () => (broken.value = false))

function albums(artist: ArtistSummary): string {
  return `${artist.album_count} ${artist.album_count === 1 ? 'album' : 'albums'}`
}
</script>

<template>
  <CardFrame v-if="!artist" pending />
  <CardFrame v-else :title="artist.name" :meta="albums(artist)" @open="emit('open', artist)">
    <template #art>
      <span class="art-box">
        <img
          v-if="artist.art_version !== null"
          class="art"
          :class="broken && 'invisible'"
          :src="artistArtworkUrl(artist.key, library, artist.art_version, 'card')"
          :srcset="artistArtworkSrcSet(artist.key, library, artist.art_version)"
          loading="lazy"
          alt=""
          @error="broken = true"
        />
      </span>
    </template>
  </CardFrame>
</template>

<style scoped>
@reference '../theme.css';

.art-box {
  @apply relative block overflow-hidden rounded;
  aspect-ratio: var(--card-ratio, 1);
  background: var(--art-none);
}
.art {
  @apply block h-full w-full rounded object-cover;
  background: color-mix(in srgb, var(--color-line), var(--color-surface));
}
</style>
