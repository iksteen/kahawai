<script setup lang="ts">
import { computed, ref, watch } from 'vue'

import type { Credit } from '../api/generated/model/credit.ts'
import { castArtworkUrl } from '../api/artwork.ts'
import Icon from './Icon.vue'

const props = defineProps<{ person: Credit; item: string; library?: string | null | undefined }>()
const source = computed(() =>
  props.library && props.person.id && props.person.image
    ? castArtworkUrl(props.item, props.person.id, props.library, 'card1x', props.person.image)
    : '',
)
const broken = ref(false)
watch(source, () => (broken.value = false))
</script>

<template>
  <li class="w-[112px] shrink-0 text-center">
    <span
      class="relative mx-auto grid size-24 place-items-center overflow-hidden rounded-full border border-line bg-surface text-dim"
    >
      <Icon name="user" :size="32" />
      <img
        v-if="source && !broken"
        :key="source"
        class="absolute inset-0 size-full object-cover"
        :src="source"
        alt=""
        loading="lazy"
        @error="broken = true"
      />
    </span>
    <span class="mt-2 line-clamp-2 min-h-10 text-[13px] font-medium" :title="person.name">{{
      person.name
    }}</span>
    <span
      v-if="person.role"
      class="mt-0.5 line-clamp-2 text-[12px] text-dim"
      :title="person.role"
      >{{ person.role }}</span
    >
  </li>
</template>
