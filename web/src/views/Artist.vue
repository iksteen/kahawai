<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'

import Btn from '../components/Btn.vue'
import Card from '../components/Card.vue'
import Failed from '../components/Failed.vue'
import PagedGrid from '../components/PagedGrid.vue'
import { useArtistAlbums } from '../composables/music.ts'
import { useSearchQuery } from '../composables/search.ts'
import { useScreenName } from '../composables/title.ts'

const route = useRoute()
const router = useRouter()
const library = computed(() => String(route.params.library ?? ''))
const key = computed(() => String(route.params.artist ?? ''))
const query = useSearchQuery()
const sort = ref('year')
const albums = useArtistAlbums(library, key, query, sort)

watch([library, key, query, sort], () => window.scrollTo({ top: 0 }))

useScreenName(
  computed(
    () => albums.artist.value?.name ?? (albums.failure.value ? 'Could not load artist' : null),
  ),
)

function openAlbum(id: string) {
  void router.push({
    name: 'artist-album',
    params: { library: library.value, artist: key.value, id },
  })
}
</script>

<template>
  <Failed
    v-if="albums.failure.value && !albums.artist.value"
    what="Could not load this artist."
    :message="albums.failure.value"
    away="Back to library"
    @retry="albums.retry"
    @away="router.push({ name: 'library', params: { library } })"
  />

  <main v-else>
    <Btn
      ghost
      small
      class="mb-[18px]"
      @click="router.push({ name: 'library', params: { library } })"
    >
      ← Library
    </Btn>
    <div class="mb-4 flex items-baseline gap-3">
      <h1 class="text-[22px] font-[650] tracking-[0.01em]">
        {{ albums.artist.value?.name ?? 'Artist' }}
      </h1>
      <select
        v-model="sort"
        class="ml-auto rounded-md border border-line bg-surface px-2 py-1 text-[13px]"
        aria-label="Sort albums"
      >
        <option value="year">Oldest first</option>
        <option value="-year">Newest first</option>
        <option value="title">Title A–Z</option>
        <option value="-title">Title Z–A</option>
        <option value="-added">Recently added</option>
        <option value="added">Oldest added</option>
      </select>
      <span class="font-mono text-dim" role="status">
        <template v-if="albums.total.value !== null">
          {{ albums.total.value }} {{ albums.total.value === 1 ? 'album' : 'albums' }}
        </template>
      </span>
    </div>

    <div v-if="albums.failure.value" class="mb-3 flex items-center gap-3">
      <p class="m-0 text-warn" role="alert">{{ albums.failure.value }}</p>
      <Btn ghost small @click="albums.retry">Try again</Btn>
    </div>

    <p v-if="albums.total.value === 0" class="text-dim">
      {{ query ? `Nothing matches “${query}”.` : 'This artist has no albums.' }}
    </p>
    <PagedGrid class="album-grid" :total="albums.total.value" min-width="150px" @need="albums.need">
      <template #default="{ at }">
        <Card
          :item="albums.loaded.value.get(at)"
          @open="openAlbum(albums.loaded.value.get(at)!.id)"
        />
      </template>
    </PagedGrid>
  </main>
</template>

<style scoped>
.album-grid {
  --card-ratio: 1;
}
</style>
