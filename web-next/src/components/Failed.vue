<script setup lang="ts">
/// A load that did not work, with a way out of it.
///
/// The screens that fetch something used to render the error string and stop:
/// no retry, no navigation, nothing but a sentence and the header. A page that
/// can only be left by editing the URL is a dead end, and the most common
/// cause — the hub restarted, the wifi blinked — is fixed by asking again.
///
/// The message is kept, in mono and dimmed. It is usually the hub's own words
/// and occasionally the only clue anybody gets; hiding it behind "something
/// went wrong" would be tidier and worse.
import Btn from './Btn.vue'
import Icon from './Icon.vue'

defineProps<{
  /// What could not be loaded, in the viewer's terms.
  what: string
  message: string
  /// Somewhere else to go, named by the caller: the library you came from is a
  /// better offer than "home" when that is where you came from. Omitted where
  /// there is nowhere to go — the home screen's own failure.
  away?: string | undefined
}>()
const emit = defineEmits<{ retry: []; away: [] }>()
</script>

<template>
  <div class="mx-auto mt-[12vh] flex max-w-[460px] flex-col items-center gap-3 px-5 text-center">
    <span class="flex text-warn">
      <Icon name="alert" :size="22" />
    </span>
    <h2 class="text-[17px]">{{ what }}</h2>
    <p class="max-h-[6em] overflow-auto font-mono text-[12px] text-wrap-pretty text-dim">
      {{ message }}
    </p>
    <span class="mt-1 flex gap-2.5">
      <Btn @click="emit('retry')">Try again</Btn>
      <Btn v-if="away" ghost @click="emit('away')">{{ away }}</Btn>
    </span>
  </div>
</template>
