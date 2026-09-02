import { describe, expect, test } from 'vitest'

import {
  awayFrom,
  boundaryKey,
  hasSearchBox,
  hasSearchPanel,
  parseSeason,
  type RouteName,
  seasonSegment,
} from '../src/domain/routes.ts'

/// Every screen there is. Listed so a new one has to be classified here rather
/// than inheriting whatever the fallthrough happens to say — and so these two
/// tables are checked over the whole domain instead of over the five names
/// somebody thought of. Both were: `detail` and `season` were in neither test,
/// and a mutation putting a results panel on both passed.
const EVERY: RouteName[] = [
  'libraries',
  'library',
  'artist',
  'artist-album',
  'admin',
  'settings',
  'detail',
  'season',
  'player',
]

describe('what counts as the same screen', () => {
  test('an autoplay handover stays one screen', () => {
    // The address changes to the next episode and nothing else does. Keyed on
    // the address, that remounted the player and orphaned its session — one
    // leaked transcoder slot per episode boundary.
    const first = boundaryKey('player', '/app/library/shows/item/e1/play', 'shows')
    const second = boundaryKey('player', '/app/library/shows/item/e2/play', 'shows')
    expect(second).toBe(first)
  })

  test('a player with no library is still not every player', () => {
    // The undefined branch: two libraries' players must not share a boundary
    // just because neither address named one.
    expect(boundaryKey('player', '/app/library/a/item/e1/play')).not.toBe(
      boundaryKey('player', '/app/library/b/item/e1/play', 'b'),
    )
  })

  test('two items are two screens', () => {
    // On the view alone this was not enough: one item's caught throw stayed
    // latched over the next, which would have rendered perfectly well.
    expect(boundaryKey('detail', '/app/library/films/item/a')).not.toBe(
      boundaryKey('detail', '/app/library/films/item/b'),
    )
  })

  test('leaving the player is a different screen', () => {
    expect(boundaryKey('player', '/app/library/shows/item/e1/play', 'shows')).not.toBe(
      boundaryKey('detail', '/app/library/shows/item/e1'),
    )
  })
})

describe('what the search box means', () => {
  test('a panel only where there is something to put in it', () => {
    // A library page filters in place; a dropdown of cross-library hits over
    // it would be two answers to one question. An item page and a season have
    // nothing to search, and the player has no box at all.
    const withPanel = EVERY.filter((name) => hasSearchPanel(name))
    expect(withPanel).toEqual(['libraries'])
  })

  test('no box at all where nothing is searchable', () => {
    // A box that silently does nothing is worse than no box.
    const withBox = EVERY.filter((name) => hasSearchBox(name))
    expect(withBox).toEqual(['libraries', 'library', 'artist'])
  })

  test('and nothing has a panel without a box to open it from', () => {
    expect(EVERY.filter((name) => hasSearchPanel(name) && !hasSearchBox(name))).toEqual([])
  })
})

describe('where a broken screen can send you', () => {
  test('every screen but home offers home', () => {
    expect(EVERY.filter((name) => awayFrom(name) === undefined)).toEqual(['libraries'])
  })

  test('and home offers nothing, because pressing it would do nothing', () => {
    // The boundary clears when the ADDRESS changes. Going home from home does
    // not change it, so the offer would be a button that leaves the error up.
    expect(awayFrom('libraries')).toBeUndefined()
  })
})

describe('a season in an address', () => {
  test('absolute numbering has a spelling of its own', () => {
    // Null is a real answer about an anime, not a missing value, so it cannot
    // be an empty segment.
    expect(seasonSegment(null)).toBe('all')
    expect(parseSeason('all')).toBeNull()
    expect(seasonSegment(2)).toBe('2')
    expect(parseSeason('2')).toBe(2)
  })
})
