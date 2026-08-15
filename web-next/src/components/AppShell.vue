<script setup lang="ts">
/// The frame every screen sits in: the wordmark's jump menu, the search box,
/// the profile menu, and one notice host.
///
/// The bottom padding is room for the music queue bar, which floats over the
/// page rather than pushing it — without it the last row of a library sits
/// under the bar and cannot be reached.
import { computed, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'

import Icon, { type IconName } from './Icon.vue'
import MenuItem from './MenuItem.vue'
import MenuPopover from './MenuPopover.vue'
import SearchBox from './SearchBox.vue'
import { hasSearchBox, hasSearchPanel, type RouteName } from '../domain/routes.ts'
import { notice } from '../composables/notices.ts'
import { useSearch } from '../composables/search.ts'

const props = defineProps<{
  libraries: { id: string; name: string; media_type: string }[]
  username: string
  admin: boolean
}>()
const emit = defineEmits<{ signOut: [] }>()

const route = useRoute()
const router = useRouter()
// Destructured, because a template only auto-unwraps refs that are top-level
// setup bindings — `search.text` inside an object is not one, and reaching for
// `.value` in a template is the mistake that spelling invites.
const { text, query, typed, reopen, dismiss, clear } = useSearch()

const name = computed(() => (route.name ?? 'libraries') as RouteName)
const panel = computed(() => hasSearchPanel(name.value))
const box = computed(() => hasSearchBox(name.value))

/// Any navigation puts the panel away — not only the two that go through
/// `go()`. Back and forward, a result opening its item, a view pushing a route
/// of its own: none of them pass through this component, and the flag
/// outliving the route is how the home screen once mounted with a results
/// panel already open over a page nobody had searched.
watch(
  () => route.fullPath,
  () => dismiss(),
)

const navOpen = ref(false)
const profileOpen = ref(false)

/// Music, film, everything else. Two libraries of the same kind can be told
/// apart by name; a film library and a music one differ in what a row of
/// artwork even shows, so the kind is worth a glyph.
function libGlyph(mediaType: string): IconName {
  if (mediaType === 'music') return 'album'
  if (mediaType === 'movies') return 'movie'
  return 'show'
}

/// One at a time: opening either closes the other, or two popovers sit over
/// each other with two sheets and the first click goes to the wrong one.
function openNav() {
  profileOpen.value = false
  navOpen.value = !navOpen.value
}
function openProfile() {
  navOpen.value = false
  profileOpen.value = !profileOpen.value
}

/// Only the destinations that SHOW what was filtered clear it. Going to
/// Settings and back to a library keeps the filter you were using — the old
/// header cleared on Home and on a library and nowhere else, and a detour is
/// not the same as a fresh start.
function go(to: Parameters<typeof router.push>[0], fresh: boolean) {
  navOpen.value = false
  profileOpen.value = false
  if (fresh) clear()
  void router.push(to)
}
</script>

<template>
  <div class="mx-auto max-w-[1200px] px-[clamp(14px,4vw,32px)] pb-22">
    <!-- Wraps rather than crushes: on a narrow screen the search box drops to
         its own line at full width instead of shrinking to something no title
         fits in. -->
    <header class="flex flex-wrap items-center justify-between gap-3 pt-5 pb-6.5">
      <div class="relative">
        <button
          class="flex cursor-pointer items-center text-xl font-[650] tracking-[0.04em]"
          title="Jump to…"
          type="button"
          :aria-expanded="navOpen"
          aria-haspopup="menu"
          @click="openNav"
        >
          <span>kahawai<span class="text-teal">~</span></span>
          <!-- Hidden: `aria-expanded` already says which way it points, and a
               glyph inside the button becomes part of its name — so the name
               would change on every toggle and be read out again. -->
          <span class="ml-[7px] flex text-dim" aria-hidden="true">
            <Icon :name="navOpen ? 'chevronUp' : 'chevronDown'" />
          </span>
        </button>
        <MenuPopover :open="navOpen" align="left" @close="navOpen = false">
          <MenuItem
            glyph="home"
            :here="name === 'libraries'"
            @click="go({ name: 'libraries' }, true)"
          >
            Home
          </MenuItem>
          <span
            v-if="props.libraries.length"
            class="my-1 block h-px bg-hairline"
            role="separator"
          />
          <MenuItem
            v-for="library in props.libraries"
            :key="library.id"
            :glyph="libGlyph(library.media_type)"
            :here="name === 'library' && route.params.library === library.id"
            @click="go({ name: 'library', params: { library: library.id } }, true)"
          >
            {{ library.name }}
          </MenuItem>
        </MenuPopover>
      </div>

      <SearchBox
        v-if="box"
        :model-value="text"
        :panel="panel"
        :shown="false"
        :highlight="-1"
        list-id="search-results"
        :option-id="(i: number) => `search-option-${i}`"
        @update:model-value="typed($event, panel)"
        @reopen="reopen(panel)"
        @clear="clear()"
      />
      <div v-else class="flex-1" />

      <!-- `ml-auto` once the header has wrapped, so the profile button stays
           at the right edge. Without it the menu anchored to it computed a
           negative x on a phone and clipped its own labels to "…ttings". -->
      <div class="relative ml-auto">
        <button
          class="flex cursor-pointer items-center gap-2 rounded-full border border-line py-[3px] pr-2.5 pl-1 hover:border-dim"
          :title="props.username"
          type="button"
          :aria-expanded="profileOpen"
          aria-haspopup="menu"
          @click="openProfile"
        >
          <span
            class="grid size-6 shrink-0 place-items-center rounded-full border border-teal-dim bg-hover text-teal"
          >
            <Icon name="user" />
          </span>
          <span class="text-[13px] text-prose">{{ props.username }}</span>
          <span class="flex text-dim" aria-hidden="true">
            <Icon :name="profileOpen ? 'chevronUp' : 'chevronDown'" />
          </span>
        </button>
        <MenuPopover :open="profileOpen" align="right" @close="profileOpen = false">
          <MenuItem
            glyph="gear"
            :here="name === 'settings'"
            @click="go({ name: 'settings' }, false)"
          >
            Settings
          </MenuItem>
          <!-- Gated here as well as on the hub, which refuses every admin route
               regardless. This is not the security boundary — it is that
               rendering a page of refusals to somebody who cannot use it
               invites the reading that something is broken. -->
          <MenuItem
            v-if="props.admin"
            glyph="shield"
            :here="name === 'admin'"
            @click="go({ name: 'admin' }, false)"
          >
            Admin
          </MenuItem>
          <span class="my-1 block h-px bg-hairline" role="separator" />
          <MenuItem glyph="signOut" leaving @click="emit('signOut')">Sign out</MenuItem>
        </MenuPopover>
      </div>
    </header>

    <slot :query="query" />

    <!-- Outside every view, so a notice survives the view that raised it
         navigating away.
         Always in the document, even empty: a live region that is inserted
         together with its text is commonly announced by nothing at all, and
         this is the one place the app has to say something went wrong.
         Bottom right and not clickable — it reports, it does not ask, and the
         music dock is bottom centre. -->
    <div
      class="pointer-events-none fixed right-5 bottom-5 z-30 max-w-[420px]"
      role="status"
      aria-live="polite"
    >
      <div
        v-if="notice !== ''"
        class="rounded-md border border-line bg-surface px-4 py-2 shadow-lg"
      >
        {{ notice }}
      </div>
    </div>
  </div>
</template>
