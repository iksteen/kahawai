<script setup lang="ts">
/// An ordered list you can rearrange: by dragging, and by keyboard.
///
/// UI-12 is the second half. A drag is a mouse gesture and nothing else, so a
/// list that can only be dragged cannot be ordered at all without one — and
/// three lists want this gesture (languages and subtitle fallbacks in
/// Settings, provider precedence in Admin), so getting it wrong in one place
/// is the most it should cost.
///
/// The keyboard version moves one place at a time with the arrow keys while a
/// row is focused. It takes more presses than a drag takes seconds, and it is
/// the difference between fiddly and impossible.
import { nextTick, ref } from 'vue'

import Icon from './Icon.vue'

const props = defineProps<{
  items: string[]
  /// Names the list for whoever cannot see which one this is.
  label: string
  /// Entries that may be reordered but not removed — the audio backstop is
  /// one: it is what makes the list total.
  pinned?: string[]
  /// What to show for an entry, when the stored token is not the word.
  display?: (item: string) => string
}>()

const emit = defineEmits<{ move: [from: number, to: number]; remove: [at: number] }>()

/// The source of the gesture in progress. Read by the drop in the same gesture
/// that set it, before any state has committed — so it is a plain variable,
/// not a ref: a list that changes underneath a lagging ref is how an
/// out-of-range source index gets spliced.
let from: number | null = null
const lifting = ref<number | null>(null)
const over = ref<number | null>(null)

function clear() {
  from = null
  lifting.value = null
  over.value = null
}

function start(at: number, event: DragEvent) {
  from = at
  lifting.value = at
  // Firefox starts no drag at all unless the event carries data, however
  // little. The index is what this needs anyway, and every drop reads it from
  // the variable above rather than from here.
  event.dataTransfer?.setData('text/plain', String(at))
}

function drop(at: number) {
  if (from !== null) emit('move', from, at)
  clear()
}

const rows = ref<HTMLElement[]>([])

/// The keyboard's version. Focus follows the row, or the next press moves a
/// different entry — which is the one thing that makes this unusable.
async function key(at: number, event: KeyboardEvent) {
  const by = event.key === 'ArrowUp' ? -1 : event.key === 'ArrowDown' ? 1 : 0
  if (by === 0) return
  const to = at + by
  if (to < 0 || to >= props.items.length) return
  event.preventDefault()
  emit('move', at, to)
  await nextTick()
  rows.value[to]?.focus()
}
</script>

<template>
  <ul class="flex flex-col gap-1" role="list" :aria-label="props.label">
    <li
      v-for="(item, at) in props.items"
      :key="item"
      ref="rows"
      class="flex cursor-grab items-center gap-2 rounded border border-line bg-surface px-2 py-1"
      :class="[lifting === at && 'opacity-40', over === at && 'border-teal-dim']"
      draggable="true"
      tabindex="0"
      role="listitem"
      :aria-label="`${props.display?.(item) ?? item}, ${at + 1} of ${props.items.length}. Use the arrow keys to move it.`"
      @dragstart="start(at, $event)"
      @dragenter="over = at"
      @dragover.prevent
      @drop="drop(at)"
      @dragend="clear"
      @keydown="key(at, $event)"
    >
      <span class="flex text-dimmer" aria-hidden="true"><Icon name="grip" /></span>
      <span class="flex-1">{{ props.display?.(item) ?? item }}</span>
      <!-- A pinned entry may be moved but not removed: it is what makes the
           list total, and a list without it answers nothing for a file in a
           language nobody named. -->
      <button
        v-if="!props.pinned?.includes(item)"
        class="cursor-pointer px-1 text-dim hover:text-warn"
        type="button"
        :aria-label="`Remove ${props.display?.(item) ?? item}`"
        @click="emit('remove', at)"
      >
        ✕
      </button>
    </li>
  </ul>
</template>
