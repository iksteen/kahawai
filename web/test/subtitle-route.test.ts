import assert from 'node:assert/strict'
import test from 'node:test'
import { subtitleRoute } from '../src/subtitle-route.ts'
import type { Subtitle } from '../src/api.ts'

const track = (p: Partial<Subtitle>) =>
  ({ delivery: 'text', format: 'srt', origin: 'embedded', ...p }) as Subtitle
const hls = { isHls: true, vttFallback: false }
const file = { isHls: false, vttFallback: false }

test('nothing chosen renders nothing', () => {
  assert.equal(subtitleRoute(undefined, hls), 'none')
})

test('burned is not the same answer as none', () => {
  // One is already in the picture and one cannot be served at all; a route that
  // folded them together would draw an empty overlay over burnt-in text.
  assert.equal(subtitleRoute(track({ delivery: 'burn' }), hls), 'burned')
  assert.equal(subtitleRoute(track({ delivery: 'none' }), hls), 'none')
})

test('the delivery the hub computed decides the renderer', () => {
  assert.equal(subtitleRoute(track({ delivery: 'ass', format: 'ass' }), hls), 'ass')
  assert.equal(subtitleRoute(track({ delivery: 'overlay', format: 'pgs' }), hls), 'image')
})

test('an embedded text track on a live playlist is fed from the tap', () => {
  assert.equal(subtitleRoute(track({ origin: 'embedded' }), hls), 'live-text')
})

test('a direct file has no tap, so the same track uses the vtt element', () => {
  // The case that pins `isHls`: identical track, different session kind.
  assert.equal(subtitleRoute(track({ origin: 'embedded' }), file), 'vtt-track')
})

test('a sidecar has no tap either', () => {
  assert.equal(subtitleRoute(track({ origin: 'sidecar' }), hls), 'vtt-track')
  assert.equal(subtitleRoute(track({ origin: 'downloaded' }), hls), 'vtt-track')
})

test('an ASS-format track this client declined goes to the flattened vtt', () => {
  // Delivery says text — the client did not take the ass route — but the
  // pipeline taps it as .ass and never writes the .jsonl the live path reads.
  // Keying on delivery alone would send it to a tap that cannot exist.
  for (const format of ['ass', 'ssa']) {
    assert.equal(subtitleRoute(track({ format, origin: 'embedded' }), hls), 'vtt-track', format)
  }
})

test('once the tap has yielded nothing, the same track takes the vtt element', () => {
  assert.equal(
    subtitleRoute(track({ origin: 'embedded' }), { isHls: true, vttFallback: true }),
    'vtt-track',
  )
})

test('every delivery has an answer', () => {
  for (const delivery of ['text', 'ass', 'overlay', 'burn', 'none'] as const) {
    const route = subtitleRoute(track({ delivery }), hls)
    assert.ok(
      ['none', 'burned', 'ass', 'image', 'live-text', 'vtt-track'].includes(route),
      `${delivery} -> ${route}`,
    )
  }
})
