<script lang="ts">
/// Which button is armed, for the whole page.
///
/// Module scope, so every `Armed` shares it: arming one disarms whichever
/// held it, and no two can be armed at once. That used to fall out of the
/// single `confirming` ref each list kept, and blur is not a substitute —
/// Safari and Firefox on macOS do not focus a button when it is clicked, so
/// no blur fires and the row it left behind stays armed.
///
/// A row unmounted while armed leaves its symbol here, and needs no cleanup
/// for it: nothing else holds that symbol, so every remaining button reads
/// unarmed and the next press overwrites it.
import { ref } from 'vue'

const holder = ref<symbol | null>(null)
</script>

<script setup lang="ts">
/// A destructive button that asks twice.
///
/// The two presses are the SAME button — swapping it for a question would
/// destroy the focused element, dropping a keyboard user at the top of the
/// document and telling a screen reader nothing. Armed, it fills in the
/// warning colour, so the second press does not look like the first. It
/// disarms on blur, Escape and any pointer press outside itself; the outside
/// listener matters on browsers that do not focus a button when it is clicked.
///
/// `name`/`armedName` are for a row of buttons that all read the same word —
/// three "Disconnect"s tell a screen reader's button list nothing apart. Both
/// keep the visible word inside them, so speaking what is on screen still
/// matches the control.
import { computed, onBeforeUnmount, watch } from 'vue'

import Btn from './Btn.vue'

const props = defineProps<{
  label: string
  armedLabel: string
  name?: string
  armedName?: string
  disabled?: boolean
}>()
const emit = defineEmits<{ confirm: [] }>()

/// This instance's claim on `holder`. A symbol, so nothing outside can name
/// it and therefore nothing outside can arm this button.
const mine = Symbol('armed')
const armed = computed(() => holder.value === mine)

/// Undefined leaves the visible text as the accessible name, which is right
/// when the label already says what it acts on.
const named = computed(() => (armed.value ? props.armedName : props.name))

let element: HTMLElement | null = null

function stopListening() {
  document.removeEventListener('pointerdown', outside, true)
  document.removeEventListener('keydown', key, true)
}

function disarm() {
  if (armed.value) holder.value = null
  stopListening()
  element = null
}

function outside(event: PointerEvent) {
  if (!element || event.composedPath().includes(element)) return
  disarm()
}

function key(event: KeyboardEvent) {
  if (event.key === 'Escape') disarm()
}

function press(event: MouseEvent) {
  if (!armed.value) {
    element = event.currentTarget as HTMLElement
    holder.value = mine
    document.addEventListener('pointerdown', outside, true)
    document.addEventListener('keydown', key, true)
    return
  }
  disarm()
  emit('confirm')
}

watch(armed, (isArmed) => {
  if (!isArmed) stopListening()
})
onBeforeUnmount(disarm)
</script>

<template>
  <Btn
    :ghost="!armed"
    :danger="armed"
    small
    :disabled="disabled"
    :aria-label="named"
    @click="press"
    @blur="disarm"
  >
    {{ armed ? armedLabel : label }}
  </Btn>
</template>
