<script setup lang="ts">
/// The one button. Teal by default; `ghost` is the secondary offer beside it,
/// which is outlined rather than filled so the pair reads as one choice and
/// one alternative.
withDefaults(defineProps<{ ghost?: boolean; small?: boolean; disabled?: boolean }>(), {})
</script>

<template>
  <button
    class="cursor-pointer rounded border font-[650] disabled:cursor-default disabled:opacity-45 disabled:filter-none"
    :class="[
      ghost
        ? 'border-line bg-transparent font-normal text-text hover:border-dim'
        : 'border-teal bg-teal text-[#062a25] hover:brightness-108',
      small ? 'px-3 py-1 text-[13px]' : 'px-[18px] py-2',
    ]"
    :disabled="disabled"
    type="button"
  >
    <slot />
  </button>
</template>
