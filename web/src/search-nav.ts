/// The rows of the search overlay, and moving a highlight through them.
///
/// One flat list, because the keyboard walks headings and hits alike: a heading
/// is a row you can land on and press.

import type { Item, LibrarySummary } from './api'

/// Shared because a mismatch is silent: the input points `aria-activedescendant`
/// at these ids, the rows carry them, and a wrong one still lights up on screen
/// while announcing nothing. The inner LIST, not the panel that scrolls.
export const SEARCH_LIST_ID = 'search-results-list'
export const searchOptionId = (i: number) => `search-opt-${i}`

/// Hits from one library, as the search returns them.
export type LibraryHits = { library: LibrarySummary; items: Item[]; total: number }

/// A row the panel renders and the keyboard can land on.
export type SearchRow =
  | { kind: 'library'; library: LibrarySummary; total: number; shown: number }
  | { kind: 'item'; item: Item; library: LibrarySummary }

/// Heading, then that library's items. An empty library contributes no heading:
/// one over nothing reads as "we looked and found some".
export function searchRows(hits: LibraryHits[]): SearchRow[] {
  const rows: SearchRow[] = []
  for (const h of hits) {
    if (h.items.length === 0) continue
    rows.push({ kind: 'library', library: h.library, total: h.total, shown: h.items.length })
    for (const item of h.items) rows.push({ kind: 'item', item, library: h.library })
  }
  return rows
}

/// `shown` is the rows that came back, never the limit asked for — three
/// matches must not read as "5 of 3". The gap to `total` is why you would press
/// the heading.
export function countLabel(shown: number, total: number): string {
  return shown < total ? `${shown} of ${total}` : String(total)
}

/// `-1` is nothing highlighted, where every query starts: down enters the list,
/// up stays out of it. Clamps rather than wraps — wrapping a list you cannot see
/// all of loses your place silently.
export function moveHighlight(count: number, from: number, delta: number): number {
  if (count === 0) return -1
  if (from < 0) return delta > 0 ? 0 : -1
  const next = from + delta
  if (next < 0) return 0
  if (next >= count) return count - 1
  return next
}
