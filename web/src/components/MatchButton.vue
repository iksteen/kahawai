<script setup lang="ts">
import { computed } from 'vue'
import Icon from './Icon.vue'

const props = defineProps<{
  confidence?: string | null | undefined
  label: string
  alwaysVisible?: boolean
}>()

const matching = computed(() => {
  if (props.confidence === 'weak')
    return { tone: 'text-sand', why: 'Uncertain match — review', quiet: false }
  if (props.confidence === 'auto' || props.confidence === 'manual')
    return { tone: 'text-dim', why: 'Re-match metadata', quiet: true }
  return { tone: 'text-warn', why: 'No metadata match — fix', quiet: false }
})
</script>

<template>
  <button
    class="flex h-[22px] w-[22px] shrink-0 cursor-pointer items-center justify-center rounded bg-bg/80 transition-opacity hover:text-teal focus-visible:opacity-100"
    :class="[
      matching.tone,
      !alwaysVisible && matching.quiet && 'opacity-0 group-hover:opacity-100',
    ]"
    type="button"
    :title="matching.why"
    :aria-label="`${matching.why}: ${label}`"
  >
    <Icon name="search" />
  </button>
</template>
