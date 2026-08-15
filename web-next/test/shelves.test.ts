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
  type LibrarySummary,
  pendingShelves,
  reachable,
  settle,
  shown,
} from '../src/domain/shelves.ts'

const item = (id: string) => ({ id }) as ItemRowI64
const LIBS: LibrarySummary[] = [
  { id: 'films', name: 'Films', media_type: 'movies' },
  { id: 'music', name: 'Music', media_type: 'music' },
]

describe('before anything has answered', () => {
  test('every library already has a shelf', () => {
    // The page has its shape before any content arrives; on a slow link that
    // is the difference between a page and a blank.
    const shelves = pendingShelves(LIBS)
    expect(shelves.map((s) => s.state)).toEqual(['pending', 'pending'])
    expect(shown(shelves)).toHaveLength(2)
  })
})

describe('once they answer', () => {
  test('an empty library gets no shelf', () => {
    // An empty rail under a heading reads as a failure to load.
    const shelves = settle(pendingShelves(LIBS), 'music', { items: [], total: 0 })
    // 'music' answered and had nothing; 'films' has not answered, and a
    // shelf is only judged empty once it has.
    expect(shown(shelves).map((s) => s.library.id)).toEqual(['films'])
    expect(shown(settle(shelves, 'films', { items: [], total: 0 }))).toEqual([])
  })

  test('but a failed one keeps its place, so it can say so', () => {
    // The case that must not look like the other: a library that would not
    // load used to vanish from the home screen entirely.
    const shelves = settle(pendingShelves(LIBS), 'music', 'failed')
    expect(shown(shelves).map((s) => s.library.id)).toContain('music')
    expect(shelves.find((s) => s.library.id === 'music')!.state).toBe('failed')
  })

  test('one answer does not disturb the others', () => {
    // Rebuilding them all would throw away pages already scrolled into.
    const first = settle(pendingShelves(LIBS), 'films', { items: [item('a')], total: 9 })
    const second = settle(first, 'music', 'failed')
    const films = second.find((s) => s.library.id === 'films')!
    expect(films.items).toHaveLength(1)
    expect(films.total).toBe(9)
    expect(films.state).toBe('ready')
  })

  test('and a retry that works clears the failure', () => {
    const failed = settle(pendingShelves(LIBS), 'films', 'failed')
    const fixed = settle(failed, 'films', { items: [item('a')], total: 1 })
    expect(fixed.find((s) => s.library.id === 'films')!.state).toBe('ready')
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
    expect(hasMore({ items: [item('a')], total: 9 })).toBe(true)
    expect(hasMore({ items: [item('a')], total: 1 })).toBe(false)
    // A page that came back short after a dedupe is not the end of a library.
    expect(hasMore({ items: [item('a'), item('b')], total: 20 })).toBe(true)
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
