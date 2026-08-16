<script setup lang="ts">
/// Your settings. Everything here saves the moment you change it, which is
/// why every control is optimistic and every failure puts the value back —
/// see `useOptimistic`, which is where the hard part is.
import { computed, ref, watch } from 'vue'

import Failed from '../components/Failed.vue'
import Icon from '../components/Icon.vue'
import Ordered from '../components/Ordered.vue'
import {
  ASS_RUNGS,
  assLadder,
  bandwidthValue,
  ORIGINAL,
  stored,
  validToken,
  wishlist,
} from '../domain/prefs.ts'
import { addAbove, moved } from '../domain/reorder.ts'
import { notify } from '../composables/notices.ts'
import { putPref, usePrefs } from '../composables/prefs.ts'
import { sentence } from '../domain/refusal.ts'
import { useOptimistic } from '../composables/optimistic.ts'

/// Per media type, because what you want from a film is not what you want
/// from anime — HUB-33.
const MEDIA_TYPES = ['movies', 'series', 'anime'] as const

const { query, values, known } = usePrefs()

/// The screen's own copy, seeded from the server's answer and edited in place.
/// An optimistic control needs somewhere to hold a value the server has not
/// confirmed, and a value put back has to land somewhere too.
watch(known, (fresh) => (values.value = { ...fresh }), { immediate: true })

const saved = ref(false)
let flashTimer: ReturnType<typeof setTimeout> | undefined
function flash() {
  saved.value = true
  clearTimeout(flashTimer)
  flashTimer = setTimeout(() => (saved.value = false), 1200)
}

/// One optimistic writer per key. They must not share a revert target: two
/// controls failing at once would put each other's values back.
const writers = new Map<string, ReturnType<typeof useOptimistic<string>>>()
function writerFor(key: string) {
  const held = writers.get(key)
  if (held) return held
  const made = useOptimistic(
    computed({
      get: () => values.value[key] ?? '',
      set: (next: string) => (values.value = { ...values.value, [key]: next }),
    }),
  )
  writers.set(key, made)
  return made
}

async function save(key: string, next: string) {
  const ok = await writerFor(key).put(next, () => putPref('', key, next))
  if (ok) flash()
}

/// A wishlist, as the list the controls work on.
function listFor(kind: 'audio' | 'subs', mediaType: string): string[] {
  return wishlist(values.value[`${kind}.${mediaType}`] ?? '', kind)
}
function saveList(kind: 'audio' | 'subs', mediaType: string, items: string[]) {
  void save(`${kind}.${mediaType}`, stored(items))
}

const typing = ref<Record<string, string>>({})
const rejected = ref<Record<string, boolean>>({})

function add(kind: 'audio' | 'subs', mediaType: string) {
  const field = `${kind}.${mediaType}`
  const token = (typing.value[field] ?? '').trim().toLowerCase()
  if (!validToken(token)) {
    rejected.value = { ...rejected.value, [field]: true }
    return
  }
  const items = listFor(kind, mediaType)
  if (items.includes(token)) {
    // Already there: nothing to save, and nothing to complain about either.
    typing.value = { ...typing.value, [field]: '' }
    return
  }
  rejected.value = { ...rejected.value, [field]: false }
  typing.value = { ...typing.value, [field]: '' }
  // Above the backstop, wherever it sits — a language added after it is never
  // reached, and the setting silently does nothing.
  saveList(kind, mediaType, addAbove(items, token, ORIGINAL))
}

function move(kind: 'audio' | 'subs', mediaType: string, from: number, to: number) {
  const next = moved(listFor(kind, mediaType), from, to)
  if (next) saveList(kind, mediaType, next)
}

function remove(kind: 'audio' | 'subs', mediaType: string, at: number) {
  const items = listFor(kind, mediaType)
  saveList(
    kind,
    mediaType,
    items.filter((_, n) => n !== at),
  )
}

/// The bandwidth box keeps its own copy, so a refusal can put the old value
/// back. Stored as the server stores it: no cap is an empty string, not "0",
/// or the local copy disagrees with the hub about the same key.
const bandwidth = ref('')
watch(
  () => values.value['bandwidth_kbps'],
  (v) => (bandwidth.value = v ?? ''),
  { immediate: true },
)

/// Read off the field rather than off the model: the box is the only control
/// here that is not committed on every keystroke, and one binding fewer
/// between what was typed and what is saved is one fewer thing to be stale.
async function saveBandwidth(typed: string) {
  bandwidth.value = typed
  const next = bandwidthValue(typed.trim() === '0' ? '' : typed)
  if (next === null) {
    notify('That is not a number of kbit/s.')
    bandwidth.value = values.value['bandwidth_kbps'] ?? ''
    return
  }
  if (next === (values.value['bandwidth_kbps'] ?? '')) return
  await save('bandwidth_kbps', next)
  bandwidth.value = values.value['bandwidth_kbps'] ?? ''
}

const ladder = computed(() => assLadder(values.value['subs.ass'] ?? ''))
function moveRung(from: number, to: number) {
  const next = moved(ladder.value, from, to)
  if (next) void save('subs.ass', stored(next))
}

/// HUB-31: which numbering an absolute-numbered series is shown in.
const animeView = computed(() => (values.value['anime_view'] === 'native' ? 'native' : 'seasons'))
</script>

<template>
  <!-- A failed load used to render the page anyway, with every control showing
       its default — which reads as "these are your settings" when the truth is
       "we have no idea what your settings are". Worse than a blank screen,
       because the next thing you do is change one. -->
  <Failed
    v-if="query.isError.value"
    what="Could not load your settings."
    :message="sentence(query.error.value)"
    @retry="query.refetch()"
  />

  <main v-else-if="query.data.value">
    <div class="mb-1 flex items-baseline gap-3">
      <h1 class="text-[22px] font-[650] tracking-[0.01em]">Settings</h1>
      <!-- Always in the layout, only sometimes visible: a chip that appears
           would shift the heading every time you changed something. -->
      <span
        class="rounded px-1.5 py-0.5 font-mono text-[11px] text-teal transition-opacity"
        :class="saved ? 'opacity-100' : 'opacity-0'"
        role="status"
      >
        saved
      </span>
    </div>
    <p class="mb-6 text-dim">Everything here saves the moment you change it.</p>

    <section class="mb-6 rounded-md border border-line bg-surface p-4">
      <h2 class="mb-2 flex items-center gap-2 text-[15px] font-[650]">
        <Icon name="play" :size="15" />
        Playback
      </h2>
      <p class="mb-3 text-dim">Applies wherever you play, on this account.</p>

      <label class="flex flex-wrap items-center gap-3">
        <span class="w-32 font-mono text-[13px] text-dim">bandwidth</span>
        <input
          v-model="bandwidth"
          class="w-48 rounded border border-line bg-bg px-2 py-1 font-mono"
          type="number"
          min="0"
          placeholder="kbit/s cap (0 = none)"
          @blur="saveBandwidth(($event.target as HTMLInputElement).value)"
        />
      </label>
      <p class="mt-2 max-w-[70ch] text-[13px] text-dim">
        A ceiling for how much data playback may use — worth setting on a metered or slow
        connection. Anything above it is re-encoded smaller; a file that cannot be re-encoded will
        refuse to play rather than stall. Leave it empty for no limit.
      </p>
    </section>

    <section class="mb-6 rounded-md border border-line bg-surface p-4">
      <h2 class="mb-2 text-[15px] font-[650]">Styled subtitles</h2>
      <p class="mb-3 max-w-[70ch] text-dim">
        How styled subtitles reach a player that cannot draw them itself, in the order to try. The
        server takes the first rung this client and this server can actually serve.
      </p>
      <Ordered
        :items="ladder"
        label="Styled subtitle fallbacks, in order"
        :display="(rung) => ASS_RUNGS[rung as keyof typeof ASS_RUNGS].name"
        @move="moveRung"
      />
      <ul class="mt-2 flex flex-col gap-1 text-[13px] text-dim">
        <li v-for="rung in ladder" :key="rung">
          <span class="text-prose">{{ ASS_RUNGS[rung].name }}</span> —
          {{ ASS_RUNGS[rung].note }}
        </li>
      </ul>
    </section>

    <section
      v-for="mediaType in MEDIA_TYPES"
      :key="mediaType"
      class="mb-6 rounded-md border border-line bg-surface p-4"
    >
      <h2 class="mb-3 text-[15px] font-[650] capitalize">{{ mediaType }}</h2>

      <div v-for="kind in ['audio', 'subs'] as const" :key="kind" class="mb-4">
        <h3 class="mb-2 font-mono text-[13px] text-dim">
          {{ kind === 'audio' ? 'audio languages' : 'subtitle languages' }}
        </h3>
        <Ordered
          :items="listFor(kind, mediaType)"
          :label="`${kind === 'audio' ? 'Audio' : 'Subtitle'} languages for ${mediaType}, in order`"
          :pinned="kind === 'audio' ? [ORIGINAL] : []"
          @move="(from, to) => move(kind, mediaType, from, to)"
          @remove="(at) => remove(kind, mediaType, at)"
        />
        <div class="mt-2 flex items-center gap-2">
          <label class="sr-only" :for="`${kind}-${mediaType}`">
            Add a language for {{ mediaType }}
          </label>
          <input
            :id="`${kind}-${mediaType}`"
            v-model="typing[`${kind}.${mediaType}`]"
            class="w-32 rounded border border-line bg-bg px-2 py-1 font-mono"
            :class="rejected[`${kind}.${mediaType}`] && 'border-warn'"
            :aria-invalid="rejected[`${kind}.${mediaType}`] || undefined"
            placeholder="en"
            @keydown.enter.prevent="add(kind, mediaType)"
          />
          <button
            class="cursor-pointer rounded border border-line px-2 py-1 hover:border-dim"
            type="button"
            @click="add(kind, mediaType)"
          >
            Add
          </button>
          <span v-if="rejected[`${kind}.${mediaType}`]" class="text-[13px] text-warn" role="alert">
            Two or three letters, like “en” or “nld”.
          </span>
        </div>
      </div>
    </section>

    <section class="mb-6 rounded-md border border-line bg-surface p-4">
      <h2 class="mb-2 text-[15px] font-[650]">Anime numbering</h2>
      <p class="mb-3 max-w-[70ch] text-dim">
        Series numbered straight through can be shown either way. This decides which numbering the
        show page and the season pages use — both, always the same one.
      </p>
      <label class="flex items-center gap-3">
        <span class="w-32 font-mono text-[13px] text-dim">show as</span>
        <select
          class="rounded border border-line bg-bg px-2 py-1"
          :value="animeView"
          @change="save('anime_view', ($event.target as HTMLSelectElement).value)"
        >
          <option value="seasons">seasons</option>
          <option value="native">as numbered in the files</option>
        </select>
      </label>
    </section>
  </main>
</template>
