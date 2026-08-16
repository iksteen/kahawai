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
  <div
    class="absolute top-full right-0 left-0 z-20 mt-1 max-h-[70vh] overflow-y-auto rounded-md border border-line bg-surface py-1 shadow-lg"
  >
    <span v-if="props.searching" class="px-3 font-mono text-[11px] text-dimmer" role="status">
      updating
    </span>

    <!-- Outside the listbox below, because a listbox may contain nothing but
         options: a paragraph and a button inside one can be dropped from the
         accessibility tree altogether, which would silently hide the only two
         things in here worth reading out. -->
    <p
      v-if="props.rows.length === 0 && props.failed.length === 0"
      class="px-3 py-2 text-dim"
      role="status"
    >
      No matches for “{{ props.query }}”.
    </p>

    <p v-if="props.failed.length > 0" class="px-3 py-2 text-warn" role="status">
      {{
        props.allFailed
          ? 'Could not search — the hub did not answer.'
          : `Could not search ${props.failed.join(', ')}, so these results are incomplete.`
      }}
      <!-- Focus goes back to the box before this is pressed: clearing the
           failure unmounts the button, and focus would land on the document
           body with the panel still open and its keys dead. -->
      <button class="cursor-pointer underline" type="button" @click="emit('retry')">
        Try again
      </button>
    </p>

    <!-- The list the input's `aria-controls` names, holding options and
         nothing else. Focus stays in the box and the lit row is named by
         `aria-activedescendant`, which is why the rows carry ids — and why
         they are not tab stops. -->
    <div :id="SEARCH_LIST_ID" role="listbox" :aria-label="`Results for ${props.query}`">
      <template v-for="(row, at) in props.rows">
        <button
          v-if="row.kind === 'library'"
          :key="`lib:${row.library.id}`"
          :id="searchOptionId(at)"
          class="flex w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left"
          :class="at === props.highlight ? 'bg-hover text-teal' : 'hover:bg-hover'"
          role="option"
          :aria-selected="at === props.highlight"
          tabindex="-1"
          :title="`Show everything in ${row.library.name}`"
          type="button"
          @click="emit('openLibrary', row.library.id)"
        >
          <span class="flex-1 font-[650]">{{ row.library.name }}</span>
          <span class="font-mono text-[11px] text-dim">{{ countLabel(row.shown, row.total) }}</span>
          <Icon name="next" :size="13" />
        </button>
        <!-- The library id belongs in the key: membership is many-to-many, so
             one item can appear under two libraries and its id alone is not
             unique in this list. -->
        <button
          v-else
          :key="`${row.library.id}:${row.item.id}`"
          :id="searchOptionId(at)"
          class="flex w-full cursor-pointer items-center gap-2 px-3 py-1 text-left"
          :class="at === props.highlight ? 'bg-hover' : 'hover:bg-hover'"
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
          <img
            class="h-[34px] w-[34px] shrink-0 rounded object-cover"
            :src="artworkUrl(row.item.id, row.item.art_version, 'thumb')"
            alt=""
          />
          <span class="flex min-w-0 flex-col">
            <span class="truncate text-[13px]">{{ row.item.title }}</span>
            <span class="truncate font-mono text-[11px] text-dim">{{ metaLine(row.item) }}</span>
          </span>
        </button>
      </template>
    </div>
  </div>
</template>
