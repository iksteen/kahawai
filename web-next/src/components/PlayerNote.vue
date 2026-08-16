<script setup lang="ts">
/// The one place a failure can be said with the picture fullscreen.
///
/// The shell's notice host is a sibling of the element that goes fullscreen, so
/// while it is, the browser paints only that subtree and a notice raised there
/// is shown to nobody — which is exactly the mode where a freeze is most
/// alarming. This host lives INSIDE the picture.
import { onBeforeUnmount, onMounted, ref } from 'vue'

import { NOTE_MS, onPlayerNote } from '../composables/player-note.ts'

const props = defineProps<{
  /// A dialog owns the screen; a note behind it is a second message about the
  /// same thing, in smaller type.
  hidden?: boolean
}>()

const message = ref('')
let timer: ReturnType<typeof setTimeout> | undefined

onMounted(() =>
  onPlayerNote((said) => {
    message.value = said
    clearTimeout(timer)
    // Latest wins: two failures in a row are usually one failure twice.
    timer = setTimeout(() => (message.value = ''), NOTE_MS)
  }),
)
onBeforeUnmount(() => {
  clearTimeout(timer)
  onPlayerNote(null)
})
</script>

<template>
  <!-- Always in the document, even empty: a live region inserted together with
       its text is commonly announced by nothing at all. -->
  <div
    class="pointer-events-none absolute right-4 bottom-20 left-4 z-7 flex justify-center"
    role="status"
    aria-live="polite"
  >
    <p
      v-if="message && !props.hidden"
      class="pointer-events-auto m-0 max-w-[60ch] rounded-md bg-bg/90 px-4 py-2 text-warn shadow-lg"
    >
      {{ message }}
    </p>
  </div>
</template>
