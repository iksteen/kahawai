<script setup lang="ts">
/// The one search box.
///
/// Its ARIA is a combobox only where a panel exists. A library page's box is a
/// filter with nothing to pop up, and telling a screen reader it is a combobox
/// would promise a list that never arrives.
///
/// `aria-expanded` comes from the PANEL rather than being guessed from "is it
/// open" and a row count: only the panel knows whether it drew anything, and a
/// query that matched nothing draws a message with no rows in it. Saying
/// "collapsed" there would hide the two states somebody would most need read
/// out. `aria-controls` likewise names the list only while there is one — the
/// rest of the time it would point at an id that is not in the document.
///
/// `type="text"`, not `type="search"`. A search input brings the UA's own
/// clear button, which would sit on top of this one, and in some browsers
/// Escape reverts the field — losing the query is not what Escape is for here.
import { computed, ref } from 'vue'

import Icon from './Icon.vue'

const props = defineProps<{
  modelValue: string
  /// Does this screen have a panel at all?
  panel: boolean
  /// What the panel reports about itself: whether it drew, and which row is
  /// lit. Walking the highlight never moves focus off this input — that is
  /// what `aria-activedescendant` is for, and taking focus onto a row would
  /// take the caret out of the field somebody is still typing in.
  shown: boolean
  highlight: number
  listId: string
  optionId: (index: number) => string
}>()

const emit = defineEmits<{
  'update:modelValue': [string]
  reopen: []
  clear: []
}>()

const input = ref<HTMLInputElement | null>(null)
defineExpose({ focus: () => input.value?.focus() })

const label = computed(() => (props.panel ? 'Search all libraries' : 'Filter this library'))

const combobox = computed(() =>
  props.panel
    ? {
        role: 'combobox',
        'aria-autocomplete': 'list' as const,
        'aria-controls': props.shown ? props.listId : undefined,
        'aria-expanded': props.shown,
        'aria-activedescendant': props.highlight >= 0 ? props.optionId(props.highlight) : undefined,
      }
    : {},
)
</script>

<template>
  <!-- `min-w-0` because a flex item's floor is its intrinsic width otherwise,
       which pushed the header off a 375px screen sideways. It stops growing at
       420 for the opposite reason: a search box the width of the page is a
       banner. -->
  <div class="relative min-w-[150px] max-w-[420px] flex-[1_1_200px]">
    <span class="pointer-events-none absolute top-1/2 left-2.5 flex -translate-y-1/2 text-dimmer">
      <Icon name="search" />
    </span>
    <input
      ref="input"
      v-bind="combobox"
      class="relative z-16 w-full rounded-full border border-line bg-surface px-[30px] py-[7px] text-[13.5px] text-text placeholder:text-dim"
      :value="modelValue"
      :placeholder="label"
      :aria-label="label"
      type="text"
      @input="emit('update:modelValue', ($event.target as HTMLInputElement).value)"
      @focus="emit('reopen')"
      @click="emit('reopen')"
    />
    <!-- Both this and the field sit above the panel's click-catcher. Below it,
         clicking into your own query to fix a typo landed on the catcher, and
         the ✕ took two clicks: one to close the panel and one to clear. -->
    <button
      v-if="modelValue !== ''"
      class="absolute top-1/2 right-2 z-16 -translate-y-1/2 cursor-pointer p-0.5 text-dim hover:text-text"
      title="Clear"
      aria-label="Clear the search"
      type="button"
      @click="emit('clear')"
    >
      ✕
    </button>
    <slot />
  </div>
</template>
