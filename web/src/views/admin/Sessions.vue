<script setup lang="ts">
/// Who is playing what, how it is being delivered, and where.
///
/// The row is written so that an operator can pick WHICH session to end
/// without opening anything: what it is, how it is being served, and how long
/// it has been idle — which is the field they choose by.
import { computed } from 'vue'

import Btn from '../../components/Btn.vue'
import Icon from '../../components/Icon.vue'
import type { AdminSession } from '../../api/generated/model/adminSession.ts'
import { adminEndSession, adminSessionLog } from '../../api/generated/kahawai.ts'
import { deliveryPlan } from '../../domain/source.ts'
import { notify } from '../../composables/notices.ts'
import { saveAs } from '../../api/download.ts'
import { sentence } from '../../domain/refusal.ts'

const props = defineProps<{
  sessions: AdminSession[]
  broken: readonly string[]
  act: (what: () => Promise<unknown>) => Promise<boolean>
}>()

const failed = computed(() => props.broken.includes('sessions'))

/// OPS-10: everything the hub recorded about one session, as a file.
///
/// NOT through `act`: that is for writes. It re-reads all six lists afterwards,
/// and — worse — clears the last refusal on success, so downloading a log would
/// wipe an error the operator had not read yet.
async function log(session: AdminSession) {
  try {
    saveAs(`session-${session.session_id}.log`, await adminSessionLog(session.session_id))
  } catch (cause) {
    notify(`Could not download the session log: ${sentence(cause)}`)
  }
}
</script>

<template>
  <section aria-label="Playback sessions">
    <p v-if="failed" class="text-warn">
      The sessions could not be read, so this is not saying nobody is streaming.
    </p>
    <p v-else-if="!props.sessions.length" class="text-dim">Nobody is streaming.</p>
    <ul class="flex flex-col gap-2">
      <li
        v-for="session in props.sessions"
        :key="session.session_id"
        class="flex flex-wrap items-center gap-3 rounded border border-line bg-surface p-2"
      >
        <!-- What the hub decided to do, in the same words the item page uses:
             `cost` is the negotiated plan and `mode` is what an old client
             asked for by name. -->
        <span
          class="chip"
          :class="`chip-${deliveryPlan(session.streams?.cost ?? session.mode).tone}`"
        >
          {{ deliveryPlan(session.streams?.cost ?? session.mode).chip }}
        </span>
        <!-- The session id, not the host, when there is no title: two untitled
             sessions on one mediahost rendered as the same line, and neither
             named the session the End button was about to kill. -->
        <span class="truncate">{{ session.title ?? session.session_id }}</span>
        <span v-if="session.streams" class="font-mono text-[12px] text-dim">
          v: {{ session.streams.video }} · a: {{ session.streams.audio }}
        </span>
        <span class="text-dim">{{ session.username ?? '?' }}</span>
        <span class="font-mono text-[12px] text-dim">idle {{ session.idle_secs }}s</span>
        <Btn ghost small class="ml-auto" @click="log(session)">Log</Btn>
        <Btn ghost small @click="props.act(() => adminEndSession(session.session_id))">
          <Icon name="signOut" />
          End
        </Btn>
      </li>
    </ul>
  </section>
</template>

<style scoped>
@reference '../../theme.css';

/* Named rather than interpolated into a utility: Tailwind reads the source for
   literal class names, so `text-${tone}` generates nothing at all. */
.chip {
  @apply rounded border border-line px-1.5 py-0.5 font-mono text-[11px];
}
.chip-teal {
  @apply text-teal;
}
.chip-sand {
  @apply text-sand;
}
.chip-warn {
  @apply border-warn text-warn;
}
</style>
