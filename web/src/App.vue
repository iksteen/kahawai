<script setup lang="ts">
/// Which of the four things there is to show: nothing yet, a hub that did not
/// start, a way in, or the app.
///
/// A session that ENDS on its own leaves the route alone, so signing back in
/// returns you to the page you were reading. A deliberate sign-out does not —
/// see `leave`.
import { computed, nextTick, onMounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'

import AppShell from './components/AppShell.vue'
import Auth from './views/Auth.vue'
import Boundary from './components/Boundary.vue'
import Failed from './components/Failed.vue'
import QueueBar from './components/QueueBar.vue'
import { addressOf, awayFrom, type RouteName } from './domain/routes.ts'
import { notify } from './composables/notices.ts'
import { signOut, whoAmI } from './api/session.ts'
import { useBoot } from './composables/boot.ts'
import { useDocumentTitle, screenName } from './composables/title.ts'
import type { Screen } from './domain/titles.ts'
import { useLibraries } from './composables/home.ts'
import { useQueue } from './composables/queue.ts'

const route = useRoute()
const router = useRouter()

const { phase, bootError, setupAvailable, setupUrl, note, start } = useBoot()
onMounted(() => void start())

/// The header's jump menu lists what the home screen already asked for, so
/// this reads the same query rather than making a second request. Empty until
/// it lands, which is a menu with Home in it.
///
/// Not before there is a session: this component exists from the first frame,
/// and asking on the boot or sign-in screens is a guaranteed 401 — two of them
/// on first-run setup, where no refresh cookie can exist to recover with.
const libraries = useLibraries(computed(() => phase.value === 'app'))

/// Who the token says you are — for the name in the header and whether to
/// offer the Admin menu, never as an authorisation decision. See `claims.ts`.
const me = computed(() => (phase.value === 'app' ? whoAmI() : { username: '', admin: false }))

const name = computed(() => (route.name ?? 'libraries') as RouteName)

/// Which SCREEN the boundary is guarding, which is not the same as the
/// address: an autoplay handover changes the URL and must not remount.
const screen = computed(() => addressOf(route))

/// UI-17: the tab strip, the bookmark, and the one thing that tells a screen
/// reader the screen changed at all. The route knows which screen; what is ON
/// it arrives a round trip later, so the title is set twice and announced once.
///
/// The GATE outranks the route, and a boot error outranks the gate — the same
/// order the template renders in. Until the session is up there is no router
/// screen on display, and the route still names whichever page the last session
/// ended on: a sign-in form under the title "Home" is the tab strip and the
/// screen reader both saying something untrue, and so is "Starting" over a hub
/// that has already given up.
const shown = computed<Screen>(() => {
  if (bootError.value) return 'failed'
  return phase.value === 'app' ? name.value : phase.value
})
/// Announced on ARRIVAL, and the boundary's key is what an arrival is: the
/// route name alone says an item and the next item are the same screen, so
/// pressing an episode said nothing at all. The same key, for the same reason
/// the boundary uses it — the player's autoplay handover changes the URL and
/// is not somewhere the viewer went.
useDocumentTitle(shown, screen)

/// The queue lives above the router, because it survives navigation — that is
/// the whole point of a queue. It is inside the SHELL, though: signing out has
/// to unmount it while the bearer still works, so its sessions are ended rather
/// than left for the reaper.
const queue = useQueue()

/// Signed in. The note goes with the screen that explained it.
function entered() {
  note.value = ''
  phase.value = 'app'
}

/// Sign out in two steps, and the ORDER is the whole subject: two requests
/// that only work while the credentials still exist, sent from a path whose
/// job is to destroy them.
///
/// Clearing the tokens first meant the player unmounted afterwards, so its
/// final progress report went out unauthenticated, 401'd, found no refresh
/// token and never landed — a leaked transcoder slot every time. Navigating
/// first unmounts the player while the bearer still works; `signOut` then
/// clears it synchronously and revokes under the shared auth lock.
///
/// `replace`, and to the home screen. A session that EXPIRES keeps its route,
/// because signing back in as yourself should return you to the page you were
/// reading — but a deliberate sign-out is a different act, and it has to reach
/// the URL. The address bar is what a reload reads, so leaving it on
/// /library/x/item/y restores the previous account's page.
///
/// `signOut` never rejects; it reports what could not be told to the hub. The
/// tokens are gone locally either way, which is the part that matters to
/// whoever asked.
async function leave() {
  // The PHASE, not the route. Navigating unmounts the route's component;
  // leaving the app unmounts everything the shell owns as well — the music
  // queue is deliberately outside the router, because it survives navigation,
  // and its teardown is one of the requests that only works while the bearer
  // does.
  phase.value = 'login'
  // Explicit, though no test can currently tell it apart: the awaits below
  // happen to flush first, including a `replace` to the route you are already
  // on. That is the router's timing, not a promise it makes, and what has to
  // hold is stated by the two tests that assert the shell is gone by the time
  // the credentials are — either of which fails if this ordering breaks,
  // whichever mechanism provided the flush.
  await nextTick()
  await router.replace({ name: 'libraries' })
  const trouble = await signOut()
  if (trouble) notify(trouble)
}
</script>

<template>
  <!-- The hub did not start. Not the sign-in screen: there is nothing to sign
       in to while it is unreachable, and a signed-in viewer has perfectly good
       tokens. -->
  <Failed v-if="bootError" what="Could not start." :message="bootError" @retry="start" />

  <!-- Nothing, deliberately. A spinner that flashes for 40 ms on every load is
       worse than a blank moment; a boot slow enough to notice ends above. -->
  <template v-else-if="phase === 'boot'" />

  <Auth
    v-else-if="phase === 'setup' || phase === 'login'"
    :mode="phase"
    :note="note"
    :setup-available="setupAvailable"
    :setup-url="setupUrl"
    @done="entered"
  />

  <AppShell
    v-else
    :libraries="libraries.data.value ?? []"
    :username="me.username"
    :admin="me.admin"
    @sign-out="leave"
  >
    <Boundary :reset-key="screen" :away="awayFrom(name)" @away="router.push({ name: 'libraries' })">
      <RouterView v-slot="{ Component }">
        <component :is="Component" />
      </RouterView>
    </Boundary>

    <!-- Its OWN boundary, not the route's. The queue survives every navigation,
         so it has the longest life and the most state to get wrong — and
         rendering it outside a boundary meant a throw in it was a white page
         with no header and no way back. `away` drops the queue rather than
         navigating: the thing that threw is the thing to put down.

         Keyed on the queue's GENERATION, so a caught throw clears when a
         different record is put on — and not on the entries, because appending
         to the one playing must not remount it mid-track. -->
    <Boundary
      v-if="queue.queue.value.entries.length > 0"
      :reset-key="`queue:${queue.generation.value}`"
      away="Put it down"
      @away="queue.clear()"
    >
      <!-- One pair of ears: the video player takes the sound while it is on
           screen, and the queue resumes when you leave it. -->
      <QueueBar :paused="name === 'player'" />
    </Boundary>
  </AppShell>

  <!-- Outside the chain above, because a `v-else` only pairs with the element
       immediately before it — and in the document for the whole of the app's
       life, because a live region inserted together with its text is commonly
       announced by nothing. Not while booting or signing in: there is no screen
       to have moved between, and those two say what they are themselves. -->
  <p v-if="phase === 'app'" class="sr-only" role="status" aria-live="polite">
    {{ screenName }}
  </p>
</template>
