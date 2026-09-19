<script setup lang="ts">
/// HUB-8: the original per-copy matching screen, backed by mediadb.
/// Corrections change a copy's metadata; mediadb owns stable library membership.
import { computed, onBeforeUnmount, onMounted, ref, useTemplateRef, watch } from 'vue'
import { useQueryClient } from '@tanstack/vue-query'
import Btn from './Btn.vue'
import {
  enrichmentDetail,
  enrichmentCorrect,
  enrichmentSearch,
  enrichmentIdentities,
  getEnrichmentArtworkUrl,
  item as catalogueItem,
} from '../api/generated/kahawai.ts'
import type { EnrichmentDetail } from '../api/generated/model/enrichmentDetail.ts'
import type { CollectionCopy } from '../api/generated/model/collectionCopy.ts'
import type { IdentityChoice } from '../api/generated/model/identityChoice.ts'
import { sentence } from '../domain/refusal.ts'
import { sourceLocation } from '../domain/source.ts'

const props = defineProps<{
  item: { id: string; title: string; library_id?: string | null; collection_item_id?: string }
}>()
const emit = defineEmits<{ close: []; applied: [libraryItemIds: string[]] }>()
const client = useQueryClient()
const copies = ref<CollectionCopy[]>([])
const selected = ref('')
const detail = ref<EnrichmentDetail>()
const query = ref('')
const local = ref<IdentityChoice[]>([])
const localMore = ref(false)
const loading = ref(true)
const searching = ref(false)
const saving = ref(false)
const busy = computed(() => loading.value || searching.value || saving.value)
const failure = ref('')
const broken = ref(new Set<string>())
const fileTitle = computed(() => detail.value?.input.title ?? props.item.title)
const fileYear = computed(() => detail.value?.input.year)
const results = computed(() => detail.value?.candidates.filter((c) => !c.rejected) ?? [])
const current = computed(() => {
  const input = detail.value?.input
  if (input?.selected) return { id: input.selected[0], record: input.selected[1] }
  return results.value.find((c) => c.strength === 0)
})
const weak = computed(() => !detail.value?.input.selected && !!current.value)
let asked = 0
let disposed = false
let localQuery = ''
let inflight = ''
const localLabel = (entry: IdentityChoice) => [entry.title, entry.year].filter(Boolean).join(' · ')
const poster = (record: string) =>
  getEnrichmentArtworkUrl(
    selected.value,
    record === detail.value?.input.selected?.[0] ? {} : { record_id: record },
  )

async function localPage(what: string, offset: number, mine: number) {
  const rows = await enrichmentIdentities(selected.value, { q: what, offset, limit: 200 })
  if (mine !== asked) return
  local.value = offset ? [...local.value, ...rows] : rows
  localMore.value = rows.length === 200
  localQuery = what
}
async function moreLocal() {
  if (busy.value) return
  const mine = ++asked
  searching.value = true
  failure.value = ''
  try {
    await localPage(localQuery, local.value.length, mine)
  } catch (cause) {
    if (mine === asked) failure.value = sentence(cause)
  } finally {
    if (mine === asked) searching.value = false
  }
}
async function load() {
  const mine = ++asked
  loading.value = true
  searching.value = false
  failure.value = ''
  detail.value = undefined
  local.value = []
  localMore.value = false
  broken.value = new Set()
  try {
    const answer = await enrichmentDetail(selected.value)
    if (mine !== asked) return
    detail.value = answer
    query.value = answer.input.title
    await localPage(query.value, 0, mine)
  } catch (cause) {
    if (mine === asked) failure.value = sentence(cause)
  } finally {
    if (mine === asked) loading.value = false
  }
}
watch(selected, load)
async function search() {
  const input = detail.value?.input
  const what = query.value.trim()
  const key = what
  if (!input || loading.value || saving.value || !what || (searching.value && key === inflight))
    return
  const mine = ++asked
  inflight = key
  searching.value = true
  failure.value = ''
  try {
    const answer = await enrichmentSearch(input.item_id, { revision: input.revision, query: what })
    if (mine !== asked) return
    detail.value = answer.detail
    local.value = answer.identities
    localMore.value = answer.identities.length === 200
    localQuery = what
    broken.value = new Set()
    failure.value = Object.entries(answer.errors)
      .map(([provider, message]) => `${provider}: ${message}`)
      .join(' ')
  } catch (cause) {
    if (mine === asked) failure.value = sentence(cause)
  } finally {
    if (mine === asked) searching.value = false
  }
}
async function apply(action: string, record_id?: string, library_item_id?: string) {
  const input = detail.value?.input
  if (!input || busy.value) return
  saving.value = true
  failure.value = ''
  try {
    await enrichmentCorrect(input.item_id, {
      revision: input.revision,
      action,
      record_id: record_id ?? null,
      ...(library_item_id ? { library_item_id } : {}),
    })
    const answer = await enrichmentDetail(input.item_id)
    await client.invalidateQueries({
      predicate: (q) =>
        ['catalogue', 'item', 'children', 'shelf', 'libraries'].includes(String(q.queryKey[0])),
      refetchType: 'none',
    })
    if (disposed) return
    emit('applied', [answer.input.library_item_id])
    emit('close')
  } catch (cause) {
    if (!disposed) failure.value = sentence(cause)
  } finally {
    saving.value = false
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
    ...box.value.querySelectorAll<HTMLElement>(
      'button, input, select, summary, a[href], [tabindex]',
    ),
  ].filter((el) => {
    if (
      (el.hasAttribute('tabindex') && el.tabIndex < 0) ||
      el.hasAttribute('disabled') ||
      el.closest('[hidden], [inert]')
    )
      return false
    for (let ancestor = el.parentElement; ancestor; ancestor = ancestor.parentElement) {
      if (
        ancestor.tagName === 'DETAILS' &&
        !ancestor.hasAttribute('open') &&
        !ancestor.querySelector(':scope > summary')?.contains(el)
      )
        return false
    }
    return true
  })
  const edge = event.shiftKey ? stops[0] : stops.at(-1)
  if (document.activeElement !== edge) return
  event.preventDefault()
  ;(event.shiftKey ? stops.at(-1) : stops[0])?.focus()
}

onMounted(async () => {
  restore = document.activeElement as HTMLElement | null
  field.value?.focus()
  window.addEventListener('keydown', keys)
  try {
    if (props.item.collection_item_id) selected.value = props.item.collection_item_id
    else {
      if (!props.item.library_id) throw new Error('This item has no library context.')
      const answer = await catalogueItem(props.item.library_id, props.item.id)
      if (disposed) return
      copies.value = answer.copies
      selected.value = answer.copies[0]?.id ?? ''
    }
    if (!selected.value) throw new Error('This source is no longer available. Reload the item.')
  } catch (cause) {
    if (!disposed) {
      failure.value = sentence(cause)
      loading.value = false
    }
  }
})
onBeforeUnmount(() => {
  disposed = true
  asked++
  window.removeEventListener('keydown', keys)
  restore?.focus()
})
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
          :disabled="loading || saving"
          class="w-full rounded border border-line bg-bg px-2 py-1"
        >
          <option v-for="(entry, index) in copies" :key="entry.id" :value="entry.id">
            {{ index + 1 }} · {{ sourceLocation(entry) }} ·
            {{ entry.paths.join(' + ') || entry.title }}
          </option>
        </select>
      </template>
      <div v-if="detail" class="mt-3 font-mono text-[12px] text-dim">
        <div>
          {{ detail.host_name ?? detail.input.mediahost_id }} · {{ detail.input.remote_id }}
        </div>
        <div v-for="source in detail.input.sources" :key="source.file_id" class="break-all">
          {{ source.root_path }} / {{ source.path }}
        </div>
      </div>
      <p class="mt-2 text-dim">
        This decision applies to the selected copy and all its file parts.
      </p>
      <div class="mt-2 flex gap-2">
        <Btn ghost small :disabled="busy || !current" @click="apply('reject', current?.id)"
          >Reject current</Btn
        >
        <Btn ghost small :disabled="busy || !detail" @click="apply('clear')"
          >Use automatic matching</Btn
        >
      </div>

      <div
        v-if="weak && current"
        class="mt-3 flex flex-wrap items-center gap-3 rounded border border-sand/40 bg-sand/10 p-2"
      >
        <span>
          Uncertain match:
          <b>{{ current.record.title }}</b>
          {{ current.record.year ? ` (${current.record.year})` : '' }} — confirm it or pick a better
          one.
        </span>
        <span class="ml-auto flex gap-2">
          <Btn small :disabled="busy" @click="apply('confirm', current.id)">Confirm current</Btn>
          <Btn ghost small :disabled="busy" @click="apply('reject', current.id)">Reject</Btn>
        </span>
      </div>

      <form class="mt-3 flex flex-wrap items-center gap-2" @submit.prevent="search">
        <label class="sr-only" for="match-query">Search titles</label>
        <input
          id="match-query"
          ref="field"
          v-model="query"
          :disabled="saving"
          class="min-w-0 flex-1 rounded border border-line bg-bg px-2 py-1"
          placeholder="Search titles"
        />
        <Btn submit small :disabled="busy || !query.trim()">{{
          searching ? 'Searching…' : 'Search'
        }}</Btn>
      </form>

      <p v-if="loading" class="mt-2" role="status">Loading matches…</p>
      <div v-if="failure" class="mt-2">
        <p class="text-warn" role="alert">{{ failure }}</p>
        <Btn v-if="selected" small ghost :disabled="busy" @click="load">Reload matches</Btn>
      </div>
      <section v-if="local.length || localMore" class="mt-3">
        <h3>Existing library items</h3>
        <ul class="mt-2 flex flex-col gap-2">
          <li v-for="entry in local" :key="entry.id">
            <Btn ghost small :disabled="busy" @click="apply('assign', undefined, entry.id)">{{
              localLabel(entry)
            }}</Btn>
          </li>
        </ul>
        <Btn v-if="localMore" ghost small class="mt-2" :disabled="busy" @click="moreLocal"
          >Load more library items</Btn
        >
      </section>

      <ul v-if="detail" class="mt-3 grid gap-3" role="list">
        <li v-for="candidate in results" :key="candidate.id">
          <button
            class="flex w-full cursor-pointer flex-col gap-1 rounded-md border border-line bg-bg p-2 text-left hover:border-teal-dim"
            type="button"
            :disabled="busy"
            @click="apply('pick', candidate.id)"
          >
            <!-- A provider with no poster for a candidate gets the swell, like
                 everything else on the site. -->
            <img
              v-if="!broken.has(candidate.id)"
              class="w-full rounded"
              :src="poster(candidate.id)"
              alt=""
              loading="lazy"
              @error="broken = new Set(broken).add(candidate.id)"
            />
            <span v-else class="ghost-art" />
            <span class="line-clamp-2 text-[14px] font-semibold">{{ candidate.record.title }}</span>
            <span class="font-mono text-[12px] text-dim">
              {{ candidate.record.year ?? '—' }} · {{ candidate.record.provider }}
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
