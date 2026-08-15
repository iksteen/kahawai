<script setup lang="ts">
/// The frame, a boundary, and whatever screen the address names.
///
/// The shell's inputs — the library list, who is signed in, whether they are
/// an admin — arrive with the screens that own them: phase 5 brings the auth
/// gate and the bootstrap call, phase 6 the libraries. Until then they are
/// stated here as the placeholders they are, rather than faked somewhere that
/// would look like a real source.
import { computed } from 'vue'
import { useRoute, useRouter } from 'vue-router'

import AppShell from './components/AppShell.vue'
import Boundary from './components/Boundary.vue'
import { notify } from './composables/notices.ts'
import { signOut } from './api/session.ts'
import { awayFrom, boundaryKey, type RouteName } from './domain/routes.ts'

const route = useRoute()
const router = useRouter()

const libraries: { id: string; name: string; media_type: string }[] = []

/// Which SCREEN the boundary is guarding, which is not the same as the
/// address: an autoplay handover changes the URL and must not remount.
const name = computed(() => (route.name ?? 'libraries') as RouteName)

const screen = computed(() =>
  boundaryKey(
    name.value,
    route.path,
    typeof route.params.library === 'string' ? route.params.library : undefined,
  ),
)

/// Never rejects; it reports what could not be told to the hub. The tokens are
/// gone locally either way, which is the part that matters to whoever asked.
async function leave() {
  const trouble = await signOut()
  if (trouble) notify(trouble)
  await router.push({ name: 'libraries' })
}
</script>

<template>
  <AppShell :libraries="libraries" username="…" :admin="false" @sign-out="leave">
    <Boundary :reset-key="screen" :away="awayFrom(name)" @away="router.push({ name: 'libraries' })">
      <RouterView v-slot="{ Component }">
        <component :is="Component" />
      </RouterView>
    </Boundary>
  </AppShell>
</template>
