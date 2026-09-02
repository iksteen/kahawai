<script setup lang="ts">
/// The same browse contract as the library grid: reserve the full result
/// height, keep only rows near the viewport in the DOM, and ask the owner for
/// the chunks those rows occupy. A paged API is an implementation detail, not
/// a reason to put a "load more" interaction in one corner of the UI.
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'

import {
  cellsIn,
  changed,
  CHUNK,
  chunksFor,
  GAP,
  type Metric,
  visibleRows,
} from '../domain/virtual.ts'

const props = defineProps<{
  total: number | null
  minWidth: string
}>()
const emit = defineEmits<{ need: [chunks: number[]] }>()

const metric = ref<Metric | null>(null)
const rows = ref({ start: 0, end: 0 })
const wrap = ref<HTMLElement | null>(null)
const grid = ref<HTMLElement | null>(null)

function measure() {
  const el = grid.value
  const cell = el?.firstElementChild as HTMLElement | null
  if (!el || !cell) return
  const cols = getComputedStyle(el).gridTemplateColumns.split(' ').filter(Boolean).length
  const rowH = cell.getBoundingClientRect().height + GAP
  if (!cols || rowH <= GAP) return
  const now = { cols, rowH }
  if (changed(metric.value, now)) metric.value = now
}

function recompute() {
  if (!wrap.value || !metric.value || props.total === null) return
  const at = visibleRows(
    {
      wrapTop: wrap.value.getBoundingClientRect().top + window.scrollY,
      scrollY: window.scrollY,
      height: window.innerHeight,
    },
    metric.value,
    props.total,
  )
  if (at.start !== rows.value.start || at.end !== rows.value.end) rows.value = at
}

let watching: ResizeObserver | undefined
function remeasure() {
  measure()
  recompute()
}
onMounted(() => {
  window.addEventListener('scroll', recompute, { passive: true })
  void nextTick(() => {
    remeasure()
    if (typeof ResizeObserver === 'undefined' || !grid.value) return
    watching = new ResizeObserver(remeasure)
    watching.observe(grid.value)
  })
})
onBeforeUnmount(() => {
  window.removeEventListener('scroll', recompute)
  watching?.disconnect()
})

watch(
  () => props.total,
  () => void nextTick(remeasure),
)
watch([metric, () => props.total], recompute)
watch(
  [rows, metric, () => props.total],
  () => {
    if (!metric.value || props.total === null) return
    emit('need', chunksFor(rows.value, metric.value, props.total))
  },
  { immediate: true },
)

const cells = computed(() => {
  const count = props.total ?? 0
  if (!metric.value) return Array.from({ length: Math.min(CHUNK, count) }, (_, n) => n)
  return cellsIn(rows.value, metric.value, count)
})
const height = computed(() => {
  if (!metric.value || props.total === null) return undefined
  return Math.max(1, Math.ceil(props.total / metric.value.cols)) * metric.value.rowH - GAP
})
const offset = computed(() =>
  metric.value ? `translateY(${rows.value.start * metric.value.rowH}px)` : undefined,
)
</script>

<template>
  <div ref="wrap" class="relative" :style="height !== undefined ? { height: `${height}px` } : {}">
    <ul
      ref="grid"
      class="absolute top-0 right-0 left-0 grid will-change-transform"
      role="list"
      :style="{
        gridTemplateColumns: `repeat(auto-fill, minmax(${minWidth}, 1fr))`,
        gap: `${GAP}px`,
        ...(offset ? { transform: offset } : {}),
      }"
    >
      <li v-for="at in cells" :key="at" :aria-setsize="total ?? -1" :aria-posinset="at + 1">
        <slot :at="at" />
      </li>
    </ul>
  </div>
</template>
