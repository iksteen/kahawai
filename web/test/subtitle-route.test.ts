import { expect, test } from 'vitest'
import { subtitleRoute } from '../src/domain/subtitle-route.ts'
import type { TrackListing } from '../src/api/generated/model/trackListing.ts'

const track = (p: Partial<TrackListing>) =>
  ({ delivery: 'text', format: 'srt', origin: 'embedded', ...p }) as TrackListing
const hls = { isHls: true, vttFallback: false }
const file = { isHls: false, vttFallback: false }

test('nothing chosen renders nothing', () => {
  expect(subtitleRoute(undefined, hls)).toBe('none')
})

test('burned is not the same answer as none', () => {
  // One is already in the picture and one cannot be served at all; a route that
  // folded them together would draw an empty overlay over burnt-in text.
  expect(subtitleRoute(track({ delivery: 'burn' }), hls)).toBe('burned')
  expect(subtitleRoute(track({ delivery: 'none' }), hls)).toBe('none')
})

test('the delivery the hub computed decides the renderer', () => {
  expect(subtitleRoute(track({ delivery: 'ass', format: 'ass' }), hls)).toBe('ass')
  expect(subtitleRoute(track({ delivery: 'overlay', format: 'pgs' }), hls)).toBe('image')
})

test('an embedded text track on a live playlist is fed from the tap', () => {
  expect(subtitleRoute(track({ origin: 'embedded' }), hls)).toBe('live-text')
})

test('a direct file has no tap, so the same track uses the vtt element', () => {
  // The case that pins `isHls`: identical track, different session kind.
  expect(subtitleRoute(track({ origin: 'embedded' }), file)).toBe('vtt-track')
})

test('a sidecar has no tap either', () => {
  expect(subtitleRoute(track({ origin: 'sidecar' }), hls)).toBe('vtt-track')
  expect(subtitleRoute(track({ origin: 'downloaded' }), hls)).toBe('vtt-track')
})

test('an ASS-format track this client declined goes to the flattened vtt', () => {
  // Delivery says text — the client did not take the ass route — but the
  // pipeline taps it as .ass and never writes the .jsonl the live path reads.
  // Keying on delivery alone would send it to a tap that cannot exist.
  for (const format of ['ass', 'ssa']) {
    expect(subtitleRoute(track({ format, origin: 'embedded' }), hls)).toBe('vtt-track')
  }
})

test('once the tap has yielded nothing, the same track takes the vtt element', () => {
  expect(subtitleRoute(track({ origin: 'embedded' }), { isHls: true, vttFallback: true })).toBe(
    'vtt-track',
  )
})

test('every delivery has an answer', () => {
  for (const delivery of ['text', 'ass', 'overlay', 'burn', 'none'] as const) {
    const route = subtitleRoute(track({ delivery }), hls)
    expect(
      ['none', 'burned', 'ass', 'image', 'live-text', 'vtt-track'].includes(route),
    ).toBeTruthy()
  }
})
