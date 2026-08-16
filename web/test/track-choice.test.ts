import { expect, test } from 'vitest'
import { initialSubtitle, needsBurnRestart } from '../src/domain/track-choice.ts'
import type { PlaybackStreams } from '../src/api/generated/model/playbackStreams.ts'
import type { TrackListing } from '../src/api/generated/model/trackListing.ts'

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
