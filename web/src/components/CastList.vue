<script setup lang="ts">
import type { Credit } from '../api/generated/model/credit.ts'
import CastPortrait from './CastPortrait.vue'

defineProps<{ cast: Credit[]; item: string; library?: string | null }>()
</script>

<template>
  <section v-if="cast.length" class="mt-8">
    <h2 class="mb-4 text-[14px] font-[650] tracking-[0.08em] text-dim uppercase">Cast</h2>
    <ul class="flex gap-5 overflow-x-auto pb-4" tabindex="0" aria-label="Cast">
      <CastPortrait
        v-for="(person, index) in cast"
        :key="`${person.id ?? person.name}:${person.role ?? ''}:${index}`"
        :person="person"
        :item="item"
        :library="library"
      />
    </ul>
  </section>
</template>
