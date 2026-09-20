<script setup lang="ts">
/// Every background queue the deployment runs, in one place.
///
/// Three areas, one shape: what is left, what is done, when the clock will
/// next move it, and the last thing that went wrong. The hub used to say
/// this only in its log — "round complete" lines nobody was reading — and the
/// media-analysis counts lived under Providers, where an operator looking
/// for a stuck queue would not think to look.
///
/// Rerun is offered only where it does something: enrichment and subtitle
/// queues are durable tables the hub can release; discovery selects its own
/// work on the mediahost.
import { computed, onUnmounted, ref, watch } from 'vue'
import { useQuery, useQueryClient } from '@tanstack/vue-query'

import Btn from '../../components/Btn.vue'
import { workRerun, workStatus } from '../../api/generated/kahawai.ts'
import type { WorkQueue } from '../../api/generated/model/workQueue.ts'
import { POLL_MS } from '../../composables/admin.ts'
import { notify } from '../../composables/notices.ts'
import {
  byArea,
  dueIn,
  needsAttention,
  progress,
  queueLabel,
  remaining,
} from '../../domain/work.ts'
import { sentence } from '../../domain/refusal.ts'

const props = defineProps<{
  act: (what: () => Promise<unknown>) => Promise<boolean>
}>()

const client = useQueryClient()
const work = useQuery({
  queryKey: ['admin', 'work'],
  queryFn: () => workStatus(),
  refetchInterval: POLL_MS,
})

/// The clock for "in 4 min", ticking with the poll so a wait counts down
/// rather than sitting at whatever it said when the page opened.
const now = ref(Math.floor(Date.now() / 1000))
const clock = setInterval(() => (now.value = Math.floor(Date.now() / 1000)), 1000)
onUnmounted(() => clearInterval(clock))

const groups = computed(() => byArea(work.data.value?.queues ?? []))

/// Said once, on the way into failure and once on the way out, like the
/// Providers panel: a notice every fifteen seconds would be worse than silence.
let failing = false
watch(
  () => (work.isError.value ? sentence(work.error.value) : ''),
  (why) => {
    if (why && !failing) {
      failing = true
      notify('Cannot reach the hub — what is shown here may be out of date.')
    } else if (!why && failing) {
      failing = false
      notify('Background work is up to date again.')
    }
  },
)

const rerunning = ref<string | null>(null)
const key = (q: WorkQueue) => `${q.area}/${q.queue}`

async function rerun(q: WorkQueue) {
  rerunning.value = key(q)
  try {
    if (await props.act(() => workRerun({ area: q.area, queue: q.queue }))) {
      notify(`${queueLabel(q.queue)} queued again.`)
      await client.invalidateQueries({ queryKey: ['admin', 'work'] })
    }
  } finally {
    rerunning.value = null
  }
}

/// Host and collection for a discovery row; nothing for a hub queue.
const where = (q: WorkQueue) =>
  q.host && q.collection ? `${q.host}/${q.collection}` : (q.host ?? q.collection ?? '')
</script>

<template>
  <div class="flex flex-col gap-4">
    <p v-if="work.isError.value" class="text-warn">
      Could not read background work: {{ sentence(work.error.value) }}
      <Btn ghost small @click="work.refetch()">Try again</Btn>
    </p>
    <p v-else-if="!work.data.value" class="text-dim">Loading background work…</p>
    <p v-else-if="!groups.length" class="text-dim">
      Nothing to do: no libraries are composed yet, and no mediahost has reported.
    </p>

    <section
      v-for="group in groups"
      :key="group.area.id"
      :aria-labelledby="`work-${group.area.id}`"
      class="rounded border border-line bg-surface p-3"
    >
      <h2 :id="`work-${group.area.id}`" class="mb-1 text-[14px] font-[600]">
        {{ group.area.label }}
      </h2>
      <p class="mb-3 text-dim">{{ group.area.intro }}</p>
      <!-- A table, because these are rows of the same five facts and an
           operator scans a column: which queue is blocked, which one is
           waiting on the clock. Wide content scrolls inside the section. -->
      <div class="overflow-x-auto">
        <table class="w-full border-collapse text-[13px]">
          <thead>
            <tr class="text-left text-dim">
              <th class="pr-3 pb-1 font-[500]">Queue</th>
              <th v-if="group.area.id === 'discovery'" class="pr-3 pb-1 font-[500]">Where</th>
              <th class="pr-3 pb-1 font-[500]">Remaining</th>
              <th v-if="group.area.id !== 'discovery'" class="pr-3 pb-1 font-[500]">Done</th>
              <th class="pr-3 pb-1 font-[500]">Next</th>
              <th class="pb-1"></th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="q in group.queues"
              :key="key(q) + where(q)"
              class="border-t border-line align-top"
              :class="needsAttention(q) && 'text-warn'"
            >
              <td class="py-1.5 pr-3 whitespace-nowrap">{{ queueLabel(q.queue) }}</td>
              <td v-if="group.area.id === 'discovery'" class="py-1.5 pr-3 break-words">
                {{ where(q) }}
              </td>
              <td class="py-1.5 pr-3">
                {{ remaining(q) }}
                <!-- The last error, under its count: an operator reading
                     "1 blocked" wants the why on the same row. -->
                <p v-if="q.error" class="mt-0.5 max-w-[420px] text-[12px] break-words">
                  {{ q.error }}
                </p>
              </td>
              <td
                v-if="group.area.id !== 'discovery'"
                class="py-1.5 pr-3 font-mono whitespace-nowrap"
              >
                <template v-if="progress(q).pct != null">
                  {{ progress(q).done }} / {{ progress(q).total }}
                  <span class="text-dim">({{ progress(q).pct }}%)</span>
                </template>
                <span v-else class="text-dim">—</span>
              </td>
              <td class="py-1.5 pr-3 whitespace-nowrap text-dim">{{ dueIn(q, now) ?? '' }}</td>
              <td class="py-1 text-right">
                <Btn v-if="q.rerun" ghost small :disabled="rerunning === key(q)" @click="rerun(q)">
                  {{ rerunning === key(q) ? 'Queuing…' : 'Rerun' }}
                </Btn>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </section>
  </div>
</template>
