<script setup lang="ts">
/// What you were part-way through, then what arrived lately in each library.
///
/// Not skipped while searching: the results are a panel over this screen, so
/// it stays where it is — which is what makes dismissing the panel free.
import { computed, watch } from 'vue'
import { useRouter } from 'vue-router'

import Art from '../components/Art.vue'
import Failed from '../components/Failed.vue'
import Shelf from '../components/Shelf.vue'
import { emptyHomeText, type Shelf as ShelfData, shown } from '../domain/shelves.ts'
import { metaLine, targetOf, watchedPct } from '../domain/label.ts'
import { notify } from '../composables/notices.ts'
import { sentence } from '../domain/refusal.ts'
import { useContinueWatching, useLibraries, useShelves } from '../composables/home.ts'
import { whoAmI } from '../api/session.ts'

const router = useRouter()

// Always, from this screen: it only exists inside the app.
const signedIn = computed(() => true)
const libraries = useLibraries(signedIn)
const continuing = useContinueWatching(signedIn)
const list = computed(() => libraries.data.value ?? [])
const { shelves, more, retry } = useShelves(list)

const rows = computed(() => shown(shelves.value))

function openLibrary(library: string) {
  void router.push({ name: 'library', params: { library } })
}
function openItem(id: string, library: string) {
  void router.push({ name: 'detail', params: { library, id } })
}

/// No row at all reads as "you have nothing on the go", which is the same lie
/// the shelves tell when they fail — louder, because the row simply is not
/// there to be doubted.
watch(
  () => continuing.isError.value,
  (failed) => failed && notify('Could not load what you were watching.'),
)

async function grow(shelf: ShelfData) {
  // A lane that stops growing is indistinguishable from one that has reached
  // the end of its library, so silence here is a lie by omission.
  if ((await more(shelf)) === 'failed') {
    notify(`Could not load more from ${shelf.library.name}.`)
  }
}
</script>

<template>
  <!-- The libraries themselves would not load: there is no home screen without
       them, and nowhere else to go from here — this IS home. -->
  <Failed
    v-if="libraries.isError.value"
    what="Could not load your libraries."
    :message="sentence(libraries.error.value)"
    @retry="libraries.refetch()"
  />

  <!-- Nothing at all until the list arrives. The shelves need it to know how
       many skeletons to draw, and a spinner for one round trip is worse than a
       blank moment. -->
  <main v-else-if="libraries.data.value">
    <p v-if="list.length === 0" class="text-dim">{{ emptyHomeText(whoAmI().admin) }}</p>

    <template v-else>
      <!-- No row at all when nothing is on the go — and a failure here is a
           notice rather than a missing row, because a missing row reads as
           "you have nothing on the go". -->
      <template v-if="continuing.data.value?.length">
        <h2 class="mb-2 text-[17px] font-[650]">Continue watching</h2>
        <div class="mb-7 grid grid-cols-[repeat(auto-fill,minmax(260px,1fr))] gap-3">
          <button
            v-for="item in continuing.data.value"
            :key="item.id"
            class="flex cursor-pointer gap-3 rounded border border-line bg-surface p-2 text-left hover:border-dim"
            type="button"
            @click="openItem(targetOf(item), item.library_id as string)"
          >
            <Art
              :item="item"
              size="card"
              class="w-[60px] shrink-0"
              :progress="false"
              :poster-of="item.kind === 'episode' ? item.parent_id : null"
            />
            <span class="flex min-w-0 flex-1 flex-col justify-center gap-1">
              <span class="truncate">{{ item.title }}</span>
              <span class="truncate font-mono text-[11px] text-dim">{{ metaLine(item) }}</span>
              <span class="block h-[3px] rounded bg-hairline">
                <span
                  class="block h-full rounded bg-teal"
                  :style="{ width: `${watchedPct(item) ?? 0}%` }"
                />
              </span>
            </span>
          </button>
        </div>
      </template>

      <Shelf
        v-for="shelf in rows"
        :key="shelf.library.id"
        :shelf="shelf"
        @open="openLibrary"
        @open-item="openItem"
        @near-end="grow(shelf)"
        @retry="(done) => retry(shelf).then(done)"
      />
    </template>
  </main>
</template>
