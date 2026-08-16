/// How an item page lays itself out. Most of this is about a series whose
/// episodes are numbered straight through: which numbering is on screen has to
/// be the same here and on the season page, or opening "Season 2" lands on a
/// different set of episodes than the list it was clicked from.

import { describe, expect, test } from 'vitest'

import { artShape, childCount, continueAt, ordered, seasonsIn } from '../src/domain/detail.ts'

const ep = (
  over: Partial<Parameters<typeof continueAt>[0] extends (infer T)[] | null ? T : never>,
) =>
  ({
    season: null,
    proj_season: null,
    episode: null,
    proj_episode: null,
    played: false,
    ...over,
  }) as {
    season: number | null
    proj_season: number | null
    episode: number | null
    proj_episode: number | null
    played: boolean
  }

describe('the artwork', () => {
  test('follows what the artwork is, not what the page is about', () => {
    // A track's art is its album's square sleeve; an episode's is a still.
    expect(artShape('album').ratio).toBe('1')
    expect(artShape('track').ratio).toBe('1')
    expect(artShape('episode').ratio).toBe('16 / 9')
    expect(artShape('movie').ratio).toBe('2 / 3')
    expect(artShape('show').ratio).toBe('2 / 3')
  })

  test('and a still is wider than a poster', () => {
    expect(artShape('episode').width).not.toBe(artShape('movie').width)
  })
})

describe('ordering a series', () => {
  test('a projection sorts by the numbers the viewer sees', () => {
    const eps = [
      ep({ proj_season: 2, proj_episode: 1, episode: 26 }),
      ep({ proj_season: 1, proj_episode: 1, episode: 1 }),
    ]
    expect(ordered(eps, true).map((e) => e.episode)).toEqual([1, 26])
  })

  test('and without one the hub’s order stands', () => {
    // Re-sorting would be a second opinion about an order that is already
    // right.
    const eps = [ep({ season: 2, episode: 1 }), ep({ season: 1, episode: 1 })]
    expect(ordered(eps, false).map((e) => e.season)).toEqual([2, 1])
  })

  test('the list it was given is left alone', () => {
    // It is the query cache's own array: sorting in place silently re-orders
    // what `continueAt` and the season strip read from it.
    const eps = [ep({ proj_season: 2, proj_episode: 1 }), ep({ proj_season: 1, proj_episode: 1 })]
    ordered(eps, true)
    expect(eps.map((e) => e.proj_season)).toEqual([2, 1])
  })

  test('and within a season it is by episode', () => {
    const eps = [
      ep({ proj_season: 1, proj_episode: 3 }),
      ep({ proj_season: 1, proj_episode: 1 }),
      ep({ proj_season: 1, proj_episode: 2 }),
    ]
    expect(ordered(eps, true).map((e) => e.proj_episode)).toEqual([1, 2, 3])
  })

  test('an episode with no season sorts last, not first', () => {
    // Specials and stragglers go under the seasons, not above them.
    const eps = [
      ep({ proj_season: null, proj_episode: 1 }),
      ep({ proj_season: 3, proj_episode: 1 }),
    ]
    expect(ordered(eps, true).map((e) => e.proj_season)).toEqual([3, null])
  })
})

describe('the seasons a series falls into', () => {
  test('are each named once, in the order they appear', () => {
    const eps = [ep({ season: 1 }), ep({ season: 1 }), ep({ season: 2 }), ep({ season: 0 })]
    expect(seasonsIn(eps, false)).toEqual([1, 2, 0])
  })

  test('and under a projection they are the projected ones', () => {
    const eps = [ep({ season: null, proj_season: 1 }), ep({ season: null, proj_season: 2 })]
    expect(seasonsIn(eps, true)).toEqual([1, 2])
    expect(seasonsIn(eps, false)).toEqual([null])
  })
})

describe('where to carry on', () => {
  test('is the first one nobody has watched', () => {
    const eps = [ep({ played: true, episode: 1 }), ep({ episode: 2 }), ep({ episode: 3 })]
    expect(continueAt(eps)?.episode).toBe(2)
  })

  test('and there is nowhere to carry on to once it is all watched', () => {
    expect(continueAt([ep({ played: true })])).toBeUndefined()
  })

  test('nothing at all while the list has not answered', () => {
    // "Start from the beginning" is the wrong answer to "we have not asked
    // yet", and it flashed in as the list arrived.
    expect(continueAt(null)).toBeUndefined()
  })
})

describe('the count under the title', () => {
  test('says how many and how many of those are watched', () => {
    expect(childCount([ep({ played: true }), ep({})], 'episode', 'episodes')).toBe(
      '2 episodes · 1 watched',
    )
  })

  test('and counts one of them as one', () => {
    expect(childCount([ep({})], 'track', 'tracks')).toBe('1 track · 0 watched')
  })

  test('but says nothing before the list has answered', () => {
    // A count of zero is a fact. Printing it before asking made every album
    // read "0 tracks" with both actions disabled for a round trip — disabled
    // because the data was absent, which is the one thing a disabled control
    // must not mean.
    expect(childCount(null, 'track', 'tracks')).toBe('')
    // And an empty list IS zero, once it has answered.
    expect(childCount([], 'track', 'tracks')).toBe('0 tracks · 0 watched')
  })
})
