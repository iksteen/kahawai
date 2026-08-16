<script setup lang="ts">
/// A row in a popover. `here` is where you already are: filled, lit, and with
/// a lit glyph, so the menu answers "where am I" as well as "where can I go".
///
/// `leaving` is sign-out. Not destructive, but not ordinary either — it warms
/// on hover instead of lighting.
import Icon, { type IconName } from './Icon.vue'

defineProps<{ here?: boolean; leaving?: boolean; glyph?: IconName }>()
</script>

<template>
  <button
    class="flex w-full items-center gap-2 px-3 py-1.5 text-left"
    :class="[
      here ? 'bg-hover text-teal' : 'text-text',
      leaving ? 'hover:bg-hover hover:text-warn' : 'hover:bg-hover',
    ]"
    :aria-current="here ? 'page' : undefined"
    role="menuitem"
    type="button"
  >
    <span v-if="glyph" class="flex w-3.5 shrink-0 justify-center" :class="here && 'text-teal'">
      <Icon :name="glyph" />
    </span>
    <slot />
  </button>
</template>
