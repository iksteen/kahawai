<script setup lang="ts">
/// One library's shelf: what arrived lately in it.
///
/// Three states and they are all different things. Pending is a lane of
/// ghosts, so the page has its shape and does not jump when the answer lands.
/// Failed says so and offers the button — an empty shelf is dropped by the
/// screen above, and a failed one that looked empty deleted whole libraries
/// from the home screen with nothing said.
import { ref, useId } from 'vue'

import Art from './Art.vue'
import Btn from './Btn.vue'
import Lane from './Lane.vue'
import { cardRatio, type Shelf } from '../domain/shelves.ts'
import { metaLine, targetOf } from '../domain/label.ts'

/// One card's width, and how much of a shelf a press of an arrow moves. Kept
/// here rather than measured: a shelf scrolls by whole cards, so the number
/// that decides the step is the same one the layout uses.
const CARD_PX = 150
const STEP = CARD_PX * 3

const props = defineProps<{ shelf: Shelf }>()
const emit = defineEmits<{
  open: [library: string]
  openItem: [id: string, library: string]
  nearEnd: []
  retry: [done: () => void]
}>()

const heading = useId()

/// Ghosts while the retry is out, so the row keeps its height and its place
/// instead of the page jumping when the answer arrives.
///
/// Cleared either way. The old shelf cleared it only on failure, because a
/// success remounted the component and threw the flag away with it; nothing
/// remounts here, so a shelf that recovered went on ghosting over its own
/// cards — the heading said "1 of 1" above a row of blanks.
const retrying = ref(false)
function askAgain() {
  retrying.value = true
  emit('retry', () => {
    retrying.value = false
  })
}
</script>

<template>
  <section
    class="mb-7"
    :style="{ '--card-ratio': cardRatio(props.shelf.library.media_type) }"
    :aria-labelledby="heading"
    :aria-busy="props.shelf.state === 'pending' || retrying"
  >
    <!-- A heading, so the shelves can be walked as headings — and a button,
         because pressing it opens the library. -->
    <h2 :id="heading" class="mt-8 mb-3 flex items-baseline gap-3">
      <button
        class="flex cursor-pointer items-baseline gap-2 text-[16px] font-[650] hover:text-teal"
        type="button"
        @click="emit('open', props.shelf.library.id)"
      >
        {{ props.shelf.library.name }}
        <span class="text-[12px] text-dimmer" aria-hidden="true">→</span>
      </button>
      <span class="text-[11.5px] font-normal text-dim">latest added</span>
      <span v-if="props.shelf.state === 'ready'" class="text-[11.5px] font-normal text-dimmer">
        {{ props.shelf.items.length }} of {{ props.shelf.total }}
      </span>
    </h2>

    <!-- Ghosts at the height a real lane would be. Half strength on the WHOLE
         card rather than just its picture, so a shelf part-way through loading
         reads as unfinished at a glance instead of as a row of oddly blank
         cards — and no swell behind them, because "there is no poster" and
         "this has not arrived" must not look alike. -->
    <div
      v-if="props.shelf.state === 'pending' || retrying"
      class="flex gap-3 overflow-hidden px-0.5 opacity-50"
      aria-hidden="true"
    >
      <div
        v-for="n in 8"
        :key="n"
        class="flex w-[150px] shrink-0 flex-col gap-1 rounded-md border border-line bg-surface p-2.5"
      >
        <span class="ghost-art" />
        <span class="mt-0.5 block h-[17px] w-3/4 rounded bg-line opacity-35" />
        <span class="block h-[14px] w-1/2 rounded bg-line opacity-35" />
      </div>
    </div>

    <div v-else-if="props.shelf.state === 'failed'" class="flex items-center gap-3 px-0.5 py-5">
      <span class="text-dim">This one would not load.</span>
      <!-- Named, because every failed shelf offers one of these and a list of
           nine identical "Try again" buttons names nothing. -->
      <Btn
        ghost
        small
        :aria-label="`Try loading ${props.shelf.library.name} again`"
        @click="askAgain"
      >
        Try again
      </Btn>
    </div>

    <Lane v-else :step="STEP" :label="props.shelf.library.name" @near-end="emit('nearEnd')">
      <button
        v-for="item in props.shelf.items"
        :key="item.id"
        class="flex w-[150px] shrink-0 cursor-pointer flex-col gap-1 rounded-md border border-line bg-surface p-2.5 text-left hover:border-teal-dim"
        type="button"
        @click="emit('openItem', targetOf(item), props.shelf.library.id)"
      >
        <Art :item="item" size="card" />
        <!-- One line each. A shelf is scanned along, not read down, so a card
             that grows a second line of title makes the whole rail taller. -->
        <span class="mt-0.5 truncate text-[13px] leading-[1.3] font-semibold">{{
          item.title
        }}</span>
        <span class="truncate font-mono text-[11px] text-dim">{{ metaLine(item) }}</span>
      </button>
    </Lane>
  </section>
</template>

<style scoped>
@reference '../theme.css';

/* The same shape the real card's art takes, read off the shelf — so a ghost in
   the music row comes out square like the album cards beside it. */
.ghost-art {
  @apply block w-full rounded bg-line opacity-35;
  aspect-ratio: var(--card-ratio, 2 / 3);
}
</style>
