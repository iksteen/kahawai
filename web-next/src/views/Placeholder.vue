<script setup lang="ts">
/// Stands in for every page until its phase builds it. Named routes and the
/// shell are phase 4; the pages are 5 onward, and each replaces one line of
/// `router.ts`.
import { useRoute } from 'vue-router'

const route = useRoute()
</script>

<template>
  <main class="py-8">
    <h2 class="text-lg">{{ String(route.name) }}</h2>
    <p class="mt-2 text-dim">Not built yet.</p>
    <pre v-if="Object.keys(route.params).length" class="mt-4 font-mono text-sm text-dimmer">{{
      route.params
    }}</pre>
  </main>
</template>
