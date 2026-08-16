<script setup lang="ts">
/// The capability mask editor — see `domain/capability-mask.ts` for what a mask
/// is and why it only ever subtracts.
///
/// Rendered in two places, because a mask only takes effect on the NEXT
/// session: on the item page, where it is set before playback starts, and in
/// the player, beside the verdict it changes — drop a codec, restart, and read
/// what the hub decided instead.
import { ref } from 'vue'

import Btn from './Btn.vue'
import type { CapabilityMask } from '../domain/capability-mask.ts'
import { buildProfile, loadMask, probedProfile, saveMask } from '../api/capabilities.ts'
import { maskSummary } from '../domain/capability-mask.ts'

const props = defineProps<{
  /// Restart playback with the new mask. Absent on the item page, where there
  /// is nothing running to restart.
  applying?: boolean
  onApply?: (() => void) | undefined
}>()

/// Told after every edit, so a mask badge outside this panel can stop showing
/// what the mask USED to be.
const emit = defineEmits<{ change: [] }>()

type DropKind = 'video' | 'audio' | 'containers'
type Flag = 'hdr' | 'ass_render' | 'graphics_overlay' | 'vtt_render'

const probed = probedProfile()
const mask = ref<CapabilityMask>(loadMask())
const copied = ref(false)

function update(next: CapabilityMask) {
  mask.value = next
  saveMask(next)
  emit('change')
}

const dropped = (kind: DropKind, name: string) => !!mask.value[kind]?.includes(name)

function toggleDrop(kind: DropKind, name: string) {
  const current = mask.value[kind] ?? []
  const next = { ...mask.value }
  const without = current.filter((n) => n !== name)
  if (current.includes(name)) {
    if (without.length) next[kind] = without
    else delete next[kind]
  } else {
    next[kind] = [...current, name]
  }
  update(next)
}

const declares = (flag: Flag) => mask.value[flag] ?? probed[flag] ?? false

function setFlag(flag: Flag, value: boolean) {
  const next = { ...mask.value }
  // Back at the probe's own answer is no override at all, so the summary never
  // reports a mask that changes nothing.
  if (value === probed[flag]) delete next[flag]
  else next[flag] = value
  update(next)
}

function setCeiling(key: 'max_height' | 'max_audio_channels', value: number) {
  const next = { ...mask.value }
  if (value > 0) next[key] = value
  else delete next[key]
  update(next)
}

const target = () => mask.value.target_duration ?? probed.target_duration
const shortSecs = () => {
  const current = mask.value.target_duration
  return current?.mode === 'short' ? current.max_secs : 6
}

function setTarget(mode: 'ignore' | 'accurate' | 'short') {
  update({
    ...mask.value,
    target_duration: mode === 'short' ? { mode, max_secs: 6 } : { mode },
  })
}

async function copyProfile() {
  // The same profile as JSON, for `kahawai-play.sh -P` and `kahawai-sweep
  // --profile`: a mask reproduced outside the browser sweeps the whole library
  // the same way. Without the source-aware refinements, which depend on the
  // item rather than on the browser.
  try {
    await navigator.clipboard?.writeText(JSON.stringify(buildProfile(), null, 2))
    copied.value = true
    setTimeout(() => (copied.value = false), 1500)
  } catch {
    // No clipboard permission: nothing to say that the button not changing
    // does not already say.
  }
}

const rows: { kind: DropKind; label: string; names: () => string[] }[] = [
  { kind: 'video', label: 'video', names: () => (probed.video ?? []).map((c) => c.codec) },
  { kind: 'audio', label: 'audio', names: () => probed.audio ?? [] },
  { kind: 'containers', label: 'container', names: () => probed.containers ?? [] },
]

const FLAGS: Flag[] = ['hdr', 'ass_render', 'graphics_overlay', 'vtt_render']
</script>

<template>
  <div class="caps font-mono text-[12px]">
    <p class="mb-3 max-w-[80ch] text-dim">
      Unchecking removes a capability from the profile sent to the hub — and from what this player
      renders — so the negotiation takes the branch a lesser client would. Encodes target h264/aac,
      so dropping those makes sources that need an encode honestly UNPLAYABLE: that refusal is the
      branch under test.
    </p>

    <div v-for="row in rows" :key="row.kind" class="row">
      <span class="label">{{ row.label }}</span>
      <span class="opts">
        <span v-if="!row.names().length" class="text-dim">none probed</span>
        <label
          v-for="name in row.names()"
          :key="name"
          :class="dropped(row.kind, name) && 'text-dimmer line-through'"
        >
          <input
            type="checkbox"
            :checked="!dropped(row.kind, name)"
            @change="toggleDrop(row.kind, name)"
          />
          {{ name }}
        </label>
      </span>
    </div>

    <div class="row">
      <span class="label">declares</span>
      <span class="opts">
        <label v-for="flag in FLAGS" :key="flag" :class="!declares(flag) && 'text-dimmer'">
          <input
            type="checkbox"
            :checked="declares(flag)"
            @change="setFlag(flag, ($event.target as HTMLInputElement).checked)"
          />
          {{ flag }}
        </label>
      </span>
    </div>

    <!-- The one declaration that is not a boolean: correctness, latency and
         encode cost, pick two. `short` is the only mode that can force a video
         encode, so it carries the ceiling it is buying. -->
    <div class="row">
      <span class="label">targetduration</span>
      <span class="opts">
        <label
          v-for="mode in ['ignore', 'accurate', 'short'] as const"
          :key="mode"
          :class="target().mode !== mode && 'text-dimmer'"
        >
          <input
            type="radio"
            name="targetduration"
            :checked="target().mode === mode"
            @change="setTarget(mode)"
          />
          {{ mode }}
        </label>
        <label v-if="target().mode === 'short'">
          max
          <select
            :value="shortSecs()"
            @change="
              update({
                ...mask,
                target_duration: {
                  mode: 'short',
                  max_secs: Number(($event.target as HTMLSelectElement).value),
                },
              })
            "
          >
            <option v-for="n in [2, 4, 6, 10]" :key="n" :value="n">{{ n }}s</option>
          </select>
        </label>
      </span>
    </div>

    <div class="row">
      <span class="label">ceilings</span>
      <span class="opts">
        <label>
          height
          <select
            :value="mask.max_height ?? 0"
            @change="setCeiling('max_height', Number(($event.target as HTMLSelectElement).value))"
          >
            <option :value="0">none</option>
            <option v-for="n in [2160, 1080, 720, 480]" :key="n" :value="n">{{ n }}</option>
          </select>
        </label>
        <label>
          channels
          <select
            :value="mask.max_audio_channels ?? 0"
            @change="
              setCeiling('max_audio_channels', Number(($event.target as HTMLSelectElement).value))
            "
          >
            <option :value="0">unlimited</option>
            <option v-for="n in [6, 2, 1]" :key="n" :value="n">{{ n }}</option>
          </select>
        </label>
      </span>
    </div>

    <div class="mt-3 flex flex-wrap items-center gap-2">
      <Btn v-if="props.onApply" small :disabled="props.applying ?? false" @click="props.onApply()">
        {{ props.applying ? 'restarting…' : 'apply & restart' }}
      </Btn>
      <Btn ghost small :disabled="!maskSummary(mask).length" @click="update({})">reset</Btn>
      <Btn ghost small @click="copyProfile">{{ copied ? 'copied' : 'copy profile json' }}</Btn>
      <!-- Only when there is a mask. Unmasked is the ordinary state of this
           panel, and saying so on every visit spends a line to report that
           nothing has happened — the checkboxes above already show it. -->
      <span v-if="maskSummary(mask).length" class="text-dim">
        masked: {{ maskSummary(mask).join(' ') }}
      </span>
    </div>
  </div>
</template>

<style scoped>
@reference '../theme.css';

.caps {
  @apply rounded-md border border-line bg-surface p-3;
}
.row {
  @apply flex flex-wrap items-baseline gap-2 border-b border-hairline py-1.5 last:border-0;
}
.label {
  @apply w-28 shrink-0 text-dim;
}
.opts {
  @apply flex flex-wrap items-center gap-3;
}
.opts label {
  @apply flex cursor-pointer items-center gap-1;
}
.opts select {
  @apply ml-1 rounded border border-line bg-bg px-1;
}
</style>
