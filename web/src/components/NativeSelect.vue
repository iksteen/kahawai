<script setup lang="ts">
// Keep the options inside this component, rather than in a parent slot. Native
// popups can reset their highlight even when an option's value attribute is
// rewritten unchanged. Stable props keep unrelated parent renders out entirely.
defineProps<{
  value: string | number
  options: readonly { value: string | number; label: string; disabled?: boolean }[]
  disabled?: boolean
}>()
const emit = defineEmits<{ change: [event: Event] }>()
</script>

<template>
  <select :value="value" :disabled="disabled" @change="emit('change', $event)">
    <option
      v-for="option in options"
      :key="option.value"
      :value="option.value"
      :disabled="option.disabled"
    >
      {{ option.label }}
    </option>
  </select>
</template>
