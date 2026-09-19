<script setup lang="ts">
import { computed } from 'vue'
import tmdb from '../assets/tmdb.svg'

const props = defineProps<{ providers: string[] }>()

// Checked 2026-09-18. https://www.themoviedb.org/api-terms-of-use (§3):
// Requires the TMDB logo and the exact non-endorsement notice below.
// https://developer.themoviedb.org/docs/faq also requires an About/Credits section.
// https://www.thetvdb.com/api-information requires "attribution with a direct link"
// for end users viewing its metadata.
// MusicBrainz supplementary data requires credit under CC BY-NC-SA 3.0:
// https://musicbrainz.org/doc/About/Data_License — "MusicBrainz is given credit".
// AniList/AniDB credits are separate: using one does not imply using the other.
const credits: Record<string, { name: string; url: string; text: string }> = {
  tmdb: {
    name: 'TMDB',
    url: 'https://www.themoviedb.org',
    text: 'This product uses TMDB and the TMDB APIs but is not endorsed, certified, or otherwise approved by TMDB.',
  },
  tvdb: {
    name: 'TheTVDB',
    url: 'https://thetvdb.com',
    text: 'Metadata provided by TheTVDB. Please consider adding missing information or subscribing.',
  },
  musicbrainz: {
    name: 'MusicBrainz',
    url: 'https://musicbrainz.org',
    text: 'Metadata from MusicBrainz.',
  },
  anilist: { name: 'AniList', url: 'https://anilist.co', text: 'Metadata from AniList.' },
  anidb: { name: 'AniDB', url: 'https://anidb.net', text: 'Metadata from AniDB.' },
}
const shown = computed(() => [...new Set(props.providers)].sort().filter((p) => credits[p]))
</script>

<template>
  <footer
    v-if="shown.length"
    aria-label="Metadata credits"
    class="mt-10 flex flex-col gap-3 border-t border-hairline pt-4 text-[12px] text-dim"
  >
    <div v-for="provider in shown" :key="provider" class="flex flex-wrap items-center gap-3">
      <a
        :href="credits[provider]!.url"
        :aria-label="credits[provider]!.name"
        target="_blank"
        rel="noopener noreferrer"
        class="underline"
      >
        <img v-if="provider === 'tmdb'" :src="tmdb" alt="TMDB" class="h-4" />
        <template v-else>{{ credits[provider]!.name }}</template>
      </a>
      <span>{{ credits[provider]!.text }}</span>
      <template v-if="provider === 'musicbrainz'">
        <a
          href="https://musicbrainz.org/doc/About/Data_License"
          target="_blank"
          rel="noopener noreferrer"
          class="underline"
          >Data licenses (CC0 / CC BY-NC-SA 3.0)</a
        >
        <a
          href="https://coverartarchive.org"
          target="_blank"
          rel="noopener noreferrer"
          class="underline"
          >Cover Art Archive</a
        >
      </template>
    </div>
  </footer>
</template>
