<script setup lang="ts">
// The clean room's whole surface, until phase 4 puts the shell here. It exists
// so `npm run dev`, `npm run build` and the test run have something real to
// prove they work on.
const wordmark = 'kahawai'
</script>

<template>
  <main class="mx-auto max-w-[1200px] px-[clamp(14px,4vw,32px)] py-8">
    <h1 class="text-2xl">{{ wordmark }}<span class="text-teal">~</span></h1>
    <p class="text-dim">The rebuilt interface. Nothing here yet.</p>
  </main>
</template>
