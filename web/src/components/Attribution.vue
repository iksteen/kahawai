<script setup lang="ts">
/// Who the metadata came from.
///
/// The TMDB line is a term of use, not a design choice — it has to be on any
/// page showing their data. The other three are the same courtesy to the
/// projects the rest of the catalogue is built on.
import tmdb from '../assets/tmdb.svg'

defineProps<{ provider?: string | null | undefined }>()

const SAYS: Record<string, string> = {
  tmdb: 'This product uses TMDB and the TMDB APIs but is not endorsed, certified, or otherwise approved by TMDB.',
  musicbrainz: 'Metadata from MusicBrainz; cover art from the Cover Art Archive.',
  anilist: 'Metadata from AniList and AniDB.',
  tvdb: 'Metadata provided by TheTVDB. Please consider contributing missing data.',
}
</script>

<template>
  <footer
    v-if="provider && SAYS[provider]"
    class="mt-10 flex items-center gap-3 border-t border-hairline pt-4 text-[12px] text-dim"
  >
    <img v-if="provider === 'tmdb'" :src="tmdb" alt="TMDB" class="h-4" />
    <span>{{ SAYS[provider] }}</span>
  </footer>
</template>
