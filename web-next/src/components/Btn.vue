<script setup lang="ts">
/// The one button. Teal by default; `ghost` is the secondary offer beside it,
/// which is outlined rather than filled so the pair reads as one choice and
/// one alternative.
/// `submit` because the type is set here rather than passed through: a
/// fallthrough `type` and the template's own would be two answers, and which
/// wins is a rule nobody should have to remember at each call site.
withDefaults(
  defineProps<{ ghost?: boolean; small?: boolean; disabled?: boolean; submit?: boolean }>(),
  {},
)
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
    :type="submit ? 'submit' : 'button'"
  >
    <slot />
  </button>
</template>
