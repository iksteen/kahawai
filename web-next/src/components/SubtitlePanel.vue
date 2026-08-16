<script setup lang="ts">
/// HUB-24: the subtitles this item has, and finding more.
///
/// The PLAYER is where tracks get picked; this section is about managing
/// downloads, so the file's own tracks are one line of prose and the hub-stored
/// ones are the list.
import { computed, ref } from 'vue'

import Btn from './Btn.vue'
import type { Candidate } from '../api/generated/model/candidate.ts'
import type { Quota } from '../api/generated/model/quota.ts'
import type { TrackListing } from '../api/generated/model/trackListing.ts'
import { notify } from '../composables/notices.ts'
import { putPref } from '../composables/prefs.ts'
import { quotaLabel } from '../domain/quota.ts'
import { sentence } from '../domain/refusal.ts'
import { subtitleDelete, subtitleDownload, subtitleSearch } from '../api/generated/kahawai.ts'

const props = defineProps<{
  item: { id: string; title: string; parent_id?: string | null }
  subs: TrackListing[]
  /// The media type's subtitle language preference (HUB-33). The search is
  /// filtered by it, with a one-click unfiltered retry.
  languages: string[]
  /// A standing choice for THIS title, which beats the language list in
  /// Settings — so it has to be visible here, and revocable.
  titleChoice: string
  /// The file's own frame rate, for the drift warning.
  fps?: number | null | undefined
}>()

const emit = defineEmits<{ changed: []; cleared: [] }>()

const busy = ref(false)
const note = ref('')
const quota = ref<Quota | null>(null)
/// `null` while the dialog is closed. An empty array is a search that found
/// nothing, which is a different thing and has its own offer.
const candidates = ref<Candidate[] | null>(null)

async function find(languages: string[]) {
  busy.value = true
  note.value = ''
  try {
    const answer = await subtitleSearch(props.item.id, { languages })
    candidates.value = answer.candidates
    quota.value = answer.quota
    if (answer.candidates.length === 0) {
      note.value = languages.length
        ? `Nothing in ${languages.join(', ')} for this file.`
        : 'No subtitles found for this file.'
    }
  } catch (cause) {
    note.value = sentence(cause)
  } finally {
    busy.value = false
  }
}

async function download(candidate: Candidate) {
  busy.value = true
  try {
    const answer = await subtitleDownload(props.item.id, {
      file_id: candidate.file_id,
      language: candidate.language,
    })
    quota.value = answer.quota
    candidates.value = null
    notify('Subtitle downloaded — it is now a track on this item.')
    emit('changed')
  } catch (cause) {
    note.value = sentence(cause)
  } finally {
    busy.value = false
  }
}

async function remove(track: TrackListing) {
  try {
    await subtitleDelete(track.id)
    emit('changed')
  } catch (cause) {
    notify(`Could not remove that track: ${sentence(cause)}`)
  }
}

async function followSettings() {
  try {
    await putPref(props.item.parent_id ?? props.item.id, 'subs', '')
    emit('cleared')
  } catch (cause) {
    notify(`Could not clear the override: ${sentence(cause)}`)
  }
}

/// What is in the FILE, as one line: the player is where these get picked.
const inTheFile = computed(() => {
  const own = props.subs.filter((s) => s.origin === 'embedded' || s.origin === 'sidecar')
  if (own.length === 0) return 'No subtitles in the file.'
  const languages = [...new Set(own.map((s) => s.language ?? '?'))]
  const shown = languages.slice(0, 12).join(', ')
  const more = languages.length > 12 ? `, +${languages.length - 12} more` : ''
  return `${own.length} in the file: ${shown}${more}`
})

/// What the hub is STORING for this item, which is what can be managed.
const stored = computed(() =>
  props.subs.filter(
    (s) => s.origin === 'downloaded' || s.origin === 'ocr' || s.origin === 'raster',
  ),
)

/// Timed for a different frame rate: the classic cause of progressive drift.
const drifts = (candidate: Candidate) =>
  !!candidate.fps && !!props.fps && Math.abs(candidate.fps - props.fps) > 0.1
</script>

<template>
  <section class="mt-8">
    <div class="mb-2 flex flex-wrap items-center gap-3">
      <h2 class="text-[14px] font-[650] tracking-[0.08em] text-dim uppercase">Subtitles</h2>
      <!-- A standing choice for this title beats the language list in Settings,
           so it has to be visible here — and revocable, or the only way back is
           to guess where it was set. -->
      <span
        v-if="props.titleChoice"
        class="flex items-center gap-1 rounded bg-sand/15 px-1.5 py-0.5 font-mono text-[11px] text-sand"
        title="This title overrides your language settings"
      >
        {{ props.titleChoice === 'off' ? 'no subtitles' : props.titleChoice }} for this title
        <button
          class="cursor-pointer px-0.5 hover:text-warn"
          type="button"
          aria-label="Follow my language settings again"
          title="Follow my language settings again"
          @click="followSettings"
        >
          ×
        </button>
      </span>
    </div>

    <p class="text-dim">{{ inTheFile }}</p>

    <ul v-if="stored.length" class="mt-2 flex flex-col">
      <li
        v-for="track in stored"
        :key="track.id"
        class="flex flex-wrap items-center gap-2 border-b border-hairline py-1.5 last:border-0"
      >
        <!-- "ocr" stays visible: machine-read text is imperfect by nature and
             must say so (HUB-32c). -->
        <span class="rounded border border-line px-1.5 py-0.5 font-mono text-[11px] text-dim">
          {{ track.origin }}
        </span>
        <span>{{ track.language ?? '?' }} · {{ track.format }}</span>
        <!-- What this track is DOING for the capabilities this browser
             declares. A stored artefact the ladder currently skips otherwise
             reads as the only subtitle on the item. -->
        <span
          class="rounded border border-line px-1.5 py-0.5 font-mono text-[11px]"
          :class="track.delivery === 'none' ? 'text-dimmer' : 'text-dim'"
          :title="track.note || undefined"
        >
          {{ track.delivery === 'none' ? 'unused' : track.delivery }}
        </span>
        <!-- Only a DOWNLOADED track, and only for whoever spent the provider
             quota on it, or an admin. The caches rebuild themselves, so
             removing one would be a button that undoes nothing. -->
        <Btn v-if="track.deletable" ghost small class="ml-auto" @click="remove(track)">Remove</Btn>
      </li>
    </ul>

    <div class="mt-3 flex flex-wrap items-center gap-2">
      <Btn ghost small :disabled="busy" @click="find(props.languages)">
        {{
          busy
            ? 'Searching…'
            : props.languages.length
              ? `Find subtitles (${props.languages.join(', ')})`
              : 'Find subtitles online'
        }}
      </Btn>
      <!-- The language filter comes from Settings → this media type; offer the
           unfiltered search when it finds nothing. -->
      <Btn
        v-if="props.languages.length && candidates?.length === 0 && !busy"
        ghost
        small
        @click="find([])"
      >
        Search all languages
      </Btn>
      <span v-if="note && !candidates" class="text-dim">{{ note }}</span>
      <span v-if="quotaLabel(quota)" class="font-mono text-[11px] text-dim">
        {{ quotaLabel(quota) }}
      </span>
    </div>

    <!-- In a dialog, not in the page: twenty-five candidates shoved the sources
         and the attribution a screen and a half down, and choosing one is a
         decision that deserves the foreground. -->
    <div
      v-if="candidates"
      class="fixed inset-0 z-40 flex items-start justify-center overflow-y-auto bg-black/60 p-6"
      @click="candidates = null"
      @keydown.esc="candidates = null"
    >
      <div
        class="w-full max-w-[900px] rounded-lg border border-line bg-surface p-4"
        role="dialog"
        aria-modal="true"
        aria-labelledby="subs-title"
        @click.stop
      >
        <div class="flex items-start gap-3">
          <h2 id="subs-title" class="text-[17px] font-[650]">
            Subtitles for “{{ props.item.title }}”
          </h2>
          <Btn ghost small class="ml-auto" aria-label="Close" @click="candidates = null">✕</Btn>
        </div>
        <p class="mt-1 text-dim">
          {{
            quotaLabel(quota) ||
            'Downloads are shared with everyone on this server unless you attach your own account in Settings.'
          }}
        </p>
        <p class="mt-1 text-warn" role="alert">{{ note }}</p>

        <div v-if="!candidates.length && props.languages.length && !busy" class="mt-2">
          <Btn ghost small @click="find([])">Search every language instead</Btn>
        </div>

        <ul class="mt-3 flex flex-col" role="list">
          <li
            v-for="candidate in candidates.slice(0, 25)"
            :key="candidate.file_id"
            class="flex flex-wrap items-center gap-2 border-b border-hairline py-1.5 last:border-0"
          >
            <span
              v-if="candidate.hash_match"
              class="rounded border border-teal px-1.5 py-0.5 font-mono text-[11px] text-teal"
              title="the provider has this exact file’s hash on this subtitle"
            >
              hash
            </span>
            <span class="rounded border border-line px-1.5 py-0.5 font-mono text-[11px] text-dim">
              {{ candidate.language ?? '?' }}
            </span>
            <span class="flex-1 truncate">{{ candidate.release_name ?? '(no name)' }}</span>
            <span class="font-mono text-[11px] text-dim">{{ candidate.downloads }} dl</span>
            <span v-if="candidate.rating" class="font-mono text-[11px] text-dim">
              ★ {{ candidate.rating.toFixed(1) }}
            </span>
            <span v-if="candidate.uploader" class="text-dim">by {{ candidate.uploader }}</span>
            <span
              v-if="drifts(candidate)"
              class="rounded border border-warn px-1.5 py-0.5 font-mono text-[11px] text-warn"
              :title="`timed for ${candidate.fps} fps; this file is ${props.fps?.toFixed(3)} fps`"
            >
              {{ candidate.fps }} fps
            </span>
            <Btn small :disabled="busy" @click="download(candidate)">Download</Btn>
          </li>
        </ul>
      </div>
    </div>
  </section>
</template>
