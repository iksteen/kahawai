/// One library's items, fetched a chunk at a time as the grid is scrolled.
///
/// Not TanStack Query: a chunk is not a page somebody asked for, it is a hole
/// in a reserved grid, and what this needs is a sparse map keyed by item index
/// with a generation stamp — not a cache keyed by request. The retry rule is
/// also the opposite of a query's: a chunk that failed must become askable
/// again, and the error line is about the SET of failures being non-empty
/// rather than about the last thing that happened.

import { onScopeDispose, type Ref, ref, watch } from 'vue'

import type { ItemRowI64 } from '../api/generated/model/itemRowI64.ts'
import { CHUNK } from '../domain/virtual.ts'
import { listItems } from '../api/generated/kahawai.ts'
import { sentence } from '../domain/refusal.ts'

export function useLibraryItems(library: Ref<string>, query: Ref<string>, sort: Ref<string>) {
  /// Sparse, keyed by item index. A `Map` rather than an array: the grid holds
  /// the whole library's height from the first answer, and most of it has
  /// never been fetched.
  const loaded = ref(new Map<number, ItemRowI64>())
  const total = ref<number | null>(null)
  /// What the library holds regardless of the filter, so the count line can
  /// say "12/2242" rather than leaving you wondering whether the other 2230
  /// are missing or excluded.
  const libraryTotal = ref<number | null>(null)
  const failure = ref('')

  /// Bumped whenever the result set changes identity. A reply carrying an
  /// older generation describes a library or a search we have left.
  let generation = 0
  /// The generation whose first reply REPLACES what is on screen rather than
  /// merging into it.
  let replacing = 0
  const asked = new Set<number>()
  /// Chunks that failed and have not since succeeded. The error line is about
  /// this set being non-empty.
  const failed = new Set<number>()

  /// Start over on a different result set — WITHOUT clearing what is
  /// displayed. Blanking here empties the page for the length of a round trip,
  /// and the old results are still true of the screen you are leaving until
  /// the new ones arrive.
  function reset() {
    generation += 1
    replacing = generation
    asked.clear()
    // The failures belonged to the result set being replaced. Left standing,
    // the line stays on screen over results that loaded perfectly — the only
    // thing that clears it is a chunk arriving while `failed` is empty, and
    // the chunk that failed is in a set nobody is asking for any more.
    failed.clear()
    failure.value = ''
  }

  async function load(chunk: number) {
    if (asked.has(chunk)) return
    asked.add(chunk)
    const mine = generation
    try {
      const answer = await listItems({
        library: library.value,
        q: query.value,
        sort: sort.value,
        limit: CHUNK,
        offset: chunk * CHUNK,
      })
      if (mine !== generation) return
      // Only the FIRST page replaces what is on screen. Keyed on the
      // generation alone, whichever reply landed first did it — so a chunk 3
      // that overtook chunk 0 cleared the map and re-seeded it with rows
      // 300–399, turning "the old results stay up until the new ones arrive"
      // into a screen of placeholders.
      const swap = replacing === mine && chunk === 0
      if (swap) replacing = 0
      total.value = answer.total
      if (!query.value) libraryTotal.value = answer.total
      const next = swap ? new Map<number, ItemRowI64>() : new Map(loaded.value)
      answer.items.forEach((item, at) => next.set(answer.offset + at, item))
      loaded.value = next
      // Cleared only when nothing is still missing. Clearing on ANY arrival
      // hides a real hole — one chunk failing beside one succeeding leaves a
      // hundred placeholder cards and silence — and never clearing leaves a
      // red line over a grid that has been complete for minutes.
      failed.delete(chunk)
      if (failed.size === 0) failure.value = ''
    } catch (cause) {
      if (mine !== generation) return
      // A failed chunk must be askable again.
      asked.delete(chunk)
      failed.add(chunk)
      failure.value = sentence(cause)
    }
  }

  /// Ask for whatever the visible rows need.
  ///
  /// No "do we already hold it" check. `asked` is the only thing that can
  /// answer that, and `loaded` cannot: after a reset it deliberately still
  /// holds the PREVIOUS result set, so a guard reading it skipped chunks the
  /// new one had never fetched — a re-sort re-fetched offset 0 and left
  /// ninety cells as permanent placeholders, because nothing re-asks.
  function need(chunks: number[]) {
    for (const chunk of chunks) void load(chunk)
  }

  /// Everything that failed, asked again — including the very first chunk,
  /// which nothing on screen would ask for, because with no `total` the grid
  /// has reserved nothing and has no visible rows to want.
  ///
  /// The old client needed a special case for that, because its retry nudged
  /// the visible-rows state and let an effect do the asking. This one asks
  /// `failed` directly, and the chunk that failed is in it either way — the
  /// special case was written anyway and no test could tell it apart.
  function retry() {
    failure.value = ''
    // No `asked.delete` here: the catch above already did it, and doing it
    // again would let a chunk that has since been re-asked and is still in
    // flight be requested twice.
    for (const chunk of failed) void load(chunk)
  }

  // A new result set: forget everything and start from the top, because a
  // scroll position in the old one means nothing in the new one. The query is
  // NOT cleared — arriving from a search on the home screen has to land with
  // that search still applied.
  watch(
    [library, query, sort],
    () => {
      reset()
      void load(0)
    },
    { immediate: true },
  )

  watch(library, () => (libraryTotal.value = null))

  onScopeDispose(() => {
    // Nothing in flight may paint after this.
    generation += 1
  })

  return { loaded, total, libraryTotal, failure, need, retry }
}
