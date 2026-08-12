import assert from 'node:assert/strict'
import test from 'node:test'
import { initialSubtitle, needsBurnRestart } from '../src/track-choice.ts'
import type { StreamVerdict, Subtitle } from '../src/api.ts'

const sub = (p: Partial<Subtitle>) =>
  ({
    id: 1,
    origin: 'embedded',
    format: 'srt',
    delivery: 'text',
    language: 'en',
    ...p,
  }) as Subtitle

test('a remembered track wins over the language wish', () => {
  const subs = [sub({ id: 7, language: 'ja' }), sub({ id: 8, language: 'en' })]
  assert.equal(initialSubtitle({ subs, exactId: 7, wishlist: ['en'] })?.id, 7)
})

test('a remembered track the hub cannot serve is not a choice', () => {
  // The wishlist gets its turn rather than the viewer getting nothing.
  const subs = [sub({ id: 7, delivery: 'none', language: 'ja' }), sub({ id: 8, language: 'en' })]
  assert.equal(initialSubtitle({ subs, exactId: 7, wishlist: ['en'] })?.id, 8)
})

test('with nothing remembered the wishlist decides', () => {
  const subs = [sub({ id: 8, language: 'en' }), sub({ id: 9, language: 'nl' })]
  assert.equal(initialSubtitle({ subs, exactId: null, wishlist: ['nl'] })?.id, 9)
})

test('no memory and no match is no subtitle', () => {
  const subs = [sub({ id: 8, language: 'de' })]
  assert.equal(initialSubtitle({ subs, exactId: 42, wishlist: ['fr'] }), null)
})

test('a remembered burn re-applies only when this session is not already burning it', () => {
  const burn = sub({ id: 5, delivery: 'burn' })
  const burning: StreamVerdict = {
    video: 'h264',
    audio: 'aac',
    subtitles: [{ track_id: 5, tier: 'burn' }],
  } as StreamVerdict
  // Already burnt into the picture being watched: restarting would re-encode
  // exactly what is on screen.
  assert.equal(needsBurnRestart(burn, burning), false)
  assert.equal(needsBurnRestart(burn, undefined), true)
  // Burning a DIFFERENT track is not this track.
  const other: StreamVerdict = {
    video: 'h264',
    audio: 'aac',
    subtitles: [{ track_id: 6, tier: 'burn' }],
  } as StreamVerdict
  assert.equal(needsBurnRestart(burn, other), true)
})

test('a client-rendered track never asks for a restart', () => {
  for (const delivery of ['text', 'ass', 'overlay', 'none'] as const) {
    assert.equal(needsBurnRestart(sub({ id: 5, delivery }), undefined), false, delivery)
  }
})
