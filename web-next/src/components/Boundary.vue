<script setup lang="ts">
/// The last line: a screen that throws while rendering takes the app with it
/// and leaves a white page — no header, no way back, nothing to report but "it
/// went blank".
///
/// `onErrorCaptured` returning `false` stops the error propagating to the next
/// boundary up, which is what makes this a boundary rather than a spectator.
///
/// Note what this does NOT catch, so nobody trusts it too far: throws in event
/// handlers and in anything asynchronous never reach it. Those are the
/// `catch` blocks' job, and the screens that fetch handle them with `Failed`.
///
/// `resetKey` is which screen this is — see `boundaryKey`. Changing it clears
/// a latched error, so leaving a broken screen is enough; the same screen
/// re-rendering is not, or the failure would flicker back on every tick.
import { onErrorCaptured, ref, watch } from 'vue'

import Failed from './Failed.vue'

const props = defineProps<{ resetKey: string; away?: string | undefined }>()
const emit = defineEmits<{ away: [] }>()

const failure = ref<Error | null>(null)

onErrorCaptured((error) => {
  failure.value = error instanceof Error ? error : new Error(String(error))
  // The console is where a stack is actually readable, and this is a bug
  // rather than a condition — somebody should be able to find which component
  // died without reproducing it twice.
  console.error('render failed', error)
  return false
})

watch(
  () => props.resetKey,
  () => (failure.value = null),
)

/// Re-render the same screen from scratch. A render throw is usually a shape
/// the screen did not expect; asking again is worth one press before reaching
/// for a reload.
///
/// Clearing the failure is the whole of it. An earlier draft also bumped a
/// counter and put it on the slot as a `:key`, to "rebuild the subtree" — no
/// test could tell the difference with it removed, and a key on a slot outlet
/// is not a mechanism I could show working. A claim in a comment that nothing
/// can demonstrate is worse than no comment.
function retry() {
  failure.value = null
}
</script>

<template>
  <Failed
    v-if="failure"
    what="This screen stopped working."
    :message="failure.message || String(failure)"
    :away="props.away"
    @retry="retry"
    @away="emit('away')"
  />
  <!-- Leaving is not cleared here: the caller navigates, `resetKey` changes,
       and the watcher above does it. One mechanism, and it is the tested one.
       A navigation that does not happen — a guard refusing it — correctly
       leaves the failure on screen. -->
  <slot v-else />
</template>
