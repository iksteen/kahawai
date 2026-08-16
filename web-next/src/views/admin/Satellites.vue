<script setup lang="ts">
/// The fleet: what is asking to be let in, and what already is.
///
/// Deleting a satellite is a certificate revocation at the TLS layer, which is
/// why every control here says so and why the delete is asked twice.
import { computed, ref, watch } from 'vue'

import Btn from '../../components/Btn.vue'
import type { CollectionOverview } from '../../api/generated/model/collectionOverview.ts'
import type { PendingEnrollment } from '../../api/generated/model/pendingEnrollment.ts'
import type { SatelliteOverview } from '../../api/generated/model/satelliteOverview.ts'
import {
  adminApprove,
  adminDeleteSatellite,
  adminSetDisabled,
} from '../../api/generated/kahawai.ts'
import { measuredPair, multiple } from '../../domain/admin.ts'
import { notify } from '../../composables/notices.ts'

const props = defineProps<{
  pending: PendingEnrollment[]
  satellites: SatelliteOverview[]
  collections: CollectionOverview[]
  /// Which of the panel's reads are failing, so an empty list is never
  /// reported as an empty fleet.
  broken: readonly string[]
  act: (what: () => Promise<unknown>) => Promise<boolean>
}>()

const code = ref('')
async function approve() {
  const typed = code.value.trim()
  if (!typed) return
  // What the HUB says it admitted, not what was typed at it. Echoing the input
  // reports the operator's own keystrokes back as though they were an answer.
  let admitted = ''
  const ok = await props.act(async () => {
    admitted = (await adminApprove({ code: typed })).approved
  })
  if (!ok) return
  code.value = ''
  notify(`Approved ${admitted}.`)
}

/// What a satellite's drain flag is being set TO, while that write is out.
///
/// Read instead of the row, because the row does not move until the re-read
/// lands: two presses computed `!satellite.disabled` from the same stale value
/// and sent `disabled: true` twice, so "disable — no wait, enable" was silently
/// one write, and the row still read `Disabled — enable`.
const setting = ref<Map<string, boolean>>(new Map())
const drained = (satellite: SatelliteOverview) =>
  setting.value.get(satellite.module_id) ?? satellite.disabled

async function drain(satellite: SatelliteOverview) {
  const want = !drained(satellite)
  setting.value = new Map(setting.value).set(satellite.module_id, want)
  // Held on success until the re-read has landed — dropping it at once flicks
  // the label back to the pre-write value for the length of a round trip — and
  // dropped at once on failure, because the row is the truth again.
  if (await props.act(() => adminSetDisabled(satellite.module_id, { disabled: want }))) return
  const next = new Map(setting.value)
  next.delete(satellite.module_id)
  setting.value = next
}

/// The re-read has landed and agrees: the override has nothing left to say.
watch(
  () => props.satellites,
  (rows) => {
    if (!setting.value.size) return
    const next = new Map(setting.value)
    for (const row of rows) {
      if (next.get(row.module_id) === row.disabled) next.delete(row.module_id)
    }
    if (next.size !== setting.value.size) setting.value = next
  },
)

/// Armed by the first press, disarmed by the second — or by looking away.
/// Without the blur it stayed armed indefinitely, through every fifteen-second
/// poll, and the next click on that row was a delete nobody meant.
///
/// One button whose LABEL changes, rather than a button swapped for a
/// question: swapping it destroys the focused element, so the keyboard is
/// returned to the top of the document and a screen reader is told nothing at
/// all. Relabelling in place keeps focus and announces the new name.
const confirming = ref<string | null>(null)

async function remove(satellite: SatelliteOverview) {
  if (confirming.value !== satellite.module_id) {
    confirming.value = satellite.module_id
    return
  }
  confirming.value = null
  if (!(await props.act(() => adminDeleteSatellite(satellite.module_id)))) return
  notify(
    `Deleted ${satellite.name}: certificate revoked, collections removed. Watch state is archived and restored if the media returns.`,
  )
}

/// MH-8: files a mediahost could not read, per host, because the host is what
/// you would go and look at. A count and no more — `FileError` carries the path
/// and the reason but only the hub's log has them, and saying "three" while
/// pointing at the log beats saying nothing.
const unreadable = computed(() => {
  const byHost = new Map<string, number>()
  for (const collection of props.collections) {
    byHost.set(
      collection.module_id,
      (byHost.get(collection.module_id) ?? 0) + (collection.scan?.failed ?? 0),
    )
  }
  return byHost
})

/// What a transcoder was MEASURED doing, under what it claims it can do.
/// Benchmarks are per element; `pace` is per class of real work and overrides
/// them in placement, so it is shown apart and last.
function facts(satellite: SatelliteOverview) {
  const caps = satellite.capabilities
  const link = satellite.link_bytes_per_sec
  return {
    encoders: caps?.encoders ?? [],
    tonemap: measuredPair(caps?.tonemap_speed_1080, caps?.tonemap_speed_2160),
    link: typeof link === 'number' && link > 0 ? `${(link / 1_000_000).toFixed(1)} MB/s` : null,
    pace: satellite.pace,
  }
}

function some(satellite: SatelliteOverview) {
  const it = facts(satellite)
  return it.encoders.length > 0 || it.tonemap !== null || it.link !== null || it.pace.length > 0
}
</script>

<template>
  <section aria-labelledby="waiting">
    <h2 id="waiting" class="mt-4 mb-2 text-[15px] font-[650]">Waiting to be let in</h2>
    <p v-if="props.broken.includes('enrolments')" class="text-warn">
      This list could not be read, so it is not saying there is nothing.
    </p>
    <p v-else-if="!props.pending.length" class="text-dim">
      None. A new satellite prints its code on its console when it first starts; enter that code
      here to admit it.
    </p>
    <ul v-else class="mb-3 flex flex-col gap-2">
      <!-- Keyed on the CSR, not the module: a satellite restarted before it was
           admitted asks twice, and two requests from one module collide on a
           key that is only the module's name. -->
      <li
        v-for="request in props.pending"
        :key="request.csr_fingerprint"
        class="flex flex-wrap items-center gap-3 rounded border border-line bg-surface p-2"
      >
        <span class="font-[650]" :title="`${request.module_id}\ncsr ${request.csr_fingerprint}`">
          {{ request.name }}
        </span>
        <span class="font-mono text-[12px] text-dim">{{ request.module_type }}</span>
        <!-- The fingerprint of what it is ASKING with, which is what the code
             on its console is proof of. -->
        <span class="truncate font-mono text-[11px] text-dimmer">
          {{ request.csr_fingerprint }}
        </span>
      </li>
    </ul>

    <!-- Approved by the code the satellite prints, not by pressing a row: the
         code is what proves whoever is at this panel can also see that
         machine. -->
    <form class="flex flex-wrap items-center gap-2" @submit.prevent="approve">
      <label class="sr-only" for="enrol-code">Enrolment code</label>
      <input
        id="enrol-code"
        v-model="code"
        class="rounded border border-line bg-bg px-2 py-1 font-mono"
        placeholder="Enrolment code (XXXX-XXXX)"
      />
      <Btn submit small :disabled="!code.trim()">Approve</Btn>
    </form>
  </section>

  <section aria-labelledby="enrolled">
    <h2 id="enrolled" class="mt-6 mb-2 text-[15px] font-[650]">Enrolled</h2>
    <p v-if="props.broken.includes('satellites')" class="text-warn">
      The fleet could not be read, so this is not saying there is nothing enrolled.
    </p>
    <p v-else-if="!props.satellites.length" class="text-dim">No satellites enrolled.</p>
    <ul class="flex flex-col gap-2">
      <li
        v-for="satellite in props.satellites"
        :key="satellite.module_id"
        class="rounded border border-line bg-surface p-2"
      >
        <div class="flex flex-wrap items-center gap-3">
          <span
            class="font-[650]"
            :title="`${satellite.module_id}\ncert ${satellite.cert_fingerprint}`"
          >
            {{ satellite.name }}
          </span>
          <span class="font-mono text-[12px] text-dim">{{ satellite.module_type }}</span>
          <span
            class="rounded px-1.5 py-0.5 font-mono text-[11px]"
            :class="satellite.connected ? 'text-teal' : 'text-warn'"
          >
            {{ satellite.connected ? 'online' : 'offline' }}
          </span>
          <span
            v-if="(unreadable.get(satellite.module_id) ?? 0) > 0"
            class="rounded px-1.5 py-0.5 font-mono text-[11px] text-warn"
            title="Files this host reported it could not read during a scan. They stay known — nothing was dropped from the library — and the hub log names each one."
          >
            {{ unreadable.get(satellite.module_id) }} unreadable
          </span>
          <!-- Only where it does something. The hub accepts the flag for any
               satellite and only placement reads it, so offering this on a
               mediahost gave an operator draining a host a 204, a persisted
               flag, a row that said "Enable" for ever — and a host still
               serving every byte it was asked for. -->
          <Btn
            v-if="satellite.module_type === 'transcoder'"
            ghost
            small
            class="ml-auto"
            :title="
              drained(satellite) ? 'Disabled — no work is sent here' : 'Stop sending work here'
            "
            @click="drain(satellite)"
          >
            {{ drained(satellite) ? 'Disabled — enable' : 'Disable' }}
          </Btn>
          <Btn
            ghost
            small
            :class="satellite.module_type === 'transcoder' ? '' : 'ml-auto'"
            title="Revokes its certificate: it is refused at the TLS layer and cannot come back on its own"
            @click="remove(satellite)"
            @blur="confirming = null"
          >
            {{ confirming === satellite.module_id ? 'Really delete + revoke?' : 'Delete' }}
          </Btn>
        </div>

        <!-- What it was measured doing, underneath. As one flex row the chips
             sat between the name and the buttons and pushed them around, so no
             two rows lined up. -->
        <div
          v-if="satellite.module_type === 'transcoder' && some(satellite)"
          class="mt-1 flex flex-wrap items-center gap-2"
        >
          <span class="font-mono text-[11px] text-dimmer">measured</span>
          <span
            v-for="encoder in facts(satellite).encoders"
            :key="encoder.element"
            class="rounded border border-line px-1.5 py-0.5 font-mono text-[11px] text-dim"
            :title="encoder.element"
          >
            {{ encoder.codec }}{{ encoder.hardware ? ' hw' : ''
            }}{{
              measuredPair(encoder.speed_1080, encoder.speed_2160)
                ? ` ${measuredPair(encoder.speed_1080, encoder.speed_2160)}`
                : ' —'
            }}
          </span>
          <span
            v-if="facts(satellite).tonemap"
            class="rounded border border-line px-1.5 py-0.5 font-mono text-[11px] text-dim"
          >
            tone-map {{ facts(satellite).tonemap }}
          </span>
          <span
            v-if="facts(satellite).link"
            class="rounded border border-line px-1.5 py-0.5 font-mono text-[11px] text-dim"
          >
            link {{ facts(satellite).link }}
          </span>
          <!-- Slower than realtime is the one measurement worth noticing, so it
               is the one that is coloured. -->
          <span
            v-for="row in facts(satellite).pace"
            :key="row.class"
            class="rounded border px-1.5 py-0.5 font-mono text-[11px]"
            :class="
              row.multiple > 0 && row.multiple < 1
                ? 'border-warn text-warn'
                : 'border-line text-dim'
            "
            title="Measured on real sessions; overrides the benchmark"
          >
            {{ row.class }} {{ multiple(row.multiple) ?? 'not measured' }}
          </span>
        </div>

        <div class="mt-1 truncate font-mono text-[11px] text-dimmer">
          {{ satellite.cert_fingerprint }}
        </div>
      </li>
    </ul>
  </section>
</template>
