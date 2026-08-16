/// The home screen's shelves, as data.
///
/// One shelf per library, and the three states each can be in. The rule this
/// module exists for: **a shelf that failed is not an empty one.** An empty
/// library genuinely has no shelf and is dropped; conflating the two deleted
/// whole libraries from the home screen with nothing said.

import type { ItemRowI64 } from '../api/generated/model/itemRowI64.ts'

export type LibrarySummary = { id: string; name: string; media_type: string }

export type Shelf = {
  library: LibrarySummary
  items: ItemRowI64[]
  /// How many the library holds, which is not how many arrived: a shelf pages
  /// as it is scrolled and stops when it has them all.
  total: number
  /// How many rows the hub has HANDED OVER, which is not `items.length`: a
  /// rescan between two pages shifts what sits at an offset, so a page can
  /// arrive holding rows this shelf already had. Both numbers are needed —
  /// this one is where the next page starts, and `items.length` is what is on
  /// screen.
  served: number
  /// `pending` until it has answered. Every library gets one the moment the
  /// list of libraries is known, so the page has its shape before any content
  /// arrives — on a slow link that was several seconds of blank.
  state: 'pending' | 'ready' | 'failed'
}

/// Which shelves are drawn.
///
/// A library with nothing in it gets none: an empty rail under a heading reads
/// as a failure to load. That judgement can only be made once it has answered
/// — before then it is a skeleton, and if it failed it keeps its heading so it
/// can say so.
export function shown(shelves: Shelf[]): Shelf[] {
  return shelves.filter((s) => s.state !== 'ready' || s.items.length > 0)
}

/// The next page, appended without duplicates.
///
/// By id, not by length: a rescan between two pages can shift what sits at an
/// offset, and appending a duplicate gives two rows the same key.
export function appendPage(have: ItemRowI64[], arrived: ItemRowI64[]): ItemRowI64[] {
  const seen = new Set(have.map((i) => i.id))
  return [...have, ...arrived.filter((i) => !seen.has(i.id))]
}

/// Whether there is more to ask for. Compared against what the hub has SERVED
/// rather than what is on screen: a page that came back short because of a
/// dedupe is not the end of the library, and comparing the deduped count would
/// ask for the same rows again for ever.
export function hasMore(shelf: Pick<Shelf, 'served' | 'total'>): boolean {
  return shelf.served < shelf.total
}

/// A sleeve is square and a poster is two by three.
export function cardRatio(mediaType: string): string {
  return mediaType === 'music' ? '1' : '2 / 3'
}

/// What to say when there are no libraries at all.
///
/// An account with no grants is not an operator: `list_libraries` answers an
/// empty list rather than a 403, so the day-one experience of a user nobody
/// has granted anything was an instruction to install server software.
export function emptyHomeText(admin: boolean): string {
  return admin
    ? 'No libraries yet. Connect a mediahost — each collection it announces becomes a library here.'
    : 'Nothing here yet. Ask whoever runs this hub to give your account access to a library.'
}

/// An item in no library has nowhere for its page to live: the URL is
/// /library/{id}/item/{id}. Only an unrestricted account can see one at all.
export function reachable<T extends { library_id: string | null }>(items: T[]): T[] {
  return items.filter((i) => i.library_id)
}
