<script setup lang="ts">
/// One item in a library grid, and the same box when it has not arrived yet.
///
/// EVERY CELL IS THE SAME HEIGHT, and that is not a style choice: it is what
/// lets the library reserve its full scroll height up front and be right about
/// it. When titles were free to wrap to one, two or three lines, cards
/// measured 307/331/355px and the reserved height drifted as you scrolled. The
/// title is therefore always exactly two lines — clipped when longer, padded
/// out when shorter — and the meta line always occupies one, which is why it
/// falls back to a dash rather than to nothing.
///
/// The placeholder is structurally the same box for the same reason: a cell
/// that has not arrived must occupy exactly what it will occupy once it does —
/// same art ratio, same two-line title, same one-line meta. Not the swell,
/// though: "there is no poster for this" and "this has not arrived yet" must
/// not look alike.
/// same art ratio, same two-line title, same one-line meta, both of them
/// non-breaking spaces so they take a line box.
///
/// It is also inert both ways: hidden from a screen reader AND not clickable.
/// It was hidden from neither and clicked by the mouse, and the click threw.
///
/// No comment above the root elements below: a comment there is a node, which
/// makes this component multi-root and changes what a parent's layout sees.
import Art from './Art.vue'
import { type Labelled, metaLine } from '../domain/label.ts'

type Row = Labelled & { played: boolean; art_version: number | null; sources?: number }

const props = defineProps<{ item?: Row | undefined }>()
const emit = defineEmits<{ open: [item: Row] }>()

/// Never empty. `metaLine` gives '' for a film with no year, and an empty span
/// takes no line box — one short cell in a grid row makes the whole row short,
/// and the measurement is taken off one cell.
function meta(item: Row): string {
  return [metaLine(item) || '—', (item.sources ?? 0) > 1 ? `${item.sources} sources` : '']
    .filter(Boolean)
    .join(' · ')
}

/// What the badges say, for whoever cannot see them. `title` on a span inside
/// a button contributes nothing to the button's name, so without this a card
/// announces the same whether it is unwatched, half-watched or finished.
function state(item: Row): string {
  if (item.played) return 'seen'
  const at = item.resume_position_ms
  const whole = item.resume_duration_ms
  return at && whole ? 'part-watched' : ''
}
</script>

<template>
  <div
    v-if="!props.item"
    class="card pointer-events-none opacity-50"
    aria-hidden="true"
    data-testid="pending-card"
  >
    <span class="ghost-art" />
    <span class="card-title line-clamp-2 h-[2.7em]">&nbsp;</span>
    <span class="card-meta">&nbsp;</span>
  </div>
  <button
    v-else
    class="card cursor-pointer text-left"
    type="button"
    @click="emit('open', props.item)"
  >
    <Art :item="props.item" size="card" />
    <span class="card-title line-clamp-2 h-[2.7em]">{{ props.item.title }}</span>
    <span class="card-meta">{{ meta(props.item) }}</span>
    <span v-if="state(props.item)" class="sr-only">{{ state(props.item) }}</span>
  </button>
</template>

<style scoped>
@reference '../theme.css';

.card {
  @apply flex w-full flex-col gap-1 rounded-md border border-line bg-surface p-2.5;
}
button.card:hover {
  @apply border-teal-dim;
}
/* The clamp itself is in the class list rather than here: whether a title is
   two lines or free to wrap is the difference between a grid that can reserve
   its own height and one that cannot, and a scoped rule is invisible to a test
   environment with no CSS in it. */
.card-title {
  @apply mt-1 text-[14px] leading-[1.35] font-semibold;
}
.card-meta {
  @apply truncate text-[12px] text-dim;
}
/* The same shape the real art takes, read off the same property. A test can
   see the class but not this rule — happy-dom drops an `aspect-ratio` whose
   value is a `var()`, however it is written — so the height half of "the same
   box" is checked by eye, and the clamp beside it is in the class list
   precisely because that one did not have to be. */
.ghost-art {
  @apply block w-full rounded bg-line opacity-35;
  aspect-ratio: var(--card-ratio, 2 / 3);
}
</style>
