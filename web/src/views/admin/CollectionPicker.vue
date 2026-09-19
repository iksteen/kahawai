<script setup lang="ts">
import { ref, watch } from 'vue'
import Btn from '../../components/Btn.vue'
import type { CatalogueCollection } from '../../api/generated/model/catalogueCollection.ts'

const selected = defineModel<string[]>({ required: true })
const props = defineProps<{
  collections: CatalogueCollection[]
  label: (id: string) => string
  disabled?: boolean
}>()
// Keep rows in place while ticking boxes or refreshing collection status.
// Only the explicit arrow controls change the displayed order.
const ordered = ref([
  ...selected.value,
  ...props.collections.filter((c) => !selected.value.includes(c.id)).map((c) => c.id),
])
watch([() => props.collections, selected], () => {
  const available = new Set([...props.collections.map((c) => c.id), ...selected.value])
  ordered.value = [
    ...ordered.value.filter((id) => available.has(id)),
    ...[...available].filter((id) => !ordered.value.includes(id)),
  ]
})
const info = (id: string) => props.collections.find((c) => c.id === id)
function toggle(id: string) {
  selected.value = selected.value.includes(id)
    ? selected.value.filter((value) => value !== id)
    : ordered.value.filter((value) => value === id || selected.value.includes(value))
}
function move(index: number, by: number) {
  const next = [...selected.value]
  const from = ordered.value.indexOf(next[index]!)
  const to = ordered.value.indexOf(next[index + by]!)
  ;[ordered.value[from], ordered.value[to]] = [ordered.value[to]!, ordered.value[from]!]
  const [id] = next.splice(index, 1)
  next.splice(index + by, 0, id!)
  selected.value = next
}
</script>

<template>
  <fieldset :disabled="disabled" class="min-w-0">
    <legend class="mb-2 font-[650]">Collections</legend>
    <p class="mb-2 text-[12px] text-dim">
      Select collections of this media type. Earlier collections take priority when items have
      multiple copies.
    </p>
    <p v-if="!ordered.length" class="text-dim">
      No collections of this type have been imported yet. You can save an empty library and add
      collections later.
    </p>
    <ul class="flex flex-col gap-2">
      <li
        v-for="id in ordered"
        :key="id"
        class="flex flex-wrap items-center gap-2 rounded border border-line p-2"
      >
        <label class="flex min-w-0 flex-1 cursor-pointer items-start gap-2">
          <input
            type="checkbox"
            class="mt-1"
            :value="id"
            :checked="selected.includes(id)"
            @change="toggle(id)"
          />
          <span class="min-w-0 break-words">
            {{ label(id) }}
            <span v-if="info(id)" class="text-[12px] text-dim">
              · {{ info(id)!.file_count }} files
              <span v-if="!info(id)!.connected" class="text-warn"> · offline</span>
              <span v-if="info(id)!.scanning"> · scanning</span>
              <span v-else-if="info(id)!.snapshot"> · importing</span>
            </span>
            <span v-else class="text-warn">
              · collection no longer available; remove it before saving</span
            >
            <span
              v-for="root in info(id)?.roots.filter((r) => r.active) ?? []"
              :key="root.id"
              class="block break-all font-mono text-[11px] text-dim"
              >{{ root.path }}</span
            >
          </span>
        </label>
        <span v-if="selected.includes(id)" class="flex gap-1">
          <Btn
            ghost
            small
            :disabled="disabled || selected.indexOf(id) === 0"
            :aria-label="`Move ${label(id)} earlier`"
            @click="move(selected.indexOf(id), -1)"
            >↑</Btn
          >
          <Btn
            ghost
            small
            :disabled="disabled || selected.indexOf(id) === selected.length - 1"
            :aria-label="`Move ${label(id)} later`"
            @click="move(selected.indexOf(id), 1)"
            >↓</Btn
          >
        </span>
      </li>
    </ul>
  </fieldset>
</template>
