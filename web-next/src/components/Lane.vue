<script setup lang="ts">
/// A row that scrolls sideways, with arrows that appear on hover and only work
/// on the side there is more to see.
///
/// The measurements come off the element rather than from the number of
/// children: the element knows its own overflow, and a card's width depends on
/// the font. A ResizeObserver catches what a scroll event never reports — the
/// window narrowing — and `onUpdated` catches what the observer never reports:
/// appending cards changes `scrollWidth`, not the element's box, so a page
/// landing after a scroll left the right arrow disabled over a lane that had
/// just grown by twenty cards.
import { onBeforeUnmount, onMounted, onUpdated, ref } from 'vue'

import Icon from './Icon.vue'
import { askAgain, edges } from '../domain/lane.ts'

const props = defineProps<{
  /// How far one press moves. Callers pass a whole number of cards, so a press
  /// always lands on a card boundary.
  step: number
  /// Names the row for whoever cannot see which shelf it belongs to.
  label: string
}>()
const emit = defineEmits<{ nearEnd: [] }>()

const lane = ref<HTMLElement | null>(null)
const more = ref({ left: false, right: false })
/// The width the last ask was fired at, or -1 when the lane is not near its
/// end. See `askAgain` — this runs on every scroll, resize and update.
let firedAt = -1

function read() {
  const el = lane.value
  if (!el) return
  // Same values, same object. This runs on every update — it has to, because
  // appending cards is exactly when the arrows change — and assigning a fresh
  // object each time makes it a render loop: assign, re-render, `onUpdated`,
  // assign.
  const next = edges(el)
  if (next.left !== more.value.left || next.right !== more.value.right) more.value = next
  const again = askAgain(firedAt, el, props.step)
  firedAt = again.firedAt
  if (again.ask) emit('nearEnd')
}

let observer: ResizeObserver | undefined
onMounted(() => {
  read()
  if (typeof ResizeObserver === 'undefined' || !lane.value) return
  observer = new ResizeObserver(read)
  observer.observe(lane.value)
})
onUpdated(read)
onBeforeUnmount(() => observer?.disconnect())

const nudge = (by: number) => lane.value?.scrollBy({ left: by, behavior: 'smooth' })
</script>

<template>
  <div class="lane-wrap group relative">
    <!-- Both arrows are always rendered; the one that cannot move anything is
         dimmed and disabled. An arrow that disappears under the cursor hands
         your click to the card beneath it — a disabled button still occupies
         the hit area and absorbs it.
         `opacity`, not `display`: a hidden button is out of the tab order and
         out of the accessibility tree, so the only keyboard path past the
         first screenful of a lane would be Tab through every card. -->
    <button
      class="nudge left-0 justify-start bg-gradient-to-r from-[rgba(10,16,18,0.92)] to-transparent pl-1 opacity-0 group-hover:opacity-100 focus-visible:opacity-100"
      :aria-label="`Scroll ${props.label} left`"
      type="button"
      :disabled="!more.left"
      @click="nudge(-props.step)"
    >
      <Icon name="chevronLeft" :size="20" />
    </button>
    <div
      ref="lane"
      class="lane flex gap-3 overflow-x-auto scroll-smooth"
      role="group"
      :aria-label="props.label"
      @scroll="read"
    >
      <slot />
    </div>
    <button
      class="nudge right-0 justify-end bg-gradient-to-l from-[rgba(10,16,18,0.92)] to-transparent pr-1 opacity-0 group-hover:opacity-100 focus-visible:opacity-100"
      :aria-label="`Scroll ${props.label} right`"
      type="button"
      :disabled="!more.right"
      @click="nudge(props.step)"
    >
      <Icon name="chevronRight" :size="20" />
    </button>
  </div>
</template>

<style scoped>
@reference '../theme.css';

/* The scrollbar is hidden because the arrows and the cut-off card at the edge
   already say it scrolls, and an always-visible bar under every shelf is four
   horizontal rules across the page. The 2px of side padding keeps a card's
   hover border and focus ring from being clipped by the overflow. */
.lane {
  padding: 2px 2px 12px;
  scrollbar-width: none;
}
.lane::-webkit-scrollbar {
  display: none;
}
/* The reveal itself is in the class list rather than here, so a test can see
   it: whether these are `opacity-0` or `display: none` is the difference
   between an arrow a keyboard can reach and one it cannot, and a scoped rule
   is invisible to a DOM with no CSS in it. */
.nudge {
  @apply absolute top-0.5 bottom-3.5 z-3 inline-flex w-10 cursor-pointer items-center text-text transition-opacity;
}
.nudge:enabled:hover {
  @apply text-teal;
}
.nudge:disabled {
  @apply cursor-default text-dimmer;
}

/* On a finger there is no hover, so the arrows never become visible and are
   still 40px of live hit area over each end: a tap on the card underneath
   scrolled the strip instead of opening anything. A touch screen scrolls a
   lane by dragging it, which is what these exist to substitute for.
   Both halves of the query, because `(hover: none)` alone is also true of
   anything with no pointing device at all — measured: headless Firefox reports
   `hover: none` with `pointer: none`, so the arrows vanished from every
   automated run of this page while a real desktop kept them. */
@media (hover: none) and (pointer: coarse) {
  .nudge {
    display: none;
  }
}
</style>
