<script setup lang="ts">
/// The frame every screen sits in: the wordmark's jump menu, the search box,
/// the profile menu, and one notice host.
///
/// The bottom padding is room for the music queue bar, which floats over the
/// page rather than pushing it — without it the last row of a library sits
/// under the bar and cannot be reached.
import { computed, nextTick, ref, useTemplateRef, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'

import Icon, { type IconName } from './Icon.vue'
import MenuItem from './MenuItem.vue'
import MenuPopover from './MenuPopover.vue'
import SearchBox from './SearchBox.vue'
import SearchPanel from './SearchPanel.vue'
import { boundaryKey, hasSearchBox, hasSearchPanel, type RouteName } from '../domain/routes.ts'
import { targetOf } from '../domain/label.ts'
import { notice } from '../composables/notices.ts'
import { moveHighlight, SEARCH_LIST_ID, searchOptionId } from '../domain/search-nav.ts'
import { useSearch } from '../composables/search.ts'
import { useQueue } from '../composables/queue.ts'
import { useSearchPanel } from '../composables/search-panel.ts'

const props = defineProps<{
  libraries: { id: string; name: string; media_type: string }[]
  username: string
  admin: boolean
}>()
const emit = defineEmits<{ signOut: [] }>()

/// Whether the music dock is up, which is the only thing that covers the
/// bottom of the page.
const queue = useQueue()
const playing = computed(() => queue.queue.value.entries.length > 0)

const route = useRoute()
const router = useRouter()
// Destructured, because a template only auto-unwraps refs that are top-level
// setup bindings — `search.text` inside an object is not one, and reaching for
// `.value` in a template is the mistake that spelling invites.
const { text, query, open, typed, reopen, dismiss, clear, taken } = useSearch()

const name = computed(() => (route.name ?? 'libraries') as RouteName)

/// UI-17: where the focus goes when the screen changes.
///
/// A real navigation puts the focus at the top of the new document. This one
/// does not, so pressing a card left a screen reader user focused on a button
/// that no longer exists — the focus falls to `<body>`, and the next Tab starts
/// at the skip link with nothing said about where they now are.
///
/// Keyed on the SCREEN rather than the address, and for the same reason the
/// error boundary is: the player's autoplay handover changes the URL and must
/// not take the focus off whatever the viewer was reaching for.
///
/// Not on the FIRST render — a page that grabs the focus on load is a page that
/// has taken it from the browser's own starting point.
const content = useTemplateRef<HTMLElement>('content')
const screen = computed(() =>
  boundaryKey(
    name.value,
    route.path,
    typeof route.params.library === 'string' ? route.params.library : undefined,
  ),
)
watch(screen, () => void nextTick(() => content.value?.focus()))
const panel = computed(() => hasSearchPanel(name.value))
const hasBox = computed(() => hasSearchBox(name.value))
/// The field itself, so the panel's retry can put focus back in it.
const box = ref<{ focus: () => void } | null>(null)

/// The panel searches whatever the header holds, on the screens that have one.
/// It is asked for even while closed, so dismissing keeps the results it
/// already has — unmounting threw them away, and focusing the box again re-ran
/// every library's search and showed nothing for a round trip.
const libraryList = computed(() => props.libraries)
const panelQuery = computed(() => (panel.value ? query.value : ''))
const results = useSearchPanel(libraryList, panelQuery)

/// On screen: the panel has drawn something AND the box has not been
/// dismissed. Both, because they are different questions — one is "is there
/// anything to show", the other is "does the viewer want it".
const showing = computed(() => open.value && results.drawn.value)

/// Dismissing abandons the walk. Here rather than in the Escape handler
/// because there are four ways out — Escape, focus leaving the search area, a
/// click on the sheet, and opening something — and only this covers them all.
/// Without it the panel came back with the old row still lit: dismissed at the
/// eighth hit, refocused, and Enter opened that hit instead of the first
/// library, which is what "nothing highlighted" is supposed to mean.
watch(showing, (on) => {
  if (!on) results.highlight.value = -1
})

/// Focus first, then ask. Pressing Try again clears the failure, which
/// unmounts the button it was pressed with — focus would land on the document
/// body with the panel still open and every one of its keys dead, because they
/// are scoped to the search area.
function askAgain() {
  box.value?.focus()
  results.retry()
}

function walk(delta: number) {
  results.highlight.value = moveHighlight(results.rows.value.length, results.highlight.value, delta)
}

/// Enter with nothing highlighted falls to the first row, which `searchRows`
/// guarantees is a library heading — so Enter straight after typing shows
/// everything the first library matched, rather than guessing at one film.
function take() {
  const row = results.rows.value[results.highlight.value] ?? results.rows.value[0]
  if (!row) return
  if (row.kind === 'library') openFromPanel(row.library.id)
  else openItemFromPanel(targetOf(row.item), row.library.id)
}

/// A library keeps the text, where it becomes that library's filter. An item
/// does not: you asked for this one thing and got it.
function openFromPanel(library: string) {
  dismiss()
  void router.push({ name: 'library', params: { library } })
}
function openItemFromPanel(id: string, library: string) {
  taken()
  void router.push({ name: 'detail', params: { library, id } })
}

/// Any navigation puts the panel away.
///
/// Belt and braces: today every way out is explicit — Escape, the sheet, a
/// library row, a hit — and only the home screen has a panel at all, so no
/// test can tell this line from the four that already do the job. It stays
/// because the flag outliving the route is how the home screen once mounted
/// with a results panel already open over a page nobody had searched, and the
/// second screen to grow a panel will not come with that memory attached.
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
  <!-- The bottom reserve is CONDITIONAL and generous. Measured on an album at
       375×720, scrolled fully down: the last two tracks sat under the dock and
       their play and add buttons hit-tested to it — four controls with no way
       to reach them. Generous rather than exact, because the bar wraps to two
       rows at that width and grows another when it has an error to show; and
       conditional, because a page with nothing playing should not pay for a
       bar that is not there. -->
  <!-- UI-17: the first thing Tab reaches, and it goes past the header. The
       header carries a search box, a wordmark menu and a profile menu, and a
       keyboard user landing on a library page had to walk all of them before
       reaching the first card — on every navigation, because the focus returns
       to the top each time.

       Visible only when focused, which is the point: it is furniture for the
       people who need it and invisible to everyone else. -->
  <a
    class="sr-only rounded bg-surface px-3 py-2 text-teal focus:not-sr-only focus:absolute focus:top-2 focus:left-2 focus:z-40"
    href="#content"
  >
    Skip to the content
  </a>
  <div
    class="mx-auto max-w-[1200px] px-[clamp(14px,4vw,32px)]"
    :class="playing ? 'pb-[170px]' : 'pb-8'"
  >
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
        v-if="hasBox"
        ref="box"
        :model-value="text"
        :panel="panel"
        :shown="showing"
        :highlight="results.highlight.value"
        :count="results.rows.value.length"
        :list-id="SEARCH_LIST_ID"
        :option-id="searchOptionId"
        @update:model-value="typed($event, panel)"
        @reopen="reopen(panel)"
        @clear="clear()"
        @walk="walk"
        @take="take"
        @dismiss="dismiss()"
      >
        <SearchPanel
          v-if="showing"
          :query="results.shownQuery.value"
          :rows="results.rows.value"
          :failed="results.failed.value"
          :all-failed="results.allFailed.value"
          :searching="results.searching.value"
          :highlight="results.highlight.value"
          @close="dismiss()"
          @retry="askAgain"
          @open-library="openFromPanel"
          @open-item="openItemFromPanel"
        />
      </SearchBox>
      <div v-else class="flex-1" />

      <!-- `ml-auto` once the header has WRAPPED, so the profile button stays at
           the right edge of a line it is alone on: its menu is anchored to its
           right, and against a button at the left edge that computed a negative
           x on a phone and clipped its own labels to "…ttings".
           Only there, though. An auto margin absorbs free space before
           `justify-between` gets to divide it, so on a header that fits, this
           collected the whole surplus on one side and packed the search box
           against the wordmark instead of leaving it centred between the two.
           Below the wrap the search box has already shrunk to its floor, so
           there is no surplus for this to take. -->
      <div class="relative max-sm:ml-auto">
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

    <!-- The skip link's target, and the landmark a screen reader jumps to.
         `tabindex="-1"` because a fragment link moves the focus only to
         something that can hold it — without it the browser scrolls and leaves
         the focus where it was, so the next Tab goes back into the header. -->
    <div id="content" ref="content" tabindex="-1" class="outline-none">
      <slot :query="query" />
    </div>

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
        class="animate-rise-note rounded-md border border-line bg-surface px-4 py-2 shadow-lg"
      >
        {{ notice }}
      </div>
    </div>
  </div>
</template>
