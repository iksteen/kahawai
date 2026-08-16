<script setup lang="ts">
/// A popover and the sheet that closes it.
///
/// The sheet is a SIBLING rather than a document listener: a click outside
/// lands on it and nowhere else, so nothing behind the menu acts on the click
/// that dismissed it. Its z-14 and the menu's z-20 are the old stylesheet's
/// numbers, and the gaps in the sequence are load-bearing: the search box sits
/// at 16 so it stays clickable while a menu is open, and the music dock at 13
/// so dismissing a menu over the bar does not also press its transport.
///
/// Escape closes it, and the listener exists only while it is open — so it
/// cannot swallow an Escape the player wants.
///
/// `role="menu"` is a promise, and this is the rest of it: arrow keys, Home,
/// End, and focus that goes into the menu on open and back to the trigger on
/// close. A menuitem puts a screen reader into focus mode, where the reader's
/// own browse keys stop working — so a menu that does not implement the arrow
/// keys leaves that user with nothing that moves.
import { nextTick, onBeforeUnmount, ref, watch } from 'vue'

const props = defineProps<{ open: boolean; align: 'left' | 'right' }>()
const emit = defineEmits<{ close: [] }>()

const menu = ref<HTMLElement | null>(null)

const items = () =>
  menu.value ? [...menu.value.querySelectorAll<HTMLElement>('[role="menuitem"]')] : []

/// Where focus came from, so it can go back. Reading it at open time rather
/// than keeping a trigger prop: whatever was focused is what the user will
/// expect to return to, and the trigger is not always it (a click focuses the
/// button in some browsers and nothing at all in others).
let returnTo: HTMLElement | null = null

function move(by: number, from: number) {
  const all = items()
  if (all.length === 0) return
  const at = from === -1 ? (by > 0 ? 0 : all.length - 1) : from
  // Wrapping, per the menu pattern: Down from the last item is the first.
  all[(at + by + all.length) % all.length]!.focus()
}

function onKey(event: KeyboardEvent) {
  if (event.key === 'Escape') {
    emit('close')
    return
  }
  const all = items()
  const at = all.indexOf(document.activeElement as HTMLElement)
  const step: Record<string, () => void> = {
    ArrowDown: () => move(1, at),
    ArrowUp: () => move(-1, at),
    Home: () => all[0]?.focus(),
    End: () => all.at(-1)?.focus(),
    // Tab out of a menu closes it rather than leaving an open menu behind a
    // sheet the keyboard cannot reach.
    Tab: () => emit('close'),
  }
  const go = step[event.key]
  if (!go) return
  // Not for Tab: preventing that would trap focus, and this menu is dismissed
  // by leaving rather than escaped from.
  if (event.key !== 'Tab') event.preventDefault()
  go()
}

watch(
  () => props.open,
  async (open) => {
    if (open) {
      returnTo = document.activeElement as HTMLElement | null
      window.addEventListener('keydown', onKey)
      await nextTick()
      items()[0]?.focus()
      return
    }
    window.removeEventListener('keydown', onKey)
    // Only if the menu still HAS focus, asked before the DOM updates: this
    // watcher runs pre-flush, so the focused row is still in the document and
    // `activeElement` does not become `<body>` until afterwards. Asking after
    // would be too late to tell "the menu had focus" from "the user clicked
    // into something else", and yanking focus out of what they clicked is the
    // failure this guard exists for.
    const held = !!menu.value?.contains(document.activeElement)
    await nextTick()
    if (held) returnTo?.focus()
    returnTo = null
  },
  { immediate: true },
)
onBeforeUnmount(() => window.removeEventListener('keydown', onKey))
</script>

<template>
  <template v-if="open">
    <div class="fixed inset-0 z-14" data-testid="menu-sheet" @click="emit('close')" />
    <div
      ref="menu"
      class="animate-rise-pop absolute top-full z-20 mt-1 min-w-48 rounded-md border border-line bg-surface py-1 shadow-lg"
      :class="align === 'right' ? 'right-0' : 'left-0'"
      role="menu"
    >
      <slot />
    </div>
  </template>
</template>
