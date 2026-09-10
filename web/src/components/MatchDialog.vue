<script setup lang="ts">
/// HUB-8 hand-matching: provider search prefilled with the FILE's title, a
/// poster grid, one click to pick.
///
/// Anchored on the file identity throughout, and it says so. The display title
/// is the (possibly wrong) match being judged, so heading the dialog with it
/// would make a wrong match look like the thing being searched for.
import { computed, onBeforeUnmount, onMounted, ref, useTemplateRef, watch } from 'vue'

import Btn from './Btn.vue'
import type { ProviderCandidate } from '../api/generated/model/providerCandidate.ts'
import {
  adminApplyMatch,
  adminReviewSearch,
  itemDetail,
  listItems,
} from '../api/generated/kahawai.ts'
import type { CollectionCopy } from '../api/generated/model/collectionCopy.ts'
import type { ItemRowI64 } from '../api/generated/model/itemRowI64.ts'
import { sentence } from '../domain/refusal.ts'

const props = defineProps<{
  item: {
    id: string
    collection_item_id?: string
    kind: string
    title: string
    year?: number | null
    file_title?: string | null
    file_year?: number | null
    matched_title?: string | null
    match_confidence?: string | null
  }
}>()

const emit = defineEmits<{ close: []; applied: [libraryItemIds: string[]] }>()
const copies = ref<CollectionCopy[]>([])
const selected = ref('')
const copy = computed(() => copies.value.find((c) => c.id === selected.value))
const local = ref<ItemRowI64[]>([])
const picked = ref<string[]>([])
const newYear = ref<string>('')

const fileTitle = computed(() => copy.value?.title ?? props.item.file_title ?? props.item.title)
const fileYear = computed(() => copy.value?.year ?? props.item.file_year ?? null)
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
    const entries = await listItems({ q: what, limit: 200 })
    if (mine !== asked) return
    local.value = entries.items.filter((item) => item.kind === props.item.kind)
    if (!['movie', 'series'].includes(props.item.kind)) {
      results.value = []
      return
    }
    const answer = await adminReviewSearch({
      kind: props.item.kind,
      query: what,
      year: fileYear.value,
      item: copy.value?.id ?? null,
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

async function apply(
  action: 'pick' | 'confirm' | 'reject' | 'reset' | 'assign' | 'new',
  candidate?: ProviderCandidate,
  libraryItemIds?: string[],
) {
  if (!copy.value) return
  busy.value = true
  try {
    const changed = await adminApplyMatch(copy.value.id, {
      expected_revision: copy.value.assignment.revision,
      library_item_ids: libraryItemIds ?? null,
      new_item:
        action === 'new'
          ? {
              kind: props.item.kind,
              title: query.value,
              year: newYear.value ? Number(newYear.value) : null,
              artist: copy.value.artist ?? null,
              parent_id: copy.value.parent_library_item_id ?? null,
              season: copy.value.season ?? null,
              episode: copy.value.episode ?? null,
              edition: null,
            }
          : null,
      action,
      provider: candidate?.provider ?? null,
      candidate: candidate ?? null,
    })
    emit('applied', changed.library_item_ids)
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
    ...box.value.querySelectorAll<HTMLElement>('button, input, select, [tabindex="0"]'),
  ].filter((el) => !el.hasAttribute('disabled'))
  const edge = event.shiftKey ? stops[0] : stops.at(-1)
  if (document.activeElement !== edge) return
  event.preventDefault()
  ;(event.shiftKey ? stops.at(-1) : stops[0])?.focus()
}

/// On the WINDOW, not on the backdrop. A key only reaches the backdrop's
/// handler when the focus is inside it, and clicking any prose in the dialog
/// puts the focus on `<body>` — where Escape then did nothing at all.
watch(selected, () => {
  ++asked
  results.value = null
  local.value = []
  picked.value = []
  query.value = fileTitle.value
  newYear.value = fileYear.value?.toString() ?? ''
  void search(query.value)
})
onMounted(async () => {
  restore = document.activeElement as HTMLElement | null
  field.value?.focus()
  window.addEventListener('keydown', keys)
  try {
    const detail = await itemDetail(props.item.id)
    copies.value = detail.copies
    selected.value = props.item.collection_item_id
      ? (copies.value.find((c) => c.id === props.item.collection_item_id)?.id ?? '')
      : (copies.value[0]?.id ?? '')
    if (!selected.value) failure.value = 'This source is no longer available. Reload the item.'
  } catch (cause) {
    failure.value = sentence(cause)
  }
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
      <template v-if="!props.item.collection_item_id">
        <label class="mt-3 block text-dim" for="match-copy">Collection copy</label>
        <select
          id="match-copy"
          v-model="selected"
          class="w-full rounded border border-line bg-bg px-2 py-1"
        >
          <option v-for="entry in copies" :key="entry.id" :value="entry.id">
            {{ entry.collection_id }} · {{ entry.paths.join(' + ') || entry.title }}
          </option>
        </select>
      </template>
      <div v-else-if="copy" class="mt-3 font-mono text-[12px] text-dim">
        <div>{{ copy.collection_id }}</div>
        <div v-for="path in copy.paths" :key="path" class="break-all">{{ path }}</div>
      </div>
      <p v-if="copy?.assignment.conflict" class="mt-2 text-warn">{{ copy.assignment.conflict }}</p>
      <p class="mt-2 text-dim">
        This decision applies to the selected copy and all its file parts.
      </p>
      <div class="mt-2 flex gap-2">
        <Btn ghost small :disabled="busy || !copy" @click="apply('reject')">Reject current</Btn>
        <Btn ghost small :disabled="busy || !copy" @click="apply('reset')"
          >Use automatic matching</Btn
        >
      </div>

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
        <label class="sr-only" for="match-query">Search titles</label>
        <input
          id="match-query"
          ref="field"
          v-model="query"
          class="flex-1 rounded border border-line bg-bg px-2 py-1"
          placeholder="Search titles"
        />
        <Btn submit small :disabled="busy">Search</Btn>
      </form>

      <p class="mt-2 text-warn" role="alert">{{ failure }}</p>
      <section v-if="local.length" class="mt-3">
        <h3>Existing library items</h3>
        <p v-if="props.item.kind === 'episode'" class="text-dim">
          Select the episodes in playback order.
        </p>
        <ul class="mt-2 flex flex-col gap-2">
          <li v-for="entry in local" :key="entry.id">
            <label v-if="props.item.kind === 'episode'"
              ><input v-model="picked" type="checkbox" :value="entry.id" />
              {{ entry.parent_title }} · {{ entry.title }} · {{ entry.season }}/{{
                entry.episode
              }}</label
            >
            <Btn v-else ghost small :disabled="busy" @click="apply('assign', undefined, [entry.id])"
              >{{ entry.title }}{{ entry.year ? ` (${entry.year})` : ''
              }}{{ entry.artist ? ` · ${entry.artist}` : '' }}</Btn
            >
          </li>
        </ul>
        <Btn
          v-if="props.item.kind === 'episode'"
          class="mt-2"
          small
          :disabled="busy || !picked.length"
          @click="apply('assign', undefined, picked)"
          >Assign selected episodes</Btn
        >
      </section>
      <details class="mt-3">
        <summary>Create a distinct library item</summary>
        <p class="mt-2 text-dim">
          Use the title above for an unlisted work, or to distinguish two works with the same title
          and year.
        </p>
        <label class="mt-2 block"
          >Year
          <input
            v-model="newYear"
            type="number"
            min="1"
            max="9999"
            class="rounded border border-line bg-bg px-2 py-1"
        /></label>
        <Btn class="mt-2" small :disabled="busy || !copy || !query.trim()" @click="apply('new')"
          >Create and assign</Btn
        >
      </details>

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
