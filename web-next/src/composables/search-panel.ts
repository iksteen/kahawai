/// Searching every library at once.
///
/// One request per library rather than one for everything, because the items
/// endpoint does not say which library a row came from, and "at most five
/// each" is not something a single LIMIT can express. They run concurrently
/// and each is a bounded query, so it costs one round trip.
///
/// The hub could group them in SQL — it was measured, and it is slower than
/// this fan-out at every catalogue size tried (`docs/web-next-plan.md`, B3).
///
/// ONE query holding all of them, rather than one query per library. The
/// difference is what happens on the next keystroke: `useQueries` rebuilds its
/// observers when the keys change, so a per-library query has no previous data
/// to keep and the panel blanks between every two letters. A single observer
/// keyed on the query text keeps the rows that are on screen until their
/// replacements arrive, which is the behaviour the old panel spent forty lines
/// on.

import { computed, type Ref, ref, watch } from 'vue'
import { keepPreviousData, useQuery } from '@tanstack/vue-query'

import type { LibrarySummary } from '../domain/shelves.ts'
import { listItems } from '../api/generated/kahawai.ts'
import { type LibraryHits, searchRows, searchTrouble } from '../domain/search-nav.ts'
import { notify } from './notices.ts'
import { sentence } from '../domain/refusal.ts'

/// How many hits each library contributes to the panel.
export const PER_LIBRARY = 5

export function useSearchPanel(libraries: Ref<LibrarySummary[]>, query: Ref<string>) {
  /// Nothing is asked while there is nothing to ask for, and nothing while the
  /// library list is still coming.
  ///
  /// The guard is on the LENGTH, not on the value. An empty list searched
  /// nothing and answered immediately, and the panel stated "No matches" as a
  /// fact about the catalogue — from a search that never ran, with no Try
  /// again, because nothing failed from its point of view.
  const asking = computed(() => query.value !== '' && libraries.value.length > 0)

  const search = useQuery({
    queryKey: computed(() => ['search', query.value, libraries.value.map((l) => l.id).join(',')]),
    enabled: asking,
    // The rows already on screen stay visible and actionable while their
    // replacements load. Blanking the panel makes every debounced keystroke
    // flash the whole surface away instead.
    placeholderData: keepPreviousData,
    queryFn: async (): Promise<LibraryHits[]> =>
      Promise.all(
        libraries.value.map(async (library) => {
          try {
            const answer = await listItems({
              library: library.id,
              q: query.value,
              limit: PER_LIBRARY,
            })
            return { library, items: answer.items, total: answer.total, failure: '' }
          } catch (cause) {
            // "No matches here" and "we could not ask" are different answers,
            // and an empty list gives the first when it means the second. The
            // query itself does not fail: one library being away must not take
            // the other three's results off the screen.
            return { library, items: [], total: 0, failure: sentence(cause) }
          }
        }),
      ),
  })

  /// The query the rows on screen belong to, and '' when there are none.
  ///
  /// `keepPreviousData` hands the last result set to the NEXT key, which is
  /// what keeps rows actionable between two keystrokes — and what made an
  /// emptied box remember. Cleared the moment nothing is being asked, so
  /// typing again starts from nothing rather than from the search somebody
  /// cancelled: the panel was labelled "Results for zzz" over the hits for
  /// "heat", and two arrow presses and Enter opened a film out of them.
  const shownQuery = ref('')
  watch(
    [asking, () => search.isPlaceholderData.value, () => search.data.value],
    () => {
      if (!asking.value) shownQuery.value = ''
      else if (!search.isPlaceholderData.value && search.data.value) shownQuery.value = query.value
    },
    { immediate: true },
  )

  const hits = computed<LibraryHits[]>(() =>
    shownQuery.value === '' ? [] : (search.data.value ?? []),
  )
  const rows = computed(() => searchRows(hits.value))

  /// Asking, with the previous query's rows still on screen.
  ///
  /// No `asking &&` guard: a query that has been disabled mid-flight stops
  /// reporting itself as fetching, so the guard was a second answer to a
  /// question that already had one, and no test could tell it apart.
  const searching = computed(() => search.isFetching.value)

  /// Cleared before asking, so a failure does not paint over the next
  /// keystroke's results for a round trip — the banner named a library the
  /// current search had not asked yet.
  const failed = computed(() =>
    searching.value ? [] : hits.value.filter((h) => h.failure !== '').map((h) => h.library.name),
  )
  const allFailed = computed(
    () => failed.value.length > 0 && failed.value.length === hits.value.length,
  )

  /// Whether there is a panel ON SCREEN — which is what the input's
  /// `aria-expanded` has to mean.
  ///
  /// Not `rows.length > 0`: a query that matches nothing, or a hub that failed
  /// every library, puts a visible panel up with no rows in it. Deriving this
  /// from the row count told a screen reader the combobox was collapsed while
  /// "No matches" or the retry button was showing — the exact two states
  /// somebody would most need read out.
  const drawn = computed(() => asking.value && shownQuery.value !== '')

  /// The lit row, `-1` for none. Not carried across a query: a position kept
  /// in a list that has been replaced points at a different film, and Enter
  /// would open it.
  const highlight = ref(-1)
  watch(rows, () => (highlight.value = -1))

  /// One notice per SETTLED ATTEMPT — the moment a search stops being in
  /// flight, and every time. Reported here rather than per library because
  /// notices are latest-wins: one each would name whichever failed last and
  /// imply the rest were fine.
  ///
  /// An earlier version remembered the sentence it had said and refused to
  /// repeat it. Notices clear after five seconds, so there was nothing on
  /// screen to duplicate: pressing Try again against a hub that was still down
  /// then said nothing at all — on the one channel the panel deliberately does
  /// not leave to a live region.
  watch(searching, (now, before) => {
    if (now || !before || !asking.value) return
    const trouble = searchTrouble(search.data.value ?? [])
    if (trouble !== '') notify(trouble)
  })

  return {
    rows,
    failed,
    allFailed,
    searching,
    drawn,
    highlight,
    /// What the rows on screen are results FOR, which is not always what is in
    /// the box: the label has to stay honest while a replacement is in flight.
    shownQuery,
    retry: () => void search.refetch(),
  }
}
