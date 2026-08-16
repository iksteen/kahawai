<script setup lang="ts">
/// The operator's panel: the fleet, the libraries composed from it, the
/// providers that describe them, the accounts that may see them, and who is
/// watching what.
///
/// One flat scroll put five unrelated jobs in one column, and the two that
/// change on their own — a satellite waiting to be admitted, somebody
/// streaming — were as likely to be off-screen as not. A tab can carry a
/// count, so those two announce themselves from wherever you are.
///
/// Each section says what it IS, because the concepts here are the ones nobody
/// else on the page explains: that deleting a satellite is a certificate
/// revocation at the TLS layer, and that a grant chip writes the whole set
/// rather than a change to it.
import { computed, nextTick, ref, useTemplateRef } from 'vue'

import AdminLibraries from './admin/Libraries.vue'
import AdminProviders from './admin/Providers.vue'
import AdminSatellites from './admin/Satellites.vue'
import AdminSessions from './admin/Sessions.vue'
import AdminUsers from './admin/Users.vue'
import Failed from '../components/Failed.vue'
import { useAdmin } from '../composables/admin.ts'
import { useHints } from '../composables/hints.ts'

const SECTIONS = [
  {
    id: 'satellites',
    label: 'Satellites',
    intro:
      'Enrolled mediahosts and transcoders. This list is the mTLS allowlist — deleting a satellite revokes its certificate, so it is refused at the TLS layer and cannot come back on its own.',
  },
  {
    id: 'libraries',
    label: 'Libraries',
    intro:
      'Compose libraries from the collections mediahosts announce. Same-type collections from different hosts merge into one browsable library; duplicate items become extra sources.',
  },
  {
    id: 'providers',
    label: 'Providers',
    intro: 'Credentials for the services that identify and describe your media.',
  },
  {
    id: 'users',
    label: 'Users & grants',
    intro:
      'Each account gets every library until you narrow it. A chip writes the account’s whole access, not a change to it, so two admins editing at once cannot merge into a set neither picked.',
  },
  {
    id: 'sessions',
    label: 'Sessions',
    intro: 'Who is playing what, how it is being delivered, and where.',
  },
] as const

type SectionId = (typeof SECTIONS)[number]['id']

const tab = ref<SectionId>('satellites')
const here = computed(() => SECTIONS.find((s) => s.id === tab.value)!)

const admin = useAdmin()

/// HUB-11. The poll is the safety net; this is what makes a scan's progress and
/// a satellite asking to be let in arrive when they happen rather than up to
/// fifteen seconds later. A scan hint fires every five hundred files and can
/// only change two of the six reads, so it only invalidates those.
useHints({
  always: ['enrollments', 'satellites', 'sessions', 'libraries', 'collections'],
  quiet: ['users', 'providers', 'enrich'],
})

/// Counts, and only for the two that change without you: a number that never
/// moves is furniture.
const badge = (id: SectionId) =>
  id === 'satellites'
    ? admin.enrollments.value.length
    : id === 'sessions'
      ? admin.sessions.value.length
      : 0

/// The real tab pattern, not buttons that look like one. `role="tablist"` is a
/// promise about the keys — arrows move between tabs and only the selected one
/// is in the tab order — so it is made good here rather than claimed.
const tabs = useTemplateRef<HTMLButtonElement[]>('tabs')

async function go(id: SectionId) {
  tab.value = id
  // A refusal is an answer to something the operator did on the screen they
  // were on. Carried onto another tab it reads as that tab's news.
  admin.actionError.value = ''
  await nextTick()
  // By id, not by index: Vue does not promise a `v-for` ref array is in source
  // order, and focusing the wrong tab is a silent, occasional bug.
  tabs.value?.find((t) => t.id === `tab-${id}`)?.focus()
}

function key(event: KeyboardEvent, at: number) {
  const by = event.key === 'ArrowRight' ? 1 : event.key === 'ArrowLeft' ? -1 : 0
  const to =
    event.key === 'Home' ? 0 : event.key === 'End' ? SECTIONS.length - 1 : by ? at + by : -1
  if (to < 0 || to >= SECTIONS.length) return
  event.preventDefault()
  void go(SECTIONS[to]!.id)
}

/// The panel is focusable but NOT focused. That is the tabs pattern: the arrow
/// keys move the focus with the selection, so it stays on the tablist, and
/// `tabindex="-1"` is what makes the next Tab press land in the panel rather
/// than skipping past everything the tab just revealed. Focusing it here
/// instead took the focus off the tab the arrow keys had just moved to.
</script>

<template>
  <main>
    <h1 class="mb-1 text-[22px] font-[650] tracking-[0.01em]">Admin · {{ here.label }}</h1>

    <div class="mt-4 mb-3 flex flex-wrap gap-1" role="tablist" aria-label="Admin sections">
      <button
        v-for="(section, at) in SECTIONS"
        :id="`tab-${section.id}`"
        :key="section.id"
        ref="tabs"
        class="flex cursor-pointer items-center gap-1.5 rounded-t border border-b-0 border-line px-3 py-1.5"
        :class="tab === section.id ? 'bg-surface text-teal' : 'text-dim hover:text-text'"
        type="button"
        role="tab"
        :aria-selected="tab === section.id"
        :aria-controls="`panel-${section.id}`"
        :tabindex="tab === section.id ? 0 : -1"
        @click="go(section.id)"
        @keydown="key($event, at)"
      >
        {{ section.label }}
        <!-- A satellite waiting to be let in is the one thing here that needs
             you now, so its count is coloured until you go and look. -->
        <span
          v-if="badge(section.id) > 0"
          class="rounded-full px-1.5 font-mono text-[11px]"
          :class="
            section.id === 'satellites' && tab !== 'satellites'
              ? 'bg-warn/20 text-warn'
              : 'bg-line text-dim'
          "
        >
          {{ badge(section.id) }}
        </span>
      </button>
    </div>
    <p class="mb-4 max-w-[80ch] text-dim">{{ here.intro }}</p>

    <!-- Both live regions exist from the first render and only their TEXT
         changes. A node inserted with its content already in it is not
         reliably announced, and this is the only channel that reports a
         refused delete.
         The read line goes quiet when NOTHING could be read: the Failed block
         below says the same thing with a Try again on it, and one outage
         described twice on one screen reads as two. -->
    <p class="mb-3 min-h-0 text-warn empty:mb-0" role="status">
      {{ admin.loaded.value ? admin.readError.value : '' }}
    </p>
    <p class="mb-3 min-h-0 text-warn empty:mb-0" role="alert">{{ admin.actionError.value }}</p>

    <div
      v-if="admin.loaded.value || !admin.readError.value"
      :id="`panel-${tab}`"
      role="tabpanel"
      :aria-labelledby="`tab-${tab}`"
      class="outline-none"
      tabindex="-1"
    >
      <AdminSatellites
        v-if="tab === 'satellites'"
        :pending="admin.enrollments.value"
        :satellites="admin.satellites.value"
        :collections="admin.collections.value"
        :broken="admin.broken.value"
        :act="admin.act"
      />

      <AdminLibraries
        v-else-if="tab === 'libraries'"
        :libraries="admin.libraries.value"
        :collections="admin.collections.value"
        :broken="admin.broken.value"
        :act="admin.act"
      />

      <AdminProviders
        v-else-if="tab === 'providers'"
        :act="admin.act"
        :refused="(why: string) => (admin.actionError.value = why)"
      />

      <AdminUsers
        v-else-if="tab === 'users'"
        :users="admin.users.value"
        :libraries="admin.libraries.value"
        :broken="admin.broken.value"
        :act="admin.act"
        :reread="() => admin.reload('users')"
        :refused="(why: string) => (admin.actionError.value = why)"
      />

      <AdminSessions
        v-else
        :sessions="admin.sessions.value"
        :broken="admin.broken.value"
        :act="admin.act"
      />
    </div>

    <!-- Nothing has ever been read AND something is failing: the panel is not
         empty, it is unavailable. -->
    <Failed
      v-if="!admin.loaded.value && admin.readError.value"
      what="Could not read the hub."
      :message="admin.readError.value"
      @retry="admin.reload()"
    />
  </main>
</template>
