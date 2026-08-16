/// The home screen's data: your libraries, what you are part-way through, and
/// a shelf of what arrived lately in each.
///
/// Every shelf is its own query, which is the whole shape of this screen. One
/// request for all of them shows nothing until the slowest library answers;
/// one per library means every shelf is on screen as a skeleton immediately
/// and fills itself in, and a library that fails is one row saying so rather
/// than a home screen that is missing it.

import { computed, type Ref, ref } from 'vue'
import { useQueries, useQuery, useQueryClient } from '@tanstack/vue-query'

import type { ItemRowI64 } from '../api/generated/model/itemRowI64.ts'
import { listItems, listLibraries } from '../api/generated/kahawai.ts'
import {
  appendPage,
  hasMore,
  type LibrarySummary,
  reachable,
  type Shelf,
} from '../domain/shelves.ts'

/// A shelf's first page, and how much it fetches each time it is scrolled near
/// its end. Small enough that opening the home screen is seven cheap queries,
/// big enough that a shelf does not grow a card at a time.
export const PER_SHELF = 20

/// How many things you can be in the middle of before the row stops helping.
/// Past this it is a list of abandoned evenings, and it pushes the shelves off
/// the screen.
export const CONTINUING = 12

/// The libraries you can see. `enabled` because the shell asks for these as
/// soon as it exists, and before the session is restored that is a guaranteed
/// 401 — two of them on the first-run setup screen, where no cookie can exist.
export function useLibraries(enabled: Ref<boolean>) {
  return useQuery({
    queryKey: ['libraries'],
    queryFn: () => listLibraries(),
    select: (r) => r.libraries,
    enabled,
  })
}

/// Cross-library and in one request, because recency only means anything
/// across the whole set: per-library calls would each be ordered correctly and
/// could not be merged, since the timestamp they were ordered by is not in the
/// response.
export function useContinueWatching(enabled: Ref<boolean>) {
  return useQuery({
    queryKey: ['continuing'],
    queryFn: () => listItems({ in_progress: true, limit: CONTINUING }),
    select: (r) => reachable(r.items),
    enabled,
  })
}

/// One query per library, keyed by library id, so a retry touches one shelf
/// and the others keep the pages the viewer has already scrolled into.
export function useShelves(libraries: Ref<LibrarySummary[]>) {
  const client = useQueryClient()

  /// What each shelf has fetched BEYOND its first page, and how many rows the
  /// hub handed over for them. Held outside the query cache because it is the
  /// viewer's scrolling rather than the server's answer: a refetch of page one
  /// must not throw it away, and a query that re-runs must not double it.
  const extra = ref<Record<string, { rows: ItemRowI64[]; served: number }>>({})

  /// Bumped when a shelf is asked again. A page already in flight belongs to
  /// the list that failed, and appending it to the fresh one splices somebody
  /// else's scroll position into the new answer — measured: the heading read
  /// "6 of 1" and the shelf never asked for anything again.
  const generation: Record<string, number> = {}

  const queries = useQueries({
    queries: computed(() =>
      libraries.value.map((library) => ({
        queryKey: ['shelf', library.id],
        queryFn: () => listItems({ library: library.id, sort: '-added', limit: PER_SHELF }),
      })),
    ),
  })

  const shelves = computed<Shelf[]>(() =>
    libraries.value.map((library, at) => {
      const q = queries.value[at]
      const first = q?.data?.items ?? []
      const more = extra.value[library.id]
      return {
        library,
        items: appendPage(first, more?.rows ?? []),
        total: q?.data?.total ?? 0,
        served: first.length + (more?.served ?? 0),
        state: q?.isError ? 'failed' : q?.isPending !== false ? 'pending' : 'ready',
      }
    }),
  )

  /// Ask for the next page of one shelf. Safe to call repeatedly — the lane
  /// asks once per width, and this refuses while one is out or the library has
  /// been read to the end.
  const busy = new Set<string>()
  async function more(shelf: Shelf): Promise<'ok' | 'end' | 'failed'> {
    const id = shelf.library.id
    if (busy.has(id) || !hasMore(shelf)) return 'end'
    busy.add(id)
    const mine = generation[id] ?? 0
    try {
      const page = await listItems({
        library: id,
        sort: '-added',
        limit: PER_SHELF,
        // Where the hub left off, not how many rows are on screen: a dedupe
        // makes those differ, and asking from the smaller number re-fetches
        // rows this shelf already has, for ever.
        offset: shelf.served,
      })
      // The list this page belongs to may be gone.
      if ((generation[id] ?? 0) !== mine) return 'end'
      const held = extra.value[id] ?? { rows: [], served: 0 }
      extra.value = {
        ...extra.value,
        [id]: {
          rows: appendPage(held.rows, page.items),
          served: held.served + page.items.length,
        },
      }
      return 'ok'
    } catch {
      // Reported by the caller, which knows the library's name. A lane that
      // stops growing is indistinguishable from one that has reached the end
      // of its library, so silence here is a lie by omission.
      return 'failed'
    } finally {
      busy.delete(id)
    }
  }

  /// One shelf, asked again. Its pages go with it: they were pages of a list
  /// that failed to load, and keeping them would splice the new first page
  /// onto somebody else's scroll position.
  ///
  /// By query key, not by position. `libraries.value` and the query results
  /// are two arrays that update in two steps, so an index taken from one and
  /// used against the other can be a library out of date — and a `findIndex`
  /// that missed returned `undefined?.refetch()`, which reported success
  /// having done nothing.
  async function retry(shelf: Shelf): Promise<boolean> {
    const id = shelf.library.id
    generation[id] = (generation[id] ?? 0) + 1
    extra.value = { ...extra.value, [id]: { rows: [], served: 0 } }
    await client.refetchQueries({ queryKey: ['shelf', id] })
    return client.getQueryState(['shelf', id])?.status === 'success'
  }

  return { shelves, more, retry }
}
