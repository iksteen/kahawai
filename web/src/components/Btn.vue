<script setup lang="ts">
/// The one button. Teal by default; `ghost` is the secondary offer beside it,
/// which is outlined rather than filled so the pair reads as one choice and
/// one alternative.
/// `submit` because the type is set here rather than passed through: a
/// fallthrough `type` and the template's own would be two answers, and which
/// wins is a rule nobody should have to remember at each call site.
///
/// The three variants past `ghost` are each one rule in the frozen design, and
/// each is here rather than at the call site because a class passed in fights
/// the one below it on stylesheet order rather than on intent:
///
///   `on`      a button whose job is already done — the watched tick. A tick
///             that is already ticked reads as state, not as an offer.
///   `danger`  the second press of a delete, filled in the warning colour so
///             the confirm does not look like the offer that preceded it.
///   `mono`    a button that names a machine thing rather than an action.
withDefaults(
  defineProps<{
    ghost?: boolean
    small?: boolean
    disabled?: boolean
    submit?: boolean
    on?: boolean
    danger?: boolean
    mono?: boolean
  }>(),
  {},
)
</script>

<template>
  <!-- `inline-flex`, and it is not decoration. Preflight lays every `svg` out
       as a block, so a button holding an icon and a label put the icon on a
       line of its own and the words underneath it — the watched tick, two
       lines tall, on every item page. A flex row also centres the two against
       each other, which a baseline never did well.
       Inline-level rather than `flex`, because four of these sit alone in
       block flow as a back link, where a block-level box would change the line
       it makes and the margin under it. -->
  <button
    class="inline-flex cursor-pointer items-center justify-center gap-[7px] rounded border font-[650] disabled:cursor-default disabled:opacity-45 disabled:filter-none"
    :class="[
      danger
        ? 'border-warn bg-warn text-[#2a120b] hover:brightness-108'
        : ghost
          ? on
            ? 'border-teal-dim bg-transparent font-medium text-teal'
            : 'border-line bg-transparent font-medium text-text hover:border-dim'
          : 'border-teal bg-teal text-[#062a25] hover:brightness-108',
      small ? 'px-3 py-1 text-[13px]' : 'px-[18px] py-2',
      // The face only: `.btn.small`'s 13px outranks `.mono`'s 12px on
      // specificity, so the reference never rendered this a size smaller.
      mono && 'font-mono',
    ]"
    :disabled="disabled"
    :type="submit ? 'submit' : 'button'"
  >
    <slot />
  </button>
</template>
