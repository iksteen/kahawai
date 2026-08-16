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
///
/// TWO shapes, because the design has two and they are not interchangeable. A
/// language is a word: a dozen of them belong in a row, as pills, beside the
/// label that names them. A fallback rung is a sentence: those belong in
/// stacked rows. Rendering both as full-width rows put one language per line
/// under a heading of its own — a column of mostly empty boxes.
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
  /// Pills in a row rather than stacked rows.
  chips?: boolean
  /// What each entry means, shown beside it. Rows with a note are laid out as
  /// one grid across the whole list — subgrid, so every explanation starts at
  /// the same place. As independent rows the notes began wherever each name
  /// happened to end, and "burnt into the picture" is a lot wider than
  /// "plain text".
  note?: (item: string) => string
  /// Nothing here can be removed at all. The fallback ladder is the case: the
  /// order expresses priority, never removal, so every rung is always present
  /// and a ✕ on each one offered something that does not exist.
  fixed?: boolean
}>()

const emit = defineEmits<{
  move: [from: number, to: number]
  remove: [at: number]
  promote: [at: number]
}>()

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

const shown = (item: string) => props.display?.(item) ?? item

/// Ids for the notes, so a row can point at its own with `aria-describedby`.
/// The row's `aria-label` REPLACES its content for a screen reader, so a note
/// rendered inside it would otherwise be read by nobody.
const uid = Math.random().toString(36).slice(2, 8)
const noteId = (at: number) => `ord-${uid}-${at}`
</script>

<template>
  <ul
    :class="
      chips
        ? 'flex flex-wrap items-center gap-1.5'
        : note
          ? 'grid grid-cols-[auto_auto_1fr] gap-x-[9px] gap-y-1'
          : 'flex flex-col gap-1'
    "
    role="list"
    :aria-label="props.label"
  >
    <li
      v-for="(item, at) in props.items"
      :key="item"
      ref="rows"
      class="cursor-grab items-center"
      :class="[
        chips
          ? 'flex min-h-7 gap-[3px] rounded border border-teal-dim py-[3px] pr-1 pl-1.5 text-[12px]'
          : note
            ? 'col-span-full grid grid-cols-subgrid rounded border border-hairline px-2 py-[5px]'
            : 'flex gap-2 rounded border border-line bg-surface px-2 py-1',
        lifting === at && 'opacity-40',
        over === at && 'border-teal bg-teal/8',
      ]"
      draggable="true"
      tabindex="0"
      role="listitem"
      :aria-label="`${shown(item)}, ${at + 1} of ${props.items.length}. Use the arrow keys to move it.`"
      :aria-describedby="note ? noteId(at) : undefined"
      @dragstart="start(at, $event)"
      @dragenter="over = at"
      @dragover.prevent
      @drop="drop(at)"
      @dragend="clear"
      @keydown="key(at, $event)"
    >
      <span class="flex text-dimmer" aria-hidden="true"
        ><Icon name="grip" :size="chips ? 10 : 14"
      /></span>

      <!-- A pill's name promotes on click. A drag is a pointer gesture, and
           the same outcome has to be reachable without one — the arrow keys
           are the other half, and this is the one-press version. -->
      <button
        v-if="chips"
        class="cursor-pointer border-0 bg-transparent p-0 font-mono text-[12px] text-teal"
        type="button"
        :title="at === 0 ? 'first choice' : 'make it the first choice'"
        :aria-label="
          at === 0 ? `${shown(item)}, first choice` : `Make ${shown(item)} the first choice`
        "
        @click="at > 0 && emit('promote', at)"
      >
        {{ shown(item) }}
      </button>
      <span v-else :class="note ? '' : 'flex-1'">{{ shown(item) }}</span>
      <span v-if="note" :id="noteId(at)" class="text-[12px] text-dim">{{ note(item) }}</span>

      <!-- The lock and the ✕ share one box: an icon has no line box and a
           glyph has a tall one, so without this a pinned pill stood taller
           than the rest. -->
      <span
        v-if="props.pinned?.includes(item)"
        class="flex h-[15px] w-[15px] shrink-0 items-center justify-center text-dimmer"
        title="always the final fallback"
      >
        <Icon name="lock" :size="10" />
      </span>
      <button
        v-else-if="!fixed"
        class="flex h-[15px] w-[15px] shrink-0 cursor-pointer items-center justify-center text-[12px] leading-none text-dim hover:text-warn"
        type="button"
        :aria-label="`Remove ${shown(item)}`"
        @click="emit('remove', at)"
      >
        ✕
      </button>
    </li>
  </ul>
</template>
