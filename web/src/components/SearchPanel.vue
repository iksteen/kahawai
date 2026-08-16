<script setup lang="ts">
/// The search results, over whatever you were already looking at.
///
/// This used to be a page: typing on the home screen replaced it outright,
/// shelves and continue-watching included, so a search you did not mean cost
/// you your place and the home screen reloaded when you cleared it. The panel
/// leaves the screen alone — which is what the design had, and what makes
/// dismissing it free.
import Icon from './Icon.vue'
import { artworkUrl } from '../api/artwork.ts'
import { countLabel, SEARCH_LIST_ID, searchOptionId, type SearchRow } from '../domain/search-nav.ts'
import { metaLine, targetOf } from '../domain/label.ts'

const props = defineProps<{
  /// What the rows on screen are results FOR, which is not always what is in
  /// the box: they stay visible while their replacement loads, and the label
  /// has to stay honest while they do.
  query: string
  rows: SearchRow[]
  /// The libraries that could not be asked, by name. Only reporting the
  /// all-failed case still stated a fact it did not have: two libraries
  /// erroring beside one with no matches printed "nothing matches" over a
  /// count of one.
  failed: string[]
  allFailed: boolean
  searching: boolean
  highlight: number
}>()

const emit = defineEmits<{
  close: []
  retry: []
  openLibrary: [id: string]
  openItem: [target: string, library: string]
}>()
</script>

<template>
  <!-- Same shape as the menus: a click anywhere else lands here rather than on
       whatever it was over. z-14 like theirs, and the search box sits at 16 so
       clicking back into your own query still reaches the field. -->
  <div class="fixed inset-0 z-14" data-testid="search-sheet" @click="emit('close')" />
  <!-- The floor is not a flat 280px: between roughly 370 and 430px the header
       wraps only the profile button, leaving the box 212–267px wide, and a
       floor wider than that box wins against `left`/`right` — the panel grew
       54px off the right edge, taking every count and arrow with it. -->
  <div
    class="animate-rise-pop absolute top-[calc(100%+8px)] right-0 left-0 z-20 max-h-[70vh] min-w-[min(280px,100%)] overflow-y-auto rounded-lg border border-line bg-surface p-1.5 shadow-[0_12px_34px_rgba(0,0,0,0.55)]"
  >
    <!-- Out of flow and pinned: in the flow it pushed the whole list down 17px
         on every debounced keystroke and back up when it settled. -->
    <span
      v-if="props.searching"
      class="sticky top-0 z-1 float-right -mb-5 mt-[3px] mr-[5px] rounded-[3px] bg-surface px-[5px] py-[2px] font-mono text-[10px] text-teal"
      role="status"
    >
      updating
    </span>

    <!-- Outside the listbox below, because a listbox may contain nothing but
         options: a paragraph and a button inside one can be dropped from the
         accessibility tree altogether, which would silently hide the only two
         things in here worth reading out. -->
    <p
      v-if="props.rows.length === 0 && props.failed.length === 0"
      class="m-0 p-2.5 text-[13px] text-dim"
      role="status"
    >
      No matches for “{{ props.query }}”.
    </p>

    <p v-if="props.failed.length > 0" class="m-0 p-2.5 text-[13px] text-warn" role="status">
      {{
        props.allFailed
          ? 'Could not search — the hub did not answer.'
          : `Could not search ${props.failed.join(', ')}, so these results are incomplete.`
      }}
      <!-- The caller puts focus back in the box before this runs: pressing it
           clears the failure, which unmounts this button, and focus would land
           on the document body with the panel still open and its keys dead. -->
      <button class="cursor-pointer underline" type="button" @click="emit('retry')">
        Try again
      </button>
    </p>

    <!-- The list the input's `aria-controls` names, holding options and
         nothing else. Focus stays in the box and the lit row is named by
         `aria-activedescendant`, which is why the rows carry ids — and why
         they are not tab stops. -->
    <div :id="SEARCH_LIST_ID" role="listbox" :aria-label="`Results for ${props.query}`">
      <template v-for="(row, at) in props.rows" :key="rowKey(row)">
        <!-- Labelled, not just titled. An accessible name comes from the
             CONTENT when there is any, and this row's content is a library's
             name beside a bare number — so arrowing onto the first result
             announced "3d 1", which is neither the group it heads nor what
             pressing it does. `title` loses to content; a label wins. -->
        <button
          v-if="row.kind === 'library'"
          :id="searchOptionId(at)"
          class="row w-full gap-2 px-2.5 py-1.5"
          :class="at === props.highlight && 'row-on'"
          role="option"
          :aria-selected="at === props.highlight"
          tabindex="-1"
          :title="`Show everything in ${row.library.name}`"
          :aria-label="`Show everything in ${row.library.name}, ${countLabel(row.shown, row.total)}`"
          type="button"
          @click="emit('openLibrary', row.library.id)"
        >
          <!-- Small, dim and upper-case: a heading separates groups, and at
               body weight it reads as a bolder result title instead. -->
          <span class="flex-1 text-[11px] font-[650] tracking-[0.08em] text-dim uppercase">
            {{ row.library.name }}
          </span>
          <span class="font-mono text-[12px] text-dim">{{ countLabel(row.shown, row.total) }}</span>
          <span class="ml-auto flex text-teal"><Icon name="next" :size="13" /></span>
        </button>
        <button
          v-else
          :id="searchOptionId(at)"
          class="row w-full gap-2.5 px-2.5 py-1.5"
          :class="at === props.highlight && 'row-on'"
          role="option"
          :aria-selected="at === props.highlight"
          tabindex="-1"
          type="button"
          @click="emit('openItem', targetOf(row.item), row.library.id)"
        >
          <!-- Not `loading="lazy"`, which it inherited from the page it
               replaced: in here those images never loaded at all. A dropdown is
               at most fifteen thumbnails that are all on screen the instant it
               opens, so deferring them was never buying anything. -->
          <img class="thumb" :src="artworkUrl(row.item.id, row.item.art_version, 'thumb')" alt="" />
          <span class="flex min-w-0 flex-col">
            <span class="truncate text-[15px]">{{ row.item.title }}</span>
            <span class="truncate font-mono text-[12px] text-dim">{{ metaLine(row.item) }}</span>
          </span>
        </button>
      </template>
    </div>
  </div>
</template>

<script lang="ts">
/// The library id belongs in the key: membership is many-to-many, so one item
/// can appear under two libraries and its id alone is not unique in this list.
///
/// On the `<template v-for>` rather than on the branches inside it. Keyed on
/// the branch, the outer list is an unkeyed fragment and rows are matched by
/// position — so a library that starts matching inserts a heading and destroys
/// and rebuilds every thumbnail below it, which is the opposite of what keys
/// are for.
function rowKey(row: SearchRow): string {
  return row.kind === 'library' ? `lib:${row.library.id}` : `${row.library.id}:${row.item.id}`
}
export default { name: 'SearchPanel' }
</script>

<style scoped>
@reference '../theme.css';

.row {
  @apply flex cursor-pointer items-center rounded text-left text-text;
}
.row:hover {
  @apply bg-hover;
}
/* Stronger than the hover it sits beside, and with a marker down the left
   edge, because the two can be on different rows at once — the mouse rests
   where it last was while the arrows move somewhere else, and the row that
   Enter opens has to be the obvious one. */
.row-on {
  @apply bg-hover;
  box-shadow: inset 2px 0 0 var(--color-teal);
}
.thumb {
  @apply block h-[34px] w-[34px] shrink-0 rounded-[3px] object-cover;
  /* The same swell, at thumbnail size — a result row with no poster should not
     be a hole in the list. */
  background: var(--art-none);
}
</style>
