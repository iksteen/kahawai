/// How a row names itself. Every case here is one somebody would notice: a
/// question mark where a season should be, four albums called Greatest Hits,
/// a progress bar past the end of the bar.

import { describe, expect, test } from 'vitest'

import { type Labelled, metaLine, seLabel, targetOf, watchedPct } from '../src/domain/label.ts'

const row = (over: Partial<Labelled>): Labelled => ({
  id: 'i1',
  kind: 'movie',
  title: 'Heat',
  artist: null,
  year: null,
  season: null,
  episode: null,
  episode_end: null,
  parent_id: null,
  parent_title: null,
  resume_position_ms: null,
  resume_duration_ms: null,
  ...over,
})

describe('episode numbering', () => {
  test('a seasoned episode is S01E02', () => {
    expect(seLabel(1, 2)).toBe('S01E02')
  })

  test('absolute numbering has no season, and no question mark either', () => {
    // Anime is numbered that way on purpose. "S?E11" reads as data we failed
    // to load, which is a different and much worse claim.
    expect(seLabel(null, 11)).toBe('E11')
    expect(seLabel(null, 11)).not.toContain('?')
  })

  test('specials are season zero, not absolute', () => {
    // 0 and null are different things and `??` would collapse them.
    expect(seLabel(0, 3)).toBe('S00E03')
  })

  test('a batch file spans its range', () => {
    expect(seLabel(1, 1, 2)).toBe('S01E01-02')
    expect(seLabel(null, 1, 2)).toBe('E01-02')
  })
})

describe('the line under a card', () => {
  test('an episode says which show it is from', () => {
    // A row called "Pilot" is otherwise one of eight.
    expect(
      metaLine(
        row({ kind: 'episode', title: 'Pilot', parent_title: 'Fringe', season: 1, episode: 1 }),
      ),
    ).toBe('Fringe S01E01')
  })

  test('an album is told apart by who made it and when', () => {
    // There are four Greatest Hits by four bands and three pressings of one.
    expect(metaLine(row({ kind: 'album', artist: 'Queen', year: 1981 }))).toBe('Queen · 1981')
  })

  test('and a missing half is left out rather than punctuated around', () => {
    expect(metaLine(row({ kind: 'album', artist: 'Queen', year: null }))).toBe('Queen')
    expect(metaLine(row({ kind: 'album', artist: null, year: 1981 }))).toBe('1981')
    expect(metaLine(row({ kind: 'album' }))).toBe('')
  })

  test('a track names its artist and its album', () => {
    expect(metaLine(row({ kind: 'track', artist: 'Queen', parent_title: 'Hot Space' }))).toBe(
      'Queen · Hot Space',
    )
  })

  test('and anything else is just its year', () => {
    expect(metaLine(row({ kind: 'movie', year: 1995 }))).toBe('1995')
    expect(metaLine(row({ kind: 'movie', year: null }))).toBe('')
  })
})

describe('where a card leads', () => {
  test('a track opens its album, because a track has no page', () => {
    expect(targetOf(row({ kind: 'track', id: 't1', parent_id: 'a1' }))).toBe('a1')
  })

  test('a track with no album still opens something', () => {
    // Nothing else to offer, and a dead card is worse than a thin page.
    expect(targetOf(row({ kind: 'track', id: 't1', parent_id: null }))).toBe('t1')
  })

  test('everything else opens itself', () => {
    expect(targetOf(row({ kind: 'episode', id: 'e1', parent_id: 's1' }))).toBe('e1')
  })
})

describe('how far through', () => {
  test('is a percentage of the whole item', () => {
    expect(watchedPct(row({ resume_position_ms: 300, resume_duration_ms: 1200 }))).toBe(25)
  })

  test('is null when there is nothing to measure against', () => {
    // Not zero: "not started" and "at the very beginning" draw differently,
    // and a zero-width bar on every unwatched card is noise.
    expect(watchedPct(row({ resume_position_ms: 300, resume_duration_ms: null }))).toBeNull()
    expect(watchedPct(row({ resume_position_ms: null, resume_duration_ms: 1200 }))).toBeNull()
    expect(watchedPct(row({ resume_position_ms: 0, resume_duration_ms: 1200 }))).toBeNull()
  })

  test('and never runs past the end of the bar', () => {
    // A position past the reported duration is ordinary: the hub's figure is
    // the container's and a player can report a little beyond it.
    expect(watchedPct(row({ resume_position_ms: 1300, resume_duration_ms: 1200 }))).toBe(100)
  })
})
