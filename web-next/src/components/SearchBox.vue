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
import { computed, ref, watch } from 'vue'

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
  /// How many rows the panel drew. A panel showing "No matches" is on screen
  /// with nothing to walk, and taking the arrows there kills the caret
  /// movement in a box whose query matched nothing.
  count: number
  listId: string
  optionId: (index: number) => string
}>()

const emit = defineEmits<{
  'update:modelValue': [string]
  reopen: []
  clear: []
  /// Walking the panel. Reported rather than handled here: the panel owns the
  /// rows, so it is the only thing that knows how far the list goes.
  walk: [delta: number]
  take: []
  dismiss: []
}>()

/// The keyboard, scoped to the search AREA rather than the window: the panel
/// is only reachable through the field it belongs to, so there is no priority
/// question against the menus, the dialogs or the player's own Escape. And
/// only while there is a panel, so a closed box keeps its own keys.
///
/// The area rather than the input alone because focus can legitimately be on
/// the ✕ or on Try again with the panel still up, and Escape has to work from
/// there too.
function onKey(event: KeyboardEvent) {
  // Only while there is a panel. A closed box keeps its own keys exactly as
  // they were — Escape in a library's filter box is the browser's, and taking
  // it dropped the caret out of the field for nothing.
  if (!props.shown) return
  // A composition owns these keys first. Typing Japanese, the arrows walk the
  // IME's candidate list and Enter commits the word — take them and choosing a
  // character navigates into a library instead.
  if (event.isComposing) return
  if (event.key === 'Escape') {
    // Out of the field as well as out of the panel: a dropdown dismissed while
    // the caret is still blinking in the box that opened it reads as a box
    // that stopped working. `preventDefault` because Escape in a search field
    // reverts its value in some browsers, and losing the query was not what
    // was asked for.
    event.preventDefault()
    emit('dismiss')
    ;(document.activeElement as HTMLElement | null)?.blur()
    return
  }
  // Walking and opening belong to the field. Anywhere else in here — the ✕,
  // Try again — the keys are that control's own, and Enter must press the
  // button rather than open a library.
  if (event.target !== input.value || props.count === 0) return
  if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
    // Or the caret jumps to one end of the query while the highlight moves.
    event.preventDefault()
    emit('walk', event.key === 'ArrowDown' ? 1 : -1)
    return
  }
  if (event.key === 'Enter') {
    event.preventDefault()
    emit('take')
  }
}

/// Tab out of the area and the panel goes with you. This replaces closing on
/// the Tab key itself, which could not tell "leaving" from "reaching for the
/// retry button inside the panel" and so made that button mouse-only.
///
/// Only when focus actually landed on something outside: a `relatedTarget` of
/// null means focus went nowhere, which is also what a mousedown on a row
/// looks like in browsers that do not focus buttons on click — closing there
/// would unmount the row before its click could fire.
function onFocusOut(event: FocusEvent) {
  const to = event.relatedTarget as Node | null
  if (to && !area.value?.contains(to)) emit('dismiss')
}

const area = ref<HTMLElement | null>(null)

/// The panel scrolls at 70vh, so the highlight can walk out of sight.
/// `nearest` rather than `center`: it moves things only as far as it must,
/// which on a long panel whose bottom is past the fold means the page comes
/// along — the alternative is a lit row nobody can see.
watch(
  () => props.highlight,
  (at) => {
    if (at < 0) return
    document.getElementById(props.optionId(at))?.scrollIntoView({ block: 'nearest' })
  },
)

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
  <div
    ref="area"
    class="relative min-w-[150px] max-w-[420px] flex-[1_1_200px]"
    @keydown="onKey"
    @focusout="onFocusOut"
  >
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
