import assert from 'node:assert/strict'
import test from 'node:test'
import { initialTracks, tracks, type TrackEvent } from '../src/player-tracks.ts'
import type { StreamVerdict, Subtitle } from '../src/api.ts'

const run = (...events: TrackEvent[]) => events.reduce(tracks, initialTracks(null))
const sub = (id: number) =>
  ({ id, delivery: 'text', format: 'srt', origin: 'embedded' }) as Subtitle

test('the opening choice does not override one the viewer made', () => {
  // The lists and the remembered pick arrive on separate round trips; if the
  // viewer picked something while they were in flight, that wins.
  const chosen = run({ type: 'subtitle-chosen', key: '9' })
  const after = tracks(chosen, { type: 'subtitle-chosen', key: '4', onlyIfUnset: true })
  assert.equal(after.subKey, '9')
  // With nothing chosen it applies.
  assert.equal(tracks(run(), { type: 'subtitle-chosen', key: '4', onlyIfUnset: true }).subKey, '4')
})

test('changing track forgets what the last tap concluded', () => {
  // Otherwise one track falling back to the flattened .vtt puts every later
  // track on that path for the rest of the session.
  const fell = run({ type: 'subtitle-chosen', key: '1' }, { type: 'tap-empty' })
  assert.equal(fell.vttFallback, true)
  assert.equal(tracks(fell, { type: 'subtitle-chosen', key: '2' }).vttFallback, false)
})

test('turning subtitles off is a choice, not an absence', () => {
  const off = run({ type: 'subtitle-chosen', key: '3' }, { type: 'subtitle-chosen', key: '' })
  assert.equal(off.subKey, '')
  // And `onlyIfUnset` may then set one again — off is unset for that purpose.
  assert.equal(tracks(off, { type: 'subtitle-chosen', key: '7', onlyIfUnset: true }).subKey, '7')
})

test('a run moving bumps the epoch and touches nothing else', () => {
  const before = run(
    { type: 'subtitle-chosen', key: '5' },
    { type: 'tracks-chosen', audio: 2, video: 1 },
  )
  const after = tracks(before, { type: 'run-moved' })
  assert.equal(after.epoch, before.epoch + 1)
  assert.deepEqual(
    { ...after, epoch: 0 },
    { ...before, epoch: 0 },
    'a restart must not disturb the selection',
  )
})

test('the verdict and the selection move independently', () => {
  const verdict: StreamVerdict = { video: 'h264', audio: 'aac' } as StreamVerdict
  const s = run(
    { type: 'tracks-chosen', audio: 3, video: 0 },
    { type: 'streams-known', streams: verdict },
  )
  assert.equal(s.audio, 3)
  assert.equal(s.streams, verdict)
})

test('lists arriving do not disturb what is chosen', () => {
  const s = run(
    { type: 'tracks-chosen', audio: 2, video: 1 },
    { type: 'subtitle-chosen', key: '8' },
    { type: 'subtitles-arrived', subs: [sub(8)] },
    { type: 'lists-arrived', audioList: [{ codec: 'aac', channels: 2 }], videoList: [] },
  )
  assert.equal(s.audio, 2)
  assert.equal(s.subKey, '8')
  assert.equal(s.subs.length, 1)
})

test('the lists land in the pickers they belong to', () => {
  // Weaker than it looks, and worth being honest about: the two lists have
  // different shapes, so swapping them is a type error and `tsc -b` catches
  // it before this does. What this pins is dropping one of them.
  const s = run({
    type: 'lists-arrived',
    audioList: [{ codec: 'eac3', channels: 6, language: 'jpn' }],
    videoList: [{ codec: 'hevc', width: 1920, height: 1080 }],
  })
  assert.deepEqual(s.audioList, [{ codec: 'eac3', channels: 6, language: 'jpn' }])
  assert.deepEqual(s.videoList, [{ codec: 'hevc', width: 1920, height: 1080 }])
})

test('the resolved opening track outranks the slot it lands in', () => {
  // Whose answer this is, exactly: the CLIENT's, from `resolveTracks` over the
  // prefs and the stream list. The hub reports no opened-audio index at all —
  // `Session` and `StreamVerdict` carry none — so a hub that clamps the index
  // it was given is not observable here, and this pins only that the resolved
  // choice replaces whatever the selector was showing.
  const s = run({ type: 'tracks-chosen', audio: 1, video: 0 }, { type: 'audio-known', audio: 2 })
  assert.equal(s.audio, 2)
  assert.equal(s.video, 0)
})
