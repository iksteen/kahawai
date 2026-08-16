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
import { awayFrom, boundaryKey, type RouteName } from './domain/routes.ts'
import { notify } from './composables/notices.ts'
import { signOut, whoAmI } from './api/session.ts'
import { useBoot } from './composables/boot.ts'
import { useLibraries } from './composables/home.ts'

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
const screen = computed(() =>
  boundaryKey(
    name.value,
    route.path,
    typeof route.params.library === 'string' ? route.params.library : undefined,
  ),
)

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
  </AppShell>
</template>
