/// The search panel's rows and its highlight. All of it is about telling two
/// things apart that look the same: no matches from could not ask, and a
/// count of what came back from the limit that was asked for.

import { describe, expect, test } from 'vitest'

import type { ItemRowI64 } from '../src/api/generated/model/itemRowI64.ts'
import {
  countLabel,
  type LibraryHits,
  moveHighlight,
  searchOptionId,
  searchRows,
  searchTrouble,
} from '../src/domain/search-nav.ts'

const films = { id: 'films', name: 'Films', media_type: 'movies' }
const music = { id: 'music', name: 'Music', media_type: 'music' }
const item = (id: string) => ({ id, title: id }) as ItemRowI64
const hits = (
  library: typeof films,
  items: ItemRowI64[],
  total = items.length,
  failure = '',
): LibraryHits => ({ library, items, total, failure })

describe('the rows', () => {
  test('are a heading and then that library’s hits', () => {
    const rows = searchRows([hits(films, [item('a'), item('b')])])
    expect(rows.map((r) => r.kind)).toEqual(['library', 'item', 'item'])
  })

  test('a library with no matches contributes no heading', () => {
    // A heading over nothing reads as "we looked and found some".
    expect(searchRows([hits(films, []), hits(music, [item('a')])]).map((r) => r.kind)).toEqual([
      'library',
      'item',
    ])
  })

  test('and the first row is always a heading, which is what Enter falls to', () => {
    // Enter with nothing highlighted shows everything the first library
    // matched, rather than guessing at one film out of it.
    const rows = searchRows([hits(films, [item('a')]), hits(music, [item('b')])])
    expect(rows[0]!.kind).toBe('library')
  })
})

describe('the count on a heading', () => {
  test('is the total when everything matched is showing', () => {
    expect(countLabel(3, 3)).toBe('3')
  })

  test('and says how many of how many when it is not', () => {
    // The gap is why you would press the heading.
    expect(countLabel(5, 42)).toBe('5 of 42')
  })

  test('it never claims more than the library holds', () => {
    // `shown` is the rows that came back, never the limit asked for: three
    // matches must not read as "5 of 3".
    expect(countLabel(3, 3)).not.toContain('of')
  })
})

describe('walking the list', () => {
  test('down enters it and up stays out', () => {
    // -1 is where every query starts.
    expect(moveHighlight(3, -1, 1)).toBe(0)
    expect(moveHighlight(3, -1, -1)).toBe(-1)
  })

  test('and it clamps rather than wraps', () => {
    // Wrapping a list you cannot see all of loses your place silently.
    expect(moveHighlight(3, 2, 1)).toBe(2)
    expect(moveHighlight(3, 0, -1)).toBe(0)
  })

  test('an empty list has nothing to land on', () => {
    expect(moveHighlight(0, 1, 1)).toBe(-1)
    expect(moveHighlight(0, -1, -1)).toBe(-1)
  })

  test('and each row has an id of its own for the input to point at', () => {
    expect(searchOptionId(0)).not.toBe(searchOptionId(1))
  })
})

describe('libraries that would not answer', () => {
  test('nothing is said when they all did', () => {
    expect(searchTrouble([hits(films, [item('a')]), hits(music, [])])).toBe('')
  })

  test('all of them failing is reported as one thing going wrong', () => {
    const trouble = searchTrouble([
      hits(films, [], 0, 'Could not reach the hub.'),
      hits(music, [], 0, 'Could not reach the hub.'),
    ])
    expect(trouble).toContain('Could not reach the hub.')
  })

  test('and some of them failing names them', () => {
    // Two libraries erroring beside one with no matches printed "nothing
    // matches" over a count of one, and the only mention of the two was a
    // notice gone in five seconds.
    const trouble = searchTrouble([hits(films, [item('a')]), hits(music, [], 0, 'nope')])
    expect(trouble).toBe('Could not search Music.')
  })
})
