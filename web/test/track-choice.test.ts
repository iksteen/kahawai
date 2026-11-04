import { expect, test } from 'vitest'
import { initialSubtitle, needsBurnRestart } from '../src/domain/track-choice.ts'
import type { PlaybackStreams } from '../src/api/generated/model/playbackStreams.ts'
import type { TrackListing } from '../src/api/generated/model/trackListing.ts'
import type { SubtitleKind } from '../src/domain/prefs.ts'

const sub = (p: Partial<TrackListing>) =>
  ({
    id: 1,
    origin: 'embedded',
    format: 'srt',
    delivery: 'text',
    language: 'en',
    ...p,
  }) as TrackListing

test('a remembered track wins over the language wish', () => {
  const subs = [sub({ id: 7, language: 'ja' }), sub({ id: 8, language: 'en' })]
  expect(initialSubtitle({ subs, exactId: 7, wishlist: ['en'] })?.id).toBe(7)
})

test('a remembered track the hub cannot serve is not a choice', () => {
  // The wishlist gets its turn rather than the viewer getting nothing.
  const subs = [sub({ id: 7, delivery: 'none', language: 'ja' }), sub({ id: 8, language: 'en' })]
  expect(initialSubtitle({ subs, exactId: 7, wishlist: ['en'] })?.id).toBe(8)
})

test('with nothing remembered the wishlist decides', () => {
  const subs = [sub({ id: 8, language: 'en' }), sub({ id: 9, language: 'nl' })]
  expect(initialSubtitle({ subs, exactId: null, wishlist: ['nl'] })?.id).toBe(9)
})

test('a language wish takes the full track over the forced one listed first', () => {
  // The Silo release: a forced English track first, then the full one and
  // the SDH one. Neither the first English row nor a carried id is right.
  const forced = sub({ id: 1, forced: true, label: 'Forced' })
  const sdh = sub({ id: 3, hearing_impaired: true, label: 'SDH' })
  const subs = [forced, sub({ id: 2 }), sdh, sub({ id: 4, language: 'ar' })]
  expect(initialSubtitle({ subs, exactId: null, wishlist: ['en'] })?.id).toBe(2)
  // SDH before forced when there is no plain track; forced only as the last
  // English reading; commentary never.
  expect(initialSubtitle({ subs: [forced, sdh], exactId: null, wishlist: ['en'] })?.id).toBe(3)
  expect(initialSubtitle({ subs: [forced], exactId: null, wishlist: ['en'] })?.id).toBe(1)
  const commentary = sub({ id: 9, commentary: true })
  expect(initialSubtitle({ subs: [commentary], exactId: null, wishlist: ['en'] })).toBe(null)
})

test('what a track is outranks how it is drawn', () => {
  const subs = [sub({ id: 1, forced: true, delivery: 'ass' }), sub({ id: 2, delivery: 'text' })]
  expect(initialSubtitle({ subs, exactId: null, wishlist: ['en'] })?.id).toBe(2)
})

test.each<[SubtitleKind, number]>([
  ['forced', 1],
  ['regular', 2],
  ['sdh', 3],
])('only %s tracks are selected automatically', (kind, id) => {
  const subs = [
    sub({ id: 1, forced: true }),
    sub({ id: 2 }),
    sub({ id: 3, hearing_impaired: true }),
  ]
  expect(initialSubtitle({ subs, exactId: null, wishlist: ['en'], kind })?.id).toBe(id)
  const others = subs.filter((track) => track.id !== id)
  expect(initialSubtitle({ subs: others, exactId: others[0]!.id, wishlist: ['any'], kind })).toBe(
    null,
  )
})

test('the type filter applies across languages and preserves fidelity within the chosen type', () => {
  const subs = [
    sub({ id: 1, language: 'en', forced: true }),
    sub({ id: 2, language: 'nl', hearing_impaired: true, delivery: 'text' }),
    sub({ id: 3, language: 'nl', hearing_impaired: true, delivery: 'ass' }),
  ]
  expect(initialSubtitle({ subs, exactId: 1, wishlist: ['en', 'nl'], kind: 'sdh' })?.id).toBe(3)
  expect(initialSubtitle({ subs, exactId: 2, wishlist: ['en'], kind: 'sdh' })?.id).toBe(2)
})

test('flags determine type, and forced takes precedence over SDH', () => {
  const both = sub({ id: 1, forced: true, hearing_impaired: true })
  const named = sub({ id: 2, label: 'English [SDH]' })
  expect(
    initialSubtitle({ subs: [both, named], exactId: null, wishlist: ['en'], kind: 'sdh' }),
  ).toBe(null)
  expect(
    initialSubtitle({ subs: [both], exactId: null, wishlist: ['en'], kind: 'forced' })?.id,
  ).toBe(1)
  expect(
    initialSubtitle({ subs: [both, named], exactId: null, wishlist: ['en'], kind: 'regular' })?.id,
  ).toBe(2)
})

test('a type preference never auto-selects unavailable, commentary, bitmap or burn tracks', () => {
  const subs = [
    sub({ id: 1, forced: true, delivery: 'none' }),
    sub({ id: 2, forced: true, commentary: true }),
    sub({ id: 3, forced: true, format: 'pgs', delivery: 'overlay' }),
    sub({ id: 4, forced: true, delivery: 'burn' }),
  ]
  expect(initialSubtitle({ subs, exactId: null, wishlist: ['en'], kind: 'forced' })).toBe(null)
  expect(
    initialSubtitle({ subs: [sub({ forced: true })], exactId: null, wishlist: [], kind: 'forced' }),
  ).toBe(null)
})

test('no memory and no match is no subtitle', () => {
  const subs = [sub({ id: 8, language: 'de' })]
  expect(initialSubtitle({ subs, exactId: 42, wishlist: ['fr'] })).toBe(null)
})

test('a remembered burn re-applies only when this session is not already burning it', () => {
  const burn = sub({ id: 5, delivery: 'burn' })
  const burning: PlaybackStreams = {
    video: 'h264',
    audio: 'aac',
    subtitles: [{ track_id: 5, tier: 'burn' }],
  } as PlaybackStreams
  // Already burnt into the picture being watched: restarting would re-encode
  // exactly what is on screen.
  expect(needsBurnRestart(burn, burning)).toBe(false)
  expect(needsBurnRestart(burn, undefined)).toBe(true)
  // Burning a DIFFERENT track is not this track.
  const other: PlaybackStreams = {
    video: 'h264',
    audio: 'aac',
    subtitles: [{ track_id: 6, tier: 'burn' }],
  } as PlaybackStreams
  expect(needsBurnRestart(burn, other)).toBe(true)
})

test('a client-rendered track never asks for a restart', () => {
  for (const delivery of ['text', 'ass', 'overlay', 'none'] as const) {
    expect(needsBurnRestart(sub({ id: 5, delivery }), undefined)).toBe(false)
  }
})
