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
import CardFrame from './CardFrame.vue'
import MatchButton from './MatchButton.vue'
import { type Labelled, metaLine } from '../domain/label.ts'

type Row = Labelled & {
  played: boolean
  art_version: number | null
  sources?: number
  match_confidence?: string | null
}

const props = defineProps<{
  item?: Row | undefined
  /// Offer the hand-match affordance. Only an admin has the endpoint, and only
  /// a work has a provider identity to match — an episode inherits its show's.
  matchable?: boolean
}>()
const emit = defineEmits<{ open: [item: Row]; match: [item: Row] }>()

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
  <CardFrame v-if="!props.item" pending />
  <div v-else class="group relative">
    <!-- Over the art rather than in the row: the row is the title and the meta
         line, and a button in it moved them on the cards that had one.
         17px, not 16: the kind and seen badges sit 6px inside the art, and the
         art starts at the card's 1px border plus its 10px padding. At 16 this
         misses its two siblings on the same corner by a pixel. -->
    <MatchButton
      v-if="props.matchable"
      class="absolute top-[17px] right-[17px] z-1"
      :confidence="props.item.match_confidence"
      :label="props.item.title"
      @click="emit('match', props.item)"
    />
    <CardFrame :title="props.item.title" :meta="meta(props.item)" @open="emit('open', props.item)">
      <template #art><Art :item="props.item" size="card" /></template>
      <span v-if="state(props.item)" class="sr-only">{{ state(props.item) }}</span>
    </CardFrame>
  </div>
</template>
