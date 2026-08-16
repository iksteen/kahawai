/// The play queue. Two things run through all of it: a record and a single
/// track are levelled differently on purpose, and removing an entry must not
/// change which track is playing unless it removed that one.

import { describe, expect, test } from 'vitest'

import type { ItemRowI64 } from '../src/api/generated/model/itemRowI64.ts'
import {
  advance,
  appendAlbum,
  appendTrack,
  current,
  EMPTY,
  playAlbum,
  removeAt,
  replayGainFactor,
  upNext,
} from '../src/domain/queue.ts'

const track = (id: string, gain?: Record<string, number | null>) =>
  ({ id, title: id, replay_gain: gain ?? null }) as ItemRowI64

const three = [track('a'), track('b'), track('c')]

describe('putting a record on', () => {
  test('replaces whatever was queued, from where you pressed', () => {
    const queue = playAlbum(three, 1)
    expect(queue.entries.map((e) => e.track.id)).toEqual(['a', 'b', 'c'])
    expect(current(queue)?.track.id).toBe('b')
  })

  test('and it is levelled as a record', () => {
    // Per-track levelling across a record flattens the quiet track the artist
    // meant to be quiet.
    expect(playAlbum(three, 0).entries.every((e) => e.gain === 'album')).toBe(true)
  })
})

describe('adding to what is playing', () => {
  test('a record goes on the end and does not interrupt', () => {
    const queue = appendAlbum(playAlbum(three, 1), [track('d')])
    expect(queue.entries.map((e) => e.track.id)).toEqual(['a', 'b', 'c', 'd'])
    expect(current(queue)?.track.id).toBe('b')
  })

  test('and a single track is levelled by itself', () => {
    // It is not arriving as part of a record, and levelling it as one would
    // land it at a different loudness from its neighbours.
    const queue = appendTrack(playAlbum(three, 0), track('single'))
    expect(queue.entries.at(-1)).toEqual({ track: track('single'), gain: 'track' })
  })
})

describe('dropping one entry', () => {
  test('one before the playing track does not change what is playing', () => {
    // UI-2. The index shifts down by one; the music does not. Four tracks and
    // a middle position, because with the playing track at the END both the
    // right answer and a clamp give the same index.
    const four = [...three, track('d')]
    const queue = removeAt(playAlbum(four, 1), 0)
    expect(queue.entries.map((e) => e.track.id)).toEqual(['b', 'c', 'd'])
    expect(current(queue)?.track.id).toBe('b')
  })

  test('one after it does not either', () => {
    const queue = removeAt(playAlbum(three, 0), 2)
    expect(current(queue)?.track.id).toBe('a')
  })

  test('the playing one moves to whatever takes its place', () => {
    const queue = removeAt(playAlbum(three, 1), 1)
    expect(queue.entries.map((e) => e.track.id)).toEqual(['a', 'c'])
    expect(current(queue)?.track.id).toBe('c')
  })

  test('and the last one leaves the queue on the new last', () => {
    const queue = removeAt(playAlbum(three, 2), 2)
    expect(current(queue)?.track.id).toBe('b')
  })

  test('removing the only entry empties it', () => {
    expect(removeAt(playAlbum([track('a')], 0), 0)).toEqual(EMPTY)
  })

  test('and an index that is not there changes nothing', () => {
    const queue = playAlbum(three, 1)
    expect(removeAt(queue, 9)).toBe(queue)
    expect(removeAt(queue, -1)).toBe(queue)
  })
})

describe('moving through it', () => {
  test('forwards and back', () => {
    const queue = playAlbum(three, 1)
    expect(current(advance(queue, 1)!)?.track.id).toBe('c')
    expect(current(advance(queue, -1)!)?.track.id).toBe('a')
  })

  test('and it stops at the ends rather than wrapping', () => {
    // A record that has finished has finished.
    expect(advance(playAlbum(three, 2), 1)).toBeNull()
    expect(advance(playAlbum(three, 0), -1)).toBeNull()
  })

  test('the one after it is what gets warmed up', () => {
    // Gapless: preparing the next track once the current one ends costs a
    // round trip plus buffering, audible on every boundary.
    expect(upNext(playAlbum(three, 0))?.track.id).toBe('b')
    expect(upNext(playAlbum(three, 2))).toBeUndefined()
  })
})

describe('how loud to play it', () => {
  test('a record uses the album measurement, a single the track one', () => {
    const both = track('x', { album_gain_db: -6, track_gain_db: -12 })
    expect(replayGainFactor(both, 'album')).toBeCloseTo(10 ** (-6 / 20))
    expect(replayGainFactor(both, 'track')).toBeCloseTo(10 ** (-12 / 20))
  })

  test('and falls back to the other rather than to nothing', () => {
    // An album rip missing its album tags still levels better by track than
    // not at all.
    const trackOnly = track('x', { album_gain_db: null, track_gain_db: -6 })
    expect(replayGainFactor(trackOnly, 'album')).toBeCloseTo(10 ** (-6 / 20))
  })

  test('a file that says nothing is played as it is', () => {
    expect(replayGainFactor(track('x'), 'album')).toBe(1)
    expect(replayGainFactor(undefined, 'album')).toBe(1)
    expect(
      replayGainFactor(track('x', { album_gain_db: null, track_gain_db: null }), 'album'),
    ).toBe(1)
  })

  test('and a gain that would clip is capped at the peak', () => {
    // A positive gain applied to a track that already reaches full scale
    // would clip it.
    const loud = track('x', { album_gain_db: 6, album_peak: 0.9 })
    expect(replayGainFactor(loud, 'album')).toBeCloseTo(1 / 0.9)
  })

  test('but a gain that does not is left alone', () => {
    const quiet = track('x', { album_gain_db: -6, album_peak: 0.9 })
    expect(replayGainFactor(quiet, 'album')).toBeCloseTo(10 ** (-6 / 20))
  })
})
