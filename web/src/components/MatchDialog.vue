<script setup lang="ts">
/// HUB-8 hand-matching: provider search prefilled with the FILE's title, a
/// poster grid, one click to pick.
///
/// Anchored on the file identity throughout, and it says so. The display title
/// is the (possibly wrong) match being judged, so heading the dialog with it
/// would make a wrong match look like the thing being searched for.
import { computed, onBeforeUnmount, onMounted, ref, useTemplateRef } from 'vue'

import Btn from './Btn.vue'
import type { ProviderCandidate } from '../api/generated/model/providerCandidate.ts'
import { adminApplyMatch, adminReviewSearch } from '../api/generated/kahawai.ts'
import { sentence } from '../domain/refusal.ts'

const props = defineProps<{
  item: {
    id: string
    kind: string
    title: string
    year?: number | null
    file_title?: string | null
    file_year?: number | null
    matched_title?: string | null
    match_confidence?: string | null
  }
}>()

const emit = defineEmits<{ close: []; applied: [] }>()

const fileTitle = computed(() => props.item.file_title ?? props.item.title)
const fileYear = computed(() => props.item.file_year ?? null)
const weak = computed(() => props.item.match_confidence === 'weak')

const query = ref(fileTitle.value)
const results = ref<ProviderCandidate[] | null>(null)
/// Posters the provider named and the browser could not fetch. Without this a
/// dead URL renders the browser's broken-image glyph in a grid of posters.
const broken = ref(new Set<string>())
const busy = ref(false)
const failure = ref('')

/// Which search this is. Two of them in flight and the older one landing last
/// leaves the grid showing candidates for a query nobody typed — and the next
/// click on it APPLIES one.
let asked = 0
/// What the search in flight is for, so Enter on the same text twice is one
/// request. Provider search is rate-limited upstream; a held Enter key should
/// not be what finds that out.
let inflight = ''

async function search(what: string) {
  const mine = ++asked
  inflight = what
  busy.value = true
  failure.value = ''
  try {
    const answer = await adminReviewSearch({
      kind: props.item.kind,
      query: what,
      year: fileYear.value,
      item: props.item.id,
    })
    if (mine !== asked) return
    results.value = answer.candidates
  } catch (cause) {
    if (mine !== asked) return
    failure.value = sentence(cause)
  } finally {
    if (mine === asked) busy.value = false
  }
}

/// Enter in the field submits, and `:disabled` on the button does not stop it.
/// A DIFFERENT query supersedes the one in flight — the sequence guard above
/// makes that safe — and the same one again is nothing to ask twice.
function again() {
  if (busy.value && inflight === query.value) return
  void search(query.value)
}

async function apply(action: 'pick' | 'confirm' | 'reject', candidate?: ProviderCandidate) {
  busy.value = true
  try {
    await adminApplyMatch(props.item.id, {
      action,
      provider: candidate?.provider ?? null,
      candidate: candidate ?? null,
    })
    emit('applied')
    emit('close')
  } catch (cause) {
    failure.value = sentence(cause)
    busy.value = false
  }
}

/// The modal's own keyboard. Escape closes it, and Tab is kept inside: a
/// dialog whose focus wanders onto the page behind it is a dialog only for
/// people using a mouse.
const box = useTemplateRef<HTMLElement>('box')
const field = useTemplateRef<HTMLInputElement>('field')
let restore: HTMLElement | null = null

function keys(event: KeyboardEvent) {
  if (event.key === 'Escape') {
    emit('close')
    return
  }
  if (event.key !== 'Tab' || !box.value) return
  const stops = [
    ...box.value.querySelectorAll<HTMLElement>('button, input, [tabindex="0"]'),
  ].filter((el) => !el.hasAttribute('disabled'))
  const edge = event.shiftKey ? stops[0] : stops.at(-1)
  if (document.activeElement !== edge) return
  event.preventDefault()
  ;(event.shiftKey ? stops.at(-1) : stops[0])?.focus()
}

/// On the WINDOW, not on the backdrop. A key only reaches the backdrop's
/// handler when the focus is inside it, and clicking any prose in the dialog
/// puts the focus on `<body>` — where Escape then did nothing at all.
onMounted(() => {
  restore = document.activeElement as HTMLElement | null
  field.value?.focus()
  window.addEventListener('keydown', keys)
  void search(fileTitle.value)
})
onBeforeUnmount(() => {
  window.removeEventListener('keydown', keys)
  restore?.focus()
})

const year = (candidate: ProviderCandidate) => candidate.release_date?.slice(0, 4) ?? '—'
const format = (candidate: ProviderCandidate) =>
  'format' in candidate && candidate.format ? ` · ${candidate.format}` : ''
</script>

<template>
  <div
    class="fixed inset-0 z-40 flex items-start justify-center overflow-y-auto bg-black/60 p-6"
    @click="emit('close')"
  >
    <div
      ref="box"
      class="w-full max-w-[900px] rounded-lg border border-line bg-surface p-4"
      role="dialog"
      aria-modal="true"
      aria-labelledby="match-title"
      @click.stop
    >
      <div class="flex items-start gap-3">
        <h2 id="match-title" class="text-[17px] font-[650]">
          Match “{{ fileTitle }}”{{ fileYear ? ` (${fileYear})` : '' }}
        </h2>
        <Btn ghost small class="ml-auto" aria-label="Close" @click="emit('close')">✕</Btn>
      </div>
      <!-- Said, rather than left to be inferred: without it a wrong match looks
           like the thing being searched for. -->
      <p class="mt-1 font-mono text-[11px] text-dimmer">
        anchored on the file identity — the display title is the match being judged
      </p>

      <div
        v-if="weak"
        class="mt-3 flex flex-wrap items-center gap-3 rounded border border-sand/40 bg-sand/10 p-2"
      >
        <span>
          Uncertain match:
          <b>{{ props.item.matched_title ?? props.item.title }}</b>
          {{ props.item.year ? ` (${props.item.year})` : '' }} — confirm it or pick a better one.
        </span>
        <span class="ml-auto flex gap-2">
          <Btn small :disabled="busy" @click="apply('confirm')">Confirm current</Btn>
          <Btn ghost small :disabled="busy" @click="apply('reject')">Reject</Btn>
        </span>
      </div>

      <form class="mt-3 flex flex-wrap items-center gap-2" @submit.prevent="again">
        <label class="sr-only" for="match-query">Search providers</label>
        <input
          id="match-query"
          ref="field"
          v-model="query"
          class="flex-1 rounded border border-line bg-bg px-2 py-1"
          placeholder="Search providers"
        />
        <Btn submit small :disabled="busy">Search</Btn>
      </form>

      <p class="mt-2 text-warn" role="alert">{{ failure }}</p>

      <ul v-if="results" class="mt-3 grid gap-3" role="list">
        <li v-for="candidate in results" :key="`${candidate.provider}-${candidate.id}`">
          <button
            class="flex w-full cursor-pointer flex-col gap-1 rounded-md border border-line bg-bg p-2 text-left hover:border-teal-dim"
            type="button"
            :disabled="busy"
            @click="apply('pick', candidate)"
          >
            <!-- A provider with no poster for a candidate gets the swell, like
                 everything else on the site. -->
            <img
              v-if="candidate.poster_url && !broken.has(candidate.poster_url)"
              class="w-full rounded"
              :src="candidate.poster_url"
              alt=""
              loading="lazy"
              @error="broken = new Set(broken).add(candidate.poster_url!)"
            />
            <span v-else class="ghost-art" />
            <span class="line-clamp-2 text-[14px] font-semibold">{{ candidate.title }}</span>
            <span class="font-mono text-[12px] text-dim">
              {{ year(candidate) }} · {{ candidate.provider }}{{ format(candidate) }}
            </span>
          </button>
        </li>
        <li v-if="!results.length" class="text-dim">no candidates — try a different query</li>
      </ul>
    </div>
  </div>
</template>

<style scoped>
@reference '../theme.css';

.grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(140px, 1fr));
}
.ghost-art {
  @apply block w-full rounded bg-line opacity-35;
  aspect-ratio: 2 / 3;
}
</style>
