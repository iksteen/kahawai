/// The home screen's shelves. The rule under most of these: a shelf that
/// failed is not an empty one — conflating them deleted whole libraries from
/// the home screen with nothing said.

import { describe, expect, test } from 'vitest'

import type { ItemRowI64 } from '../src/api/generated/model/itemRowI64.ts'
import {
  appendPage,
  cardRatio,
  emptyHomeText,
  hasMore,
  reachable,
  type Shelf,
  shown,
} from '../src/domain/shelves.ts'

const item = (id: string) => ({ id }) as ItemRowI64
const shelf = (over: Partial<Shelf>): Shelf => ({
  library: { id: 'films', name: 'Films', media_type: 'movies' },
  items: [],
  total: 0,
  served: 0,
  state: 'ready',
  ...over,
})

describe('which shelves are drawn', () => {
  test('a library with nothing in it gets none', () => {
    // An empty rail under a heading reads as a failure to load.
    expect(shown([shelf({ items: [], state: 'ready' })])).toEqual([])
  })

  test('but one that would not load keeps its place, so it can say so', () => {
    // The case that must not look like the other: a library that would not
    // load used to vanish from the home screen entirely.
    expect(shown([shelf({ items: [], state: 'failed' })])).toHaveLength(1)
  })

  test('and one that has not answered is not judged yet', () => {
    expect(shown([shelf({ items: [], state: 'pending' })])).toHaveLength(1)
  })
})

describe('paging a shelf', () => {
  test('appends what is new', () => {
    expect(appendPage([item('a')], [item('b')]).map((i) => i.id)).toEqual(['a', 'b'])
  })

  test('and drops what it already has', () => {
    // A rescan between two pages shifts what sits at an offset; appending the
    // duplicate gives two rows one key.
    expect(appendPage([item('a'), item('b')], [item('b'), item('c')]).map((i) => i.id)).toEqual([
      'a',
      'b',
      'c',
    ])
  })

  test('there is more to ask for until the library runs out', () => {
    expect(hasMore({ served: 1, total: 9 })).toBe(true)
    expect(hasMore({ served: 1, total: 1 })).toBe(false)
  })

  test('and what was served decides it, not what is on screen', () => {
    // A page that came back holding rows this shelf already had leaves fewer
    // items than rows served. Comparing the deduped count would ask the hub
    // for the same offset for ever.
    expect(hasMore({ served: 20, total: 20 })).toBe(false)
  })
})

describe('the shape of a card', () => {
  test('a sleeve is square and a poster is two by three', () => {
    expect(cardRatio('music')).toBe('1')
    expect(cardRatio('movies')).toBe('2 / 3')
    expect(cardRatio('shows')).toBe('2 / 3')
  })
})

describe('a home screen with nothing on it', () => {
  test('tells an operator to connect a mediahost', () => {
    expect(emptyHomeText(true)).toContain('mediahost')
  })

  test('and tells everybody else to ask one', () => {
    // An account with no grants is not an operator, and the day-one
    // experience of a user nobody had granted anything was an instruction to
    // install server software.
    expect(emptyHomeText(false)).not.toContain('mediahost')
    expect(emptyHomeText(false)).toContain('Ask')
  })
})

describe('continue watching', () => {
  test('drops what has nowhere to open', () => {
    // An item in no library has no page: the URL is /library/{id}/item/{id}.
    const rows = [
      { id: 'a', library_id: 'films' },
      { id: 'b', library_id: null },
    ]
    expect(reachable(rows).map((i) => i.id)).toEqual(['a'])
  })
})
