/// The rows of the search panel, and moving a highlight through them.
///
/// One flat list, because the keyboard walks headings and hits alike: a
/// heading is a row you can land on and press.

import type { ItemRowI64 } from '../api/generated/model/itemRowI64.ts'
import type { LibrarySummary } from './shelves.ts'

/// Shared because a mismatch is silent: the input points
/// `aria-activedescendant` at these ids, the rows carry them, and a wrong one
/// still lights up on screen while announcing nothing. The inner LIST, not the
/// panel that scrolls.
export const SEARCH_LIST_ID = 'search-results-list'
export const searchOptionId = (i: number) => `search-opt-${i}`

/// Hits from one library, as the search returns them.
export type LibraryHits = {
  library: LibrarySummary
  items: ItemRowI64[]
  total: number
  /// Empty when it answered. A library that could not be asked is a different
  /// answer from one with no matches, and an empty list gives the first when
  /// it means the second.
  failure: string
}

/// A row the panel renders and the keyboard can land on.
export type SearchRow =
  | { kind: 'library'; library: LibrarySummary; total: number; shown: number }
  | { kind: 'item'; item: ItemRowI64; library: LibrarySummary }

/// Heading, then that library's items. An empty library contributes no
/// heading: one over nothing reads as "we looked and found some".
export function searchRows(hits: LibraryHits[]): SearchRow[] {
  const rows: SearchRow[] = []
  for (const hit of hits) {
    if (hit.items.length === 0) continue
    rows.push({
      kind: 'library',
      library: hit.library,
      total: hit.total,
      shown: hit.items.length,
    })
    for (const item of hit.items) rows.push({ kind: 'item', item, library: hit.library })
  }
  return rows
}

/// `shown` is the rows that came back, never the limit asked for — three
/// matches must not read as "5 of 3". The gap to `total` is why you would
/// press the heading.
export function countLabel(shown: number, total: number): string {
  return shown < total ? `${shown} of ${total}` : String(total)
}

/// `-1` is nothing highlighted, where every query starts: down enters the
/// list, up stays out of it. Clamps rather than wraps — wrapping a list you
/// cannot see all of loses your place silently.
export function moveHighlight(count: number, from: number, delta: number): number {
  if (count === 0) return -1
  if (from < 0) return delta > 0 ? 0 : -1
  const next = from + delta
  if (next < 0) return 0
  if (next >= count) return count - 1
  return next
}

/// What one notice says about a set of libraries that would not answer.
///
/// One notice for the whole search, once every library has answered, so it can
/// say whether this was a bad connection or one bad library. Per-library
/// notices are latest-wins: one would name whichever failed last and imply the
/// rest were fine.
export function searchTrouble(hits: LibraryHits[]): string {
  const broke = hits.filter((h) => h.failure !== '')
  if (broke.length === 0) return ''
  if (broke.length === hits.length) return `Could not search — ${broke[0]!.failure}`
  return `Could not search ${broke.map((h) => h.library.name).join(', ')}.`
}
