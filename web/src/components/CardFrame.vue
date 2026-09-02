<script setup lang="ts">
/// Shared fixed-height shell for virtualised album and Album Artist grids.
/// The art slot chooses what the square represents; title and metadata retain
/// identical line boxes so PagedGrid can measure either card the same way.
withDefaults(
  defineProps<{
    title?: string
    meta?: string
    pending?: boolean
  }>(),
  { title: '', meta: '', pending: false },
)
const emit = defineEmits<{ open: [] }>()
</script>

<template>
  <div
    v-if="pending"
    class="card pointer-events-none opacity-50"
    aria-hidden="true"
    data-testid="pending-card"
  >
    <span class="ghost-art" />
    <span class="card-title line-clamp-2 h-[2.7em]">&nbsp;</span>
    <span class="card-meta">&nbsp;</span>
  </div>
  <button v-else class="card w-full cursor-pointer text-left" type="button" @click="emit('open')">
    <slot name="art" />
    <span class="card-title line-clamp-2 h-[2.7em]">{{ title }}</span>
    <span class="card-meta">{{ meta || '—' }}</span>
    <slot />
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
.card-title {
  @apply mt-1 text-[14px] leading-[1.35] font-semibold;
}
.card-meta {
  @apply truncate text-[12px] text-dim;
}
.ghost-art {
  @apply block w-full rounded bg-line opacity-35;
  aspect-ratio: var(--card-ratio, 2 / 3);
}
</style>
