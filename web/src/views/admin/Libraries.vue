<script setup lang="ts">
/// Draft membership stays local through polls. Only Save replaces the ordered
/// set; committed results update the query cache before another edit is allowed.
import { computed, ref, watch } from 'vue'
import { useQueryClient } from '@tanstack/vue-query'
import Armed from '../../components/Armed.vue'
import Btn from '../../components/Btn.vue'
import CollectionPicker from './CollectionPicker.vue'
import type { CatalogueCollection } from '../../api/generated/model/catalogueCollection.ts'
import type { CatalogueLibrary } from '../../api/generated/model/catalogueLibrary.ts'
import {
  createLibrary,
  deleteLibrary,
  setCollections,
  refreshLibrary,
} from '../../api/generated/kahawai.ts'
import { notify } from '../../composables/notices.ts'

const props = defineProps<{
  libraries: CatalogueLibrary[]
  collections: CatalogueCollection[]
  hosts: { module_id: string; name: string }[]
  loading: boolean
  collectionsLoading: boolean
  broken: readonly string[]
  act: (what: () => Promise<unknown>) => Promise<boolean>
}>()
const client = useQueryClient()
const queryKey = ['admin', 'libraries']
const MEDIA_TYPES = ['movies', 'series', 'anime', 'music']
const name = ref('')
const mediaType = ref('movies')
const initial = ref<string[]>([])
const editing = ref<string | null>(null)
const draft = ref<string[]>([])
const busy = ref(false)
const failed = computed(() => props.broken.includes('libraries'))
const collectionsFailed = computed(() => props.broken.includes('collections'))
const blocked = computed(
  () =>
    busy.value ||
    props.loading ||
    props.collectionsLoading ||
    failed.value ||
    collectionsFailed.value,
)
watch(mediaType, () => {
  initial.value = []
})
const ofType = (type: string) => props.collections.filter((c) => c.media_type === type)
const info = (id: string) => props.collections.find((c) => c.id === id)
function label(id: string) {
  const collection = info(id)
  if (!collection) return id
  const host =
    props.hosts.find((h) => h.module_id === collection.mediahost_id)?.name ??
    (collection.mediahost_id === 'local' ? 'This hub' : collection.mediahost_id)
  return `${host}/${collection.remote_id}`
}
function valid(ids: string[], type: string) {
  return ids.every((id) => info(id)?.media_type === type)
}
async function commit(write: () => Promise<(rows: CatalogueLibrary[]) => CatalogueLibrary[]>) {
  if (blocked.value) return false
  busy.value = true
  try {
    return await props.act(async () => {
      const update = await write()
      // A poll that started before the write must not put old membership back.
      await client.cancelQueries({ queryKey })
      client.setQueryData<CatalogueLibrary[]>(queryKey, (rows) => update(rows ?? props.libraries))
      await Promise.all(
        ['catalogue', 'libraries', 'shelf', 'item', 'children'].map((key) =>
          client.invalidateQueries({ queryKey: [key] }),
        ),
      )
    })
  } finally {
    busy.value = false
  }
}
async function create() {
  if (!name.value.trim() || !valid(initial.value, mediaType.value)) return
  const ok = await commit(async () => {
    const library = await createLibrary({
      name: name.value.trim(),
      media_type: mediaType.value,
      collection_ids: [...initial.value],
    })
    return (rows) => [...rows.filter((row) => row.id !== library.id), library]
  })
  if (ok) {
    name.value = ''
    initial.value = []
    notify('Library created.')
  }
}
function edit(library: CatalogueLibrary) {
  editing.value = library.id
  draft.value = [...library.collection_ids]
}
async function save(library: CatalogueLibrary) {
  if (!valid(draft.value, library.media_type)) return
  const ids = [...draft.value]
  if (
    await commit(async () => {
      await setCollections(library.id, { collection_ids: ids })
      return (rows) =>
        rows.map((row) => (row.id === library.id ? { ...row, collection_ids: ids } : row))
    })
  ) {
    editing.value = null
    notify('Library collections saved.')
  }
}
async function remove(library: CatalogueLibrary) {
  if (
    await commit(async () => {
      await deleteLibrary(library.id)
      return (rows) => rows.filter((row) => row.id !== library.id)
    })
  )
    notify(`Deleted ${library.name}.`)
}
async function rescan(library: CatalogueLibrary, deep: boolean) {
  if (blocked.value) return
  busy.value = true
  try {
    await props.act(async () => {
      const result = await refreshLibrary(library.id, { deep })
      notify(
        `${deep ? 'Deep rescan' : 'Rescan'} requested for ${result.asked} collections${result.offline ? `; ${result.offline} offline` : ''}${result.unsupported ? `; ${result.unsupported} require a mediahost update for deep rescan` : ''}.`,
      )
    })
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <section aria-label="Libraries">
    <form class="mb-5 rounded border border-line bg-surface p-3" @submit.prevent="create">
      <h2 class="mb-3 font-[650]">Create a library</h2>
      <fieldset :disabled="busy" class="mb-3 flex flex-wrap items-end gap-3">
        <label class="flex flex-col gap-1" for="new-library-type"
          >Media type
          <select
            id="new-library-type"
            v-model="mediaType"
            class="h-9 rounded border border-line bg-bg px-2 py-1"
          >
            <option v-for="type in MEDIA_TYPES" :key="type" :value="type">{{ type }}</option>
          </select>
        </label>
        <label class="flex min-w-0 flex-[1_1_160px] flex-col gap-1" for="new-library"
          >Name
          <input
            id="new-library"
            v-model="name"
            class="h-9 rounded border border-line bg-bg px-2 py-1"
            placeholder="e.g. Films"
          />
        </label>
      </fieldset>
      <p v-if="collectionsFailed" class="text-warn">
        Collections could not be read. Try again before changing library membership.
      </p>
      <p v-else-if="collectionsLoading" class="text-dim">Loading collections…</p>
      <CollectionPicker
        v-else
        v-model="initial"
        :collections="ofType(mediaType)"
        :label="label"
        :disabled="blocked"
      />
      <Btn
        submit
        small
        class="mt-3"
        :disabled="blocked || !name.trim() || !valid(initial, mediaType)"
        >Create</Btn
      >
    </form>
    <p v-if="failed" class="text-warn">
      The libraries could not be read, so this is not saying there are none.
    </p>
    <p v-else-if="loading" class="text-dim">Loading libraries…</p>
    <p v-else-if="!libraries.length" class="text-dim">
      No libraries yet. Create one from the collections your mediahosts announce.
    </p>
    <p v-if="failed || collectionsFailed">
      <Btn ghost small @click="client.invalidateQueries({ queryKey: ['admin'] })">Try again</Btn>
    </p>
    <ul class="flex flex-col gap-3">
      <li
        v-for="library in libraries"
        :key="library.id"
        class="min-w-0 rounded border border-line bg-surface p-3"
      >
        <h2 class="break-words font-[650]">
          {{ library.name }}
          <span class="font-mono text-[12px] text-dim">{{ library.media_type }}</span>
        </h2>
        <form v-if="editing === library.id" class="mt-3" @submit.prevent="save(library)">
          <CollectionPicker
            v-model="draft"
            :collections="ofType(library.media_type)"
            :label="label"
            :disabled="blocked"
          />
          <div class="mt-3 flex gap-2">
            <Btn submit small :disabled="blocked || !valid(draft, library.media_type)">{{
              busy ? 'Saving…' : 'Save collections'
            }}</Btn>
            <Btn ghost small :disabled="busy" @click="editing = null">Cancel</Btn>
          </div>
        </form>
        <template v-else>
          <p v-if="!library.collection_ids.length" class="mt-2 text-dim">
            No collections assigned.
          </p>
          <ol class="mt-2 flex list-inside list-decimal flex-col gap-1">
            <li v-for="id in library.collection_ids" :key="id" class="break-words text-[13px]">
              {{ label(id) }}
              <span v-if="info(id)" class="text-dim">
                · {{ info(id)!.file_count }} files
                <span v-if="!info(id)!.connected" class="text-warn"> · offline</span>
                <span v-if="info(id)!.scanning"> · scanning</span>
                <span v-else-if="info(id)!.snapshot"> · importing</span>
              </span>
            </li>
          </ol>
          <div class="mt-3 flex flex-wrap gap-2">
            <Btn
              ghost
              small
              :disabled="blocked || editing !== null"
              :aria-label="`Edit collections in ${library.name}`"
              @click="edit(library)"
              >Edit collections</Btn
            >
            <Btn
              ghost
              small
              :disabled="blocked || !library.collection_ids.length"
              :aria-label="`Rescan ${library.name}`"
              title="Check for new, changed and removed files"
              @click="rescan(library, false)"
              >Rescan</Btn
            >
            <Btn
              ghost
              small
              :disabled="blocked || !library.collection_ids.length"
              :aria-label="`Deep rescan ${library.name}`"
              title="Re-probe every file, including unchanged files"
              @click="rescan(library, true)"
              >Deep rescan</Btn
            >
            <Armed
              label="Delete"
              armed-label="Really delete?"
              :name="`Delete ${library.name}`"
              :armed-name="`Really delete ${library.name}?`"
              :disabled="blocked || editing !== null"
              title="Delete this library"
              @confirm="remove(library)"
            />
          </div>
        </template>
      </li>
    </ul>
  </section>
</template>
