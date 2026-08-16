<script setup lang="ts">
/// Composing libraries out of the collections the mediahosts announce.
///
/// A library is not a folder: it is a NAME over a set of collections, possibly
/// on different hosts, which merge into one browsable thing. That is why the
/// row is a name plus a list of attachments, and why detaching is a small ×
/// rather than a button of its own.
import { computed, ref } from 'vue'

import Btn from '../../components/Btn.vue'
import type { CollectionOverview } from '../../api/generated/model/collectionOverview.ts'
import type { LibraryOverview } from '../../api/generated/model/libraryOverview.ts'
import {
  adminAttachCollection,
  adminCreateLibrary,
  adminDeleteLibrary,
  adminDetachCollection,
  adminRefreshLibrary,
} from '../../api/generated/kahawai.ts'
import { notify } from '../../composables/notices.ts'

const props = defineProps<{
  libraries: LibraryOverview[]
  collections: CollectionOverview[]
  broken: readonly string[]
  act: (what: () => Promise<unknown>) => Promise<boolean>
}>()

const MEDIA_TYPES = ['movies', 'series', 'anime', 'music']

const newLibrary = ref({ name: '', media_type: 'movies' })
async function create() {
  const { name, media_type } = newLibrary.value
  if (!name.trim()) return
  if (!(await props.act(() => adminCreateLibrary({ name: name.trim(), media_type })))) return
  // Cleared BEFORE the re-read, not after it: `act` no longer waits for six
  // requests, and the form that kept its values with Create still live was one
  // Enter away from creating the library twice.
  newLibrary.value = { name: '', media_type }
}

/// What each attachment is, from the collections list — the membership itself
/// carries only ids, and whether the host is connected and how its scan is
/// going is the part an operator is looking for.
function infoFor(member: { module_id: string; collection_id: string }) {
  return props.collections.find(
    (c) => c.module_id === member.module_id && c.collection_id === member.collection_id,
  )
}

/// Collections of this library's own media type that are not already on it.
/// Mixing types would merge a music collection into a film library, and the
/// hub refuses it — so it is not offered.
function attachable(library: LibraryOverview) {
  return props.collections.filter(
    (c) =>
      c.media_type === library.media_type &&
      !library.collections.some(
        (m) => m.module_id === c.module_id && m.collection_id === c.collection_id,
      ),
  )
}

/// Back to "attach…" whichever way it went. It is a MENU, not a value: leaving
/// the picked row selected after a refusal shows a collection that is not
/// attached, and the operator cannot re-pick it to try again because it is
/// still the current option.
async function attach(library: LibraryOverview, event: Event) {
  const select = event.target as HTMLSelectElement
  const collection = attachable(library)[Number(select.value)]
  select.value = ''
  if (!collection) return
  await props.act(() =>
    adminAttachCollection(library.id, {
      module_id: collection.module_id,
      collection_id: collection.collection_id,
    }),
  )
}

async function refresh(library: LibraryOverview) {
  let asked = 0
  let offline = 0
  const ok = await props.act(async () => {
    const answer = await adminRefreshLibrary(library.id)
    asked = answer.asked
    offline = answer.offline
  })
  if (!ok) return
  notify(`Refresh requested: ${asked} collection(s)${offline > 0 ? `, ${offline} offline` : ''}.`)
}

/// Armed, like the satellite and user deletes — and this is the most
/// destructive of the three. It cascades the collection attachments AND every
/// per-user grant, and re-creating the library mints a new id, so every
/// narrowed account has to be granted again by hand: an account granted only
/// this library silently becomes "no access", with nothing on screen to say so.
/// It was the one that went on a single click.
const confirming = ref<string | null>(null)
async function remove(library: LibraryOverview) {
  if (confirming.value !== library.id) {
    confirming.value = library.id
    return
  }
  confirming.value = null
  await props.act(() => adminDeleteLibrary(library.id))
}

/// The attachment key, which has to be the PAIR: one collection id can exist on
/// two hosts.
const keyOf = (m: { module_id: string; collection_id: string }) =>
  `${m.module_id}/${m.collection_id}`

const failed = computed(() => props.broken.includes('libraries'))
</script>

<template>
  <section aria-label="Libraries">
    <form class="mb-4 flex flex-wrap items-center gap-2" @submit.prevent="create">
      <label class="sr-only" for="new-library">New library name</label>
      <input
        id="new-library"
        v-model="newLibrary.name"
        class="rounded border border-line bg-bg px-2 py-1"
        placeholder="new library"
      />
      <label class="sr-only" for="new-library-type">Media type</label>
      <select
        id="new-library-type"
        v-model="newLibrary.media_type"
        class="rounded border border-line bg-bg px-2 py-1"
      >
        <option v-for="type in MEDIA_TYPES" :key="type" :value="type">{{ type }}</option>
      </select>
      <Btn submit small :disabled="!newLibrary.name.trim()">Create</Btn>
    </form>

    <p v-if="failed" class="text-warn">
      The libraries could not be read, so this is not saying there are none.
    </p>
    <p v-else-if="!props.libraries.length" class="text-dim">
      No libraries yet. Create one, then attach the collections your mediahosts announce.
    </p>

    <ul class="flex flex-col gap-2">
      <li
        v-for="library in props.libraries"
        :key="library.id"
        class="flex flex-wrap items-center gap-2 rounded border border-line bg-surface p-2"
      >
        <span class="font-mono text-[12px] text-dim">{{ library.media_type }}</span>
        <span class="font-[650]">{{ library.name }}</span>

        <!-- One chip per attached collection: which host, which collection, and
             how its last scan went. An operator watching a scan, or looking for
             the offline host that is hiding half a library, reads this. -->
        <span
          v-for="member in library.collections"
          :key="keyOf(member)"
          class="flex items-center gap-1 rounded border px-1.5 py-0.5 font-mono text-[11px]"
          :class="
            infoFor(member) && !infoFor(member)!.connected
              ? 'border-warn text-warn'
              : 'border-line text-dim'
          "
        >
          {{ member.host_name ?? member.module_id }}/{{ member.collection_id }}
          <template v-if="infoFor(member) && !infoFor(member)!.connected"> (offline)</template>
          <template v-if="infoFor(member)?.scan">
            ·
            {{ infoFor(member)!.scan!.complete ? 'scanned' : 'scanning' }}
            {{ infoFor(member)!.scan!.scanned
            }}<template v-if="infoFor(member)!.scan!.skipped > 0">
              (+{{ infoFor(member)!.scan!.skipped }} unchanged)</template
            ><template v-if="infoFor(member)!.scan!.failed > 0">
              · {{ infoFor(member)!.scan!.failed }} failed</template
            >
          </template>
          <button
            class="cursor-pointer px-0.5 hover:text-warn"
            type="button"
            :aria-label="`Detach ${member.host_name ?? member.module_id}/${member.collection_id} from ${library.name}`"
            title="detach"
            @click="
              props.act(() =>
                adminDetachCollection(library.id, member.module_id, member.collection_id),
              )
            "
          >
            ×
          </button>
        </span>

        <template v-if="attachable(library).length">
          <label class="sr-only" :for="`attach-${library.id}`">
            Attach a collection to {{ library.name }}
          </label>
          <select
            :id="`attach-${library.id}`"
            class="rounded border border-line bg-bg px-1 py-0.5 font-mono text-[11px]"
            value=""
            @change="attach(library, $event)"
          >
            <option value="">attach…</option>
            <option
              v-for="(collection, index) in attachable(library)"
              :key="keyOf(collection)"
              :value="index"
            >
              {{ collection.host_name ?? collection.module_id }}/{{ collection.collection_id }}
            </option>
          </select>
        </template>

        <Btn
          ghost
          small
          class="ml-auto"
          :disabled="!library.collections.length"
          title="Ask every attached collection's host to scan again"
          @click="refresh(library)"
        >
          Refresh
        </Btn>
        <Btn
          ghost
          small
          title="Removes the library, its collection attachments and every grant to it"
          @click="remove(library)"
          @blur="confirming = null"
        >
          {{ confirming === library.id ? 'Really delete + revoke grants?' : 'Delete' }}
        </Btn>
      </li>
    </ul>
  </section>
</template>
