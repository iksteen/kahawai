/// Searching every library at once.
///
/// ONE request: the hub's `/search` answers every library this viewer may see,
/// each with the kinds it holds (for music, album artists and songs beside the
/// albums), asking them concurrently on its side. A request per library — or
/// per kind — cost each a round trip of its own, which on a slow link is the
/// waiting. The hub does not group them in one query either: that was measured
/// and is slower at every catalogue size tried (`docs/web-next-plan.md`, B3).
///
/// All or nothing. The libraries share one catalogue, so they fail together
/// anyway, and results with a library quietly missing read as fewer matches.
///
/// ONE query holding all of them. The difference is what happens on the next
/// keystroke: a query whose key changes keeps the rows that are on screen
/// until their replacements arrive (`keepPreviousData`), which is the
/// behaviour the old panel spent forty lines on.

import { computed, type Ref, ref, watch } from 'vue'
import { keepPreviousData, useQuery } from '@tanstack/vue-query'

import { searchEverywhere } from '../api/catalogue.ts'
import { type LibraryHits, searchRows } from '../domain/search-nav.ts'
import { notify } from './notices.ts'
import { sentence } from '../domain/refusal.ts'

/// How many hits of each kind each library contributes to the panel.
export const PER_LIBRARY = 5

export function useSearchPanel(query: Ref<string>) {
  /// Nothing is asked while there is nothing to ask for.
  const asking = computed(() => query.value !== '')

  const search = useQuery({
    queryKey: computed(() => ['search', query.value]),
    enabled: asking,
    // The rows already on screen stay visible and actionable while their
    // replacements load. Blanking the panel makes every debounced keystroke
    // flash the whole surface away instead.
    placeholderData: keepPreviousData,
    queryFn: (): Promise<LibraryHits[]> => searchEverywhere({ q: query.value, limit: PER_LIBRARY }),
  })

  /// Asking, with the previous query's rows still on screen.
  ///
  /// No `asking &&` guard: a query that has been disabled mid-flight stops
  /// reporting itself as fetching, so the guard was a second answer to a
  /// question that already had one, and no test could tell it apart.
  const searching = computed(() => search.isFetching.value)

  /// Why the search could not be answered, '' while it can or while it is
  /// being asked again — so a failure does not paint over the next keystroke's
  /// results for a round trip.
  const failure = computed(() =>
    !searching.value && asking.value && search.isError.value ? sentence(search.error.value) : '',
  )

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
    [asking, () => search.isPlaceholderData.value, () => search.data.value, failure],
    () => {
      if (!asking.value) shownQuery.value = ''
      else if (failure.value !== '') shownQuery.value = query.value
      else if (!search.isPlaceholderData.value && search.data.value) shownQuery.value = query.value
    },
    { immediate: true },
  )

  /// Nothing under a failure: rows left over from the last answer would read
  /// as this query's.
  const hits = computed<LibraryHits[]>(() =>
    shownQuery.value === '' || failure.value !== '' ? [] : (search.data.value ?? []),
  )
  const rows = computed(() => searchRows(hits.value))

  /// Whether there is a panel ON SCREEN — which is what the input's
  /// `aria-expanded` has to mean.
  ///
  /// Not `rows.length > 0`: a query that matches nothing, or a search that
  /// failed, puts a visible panel up with no rows in it. Deriving this
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
  /// flight, and every time.
  ///
  /// An earlier version remembered the sentence it had said and refused to
  /// repeat it. Notices clear after five seconds, so there was nothing on
  /// screen to duplicate: pressing Try again against a hub that was still down
  /// then said nothing at all — on the one channel the panel deliberately does
  /// not leave to a live region.
  watch(searching, (now, before) => {
    if (now || !before || failure.value === '') return
    notify(`Could not search — ${failure.value}`)
  })

  return {
    rows,
    failure,
    searching,
    drawn,
    highlight,
    /// What the rows on screen are results FOR, which is not always what is in
    /// the box: the label has to stay honest while a replacement is in flight.
    shownQuery,
    retry: () => void search.refetch(),
  }
}
