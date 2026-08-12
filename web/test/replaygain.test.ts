import { strict as assert } from 'node:assert'
import { test } from 'node:test'
import { replayGainFactor } from '../src/replaygain.ts'
import type { Item } from '../src/api.ts'

/// Only the loudness fields matter here.
function track(rg: Partial<NonNullable<Item['replay_gain']>> | null): Item {
  return {
    id: 't',
    kind: 'track',
    title: 't',
    year: null,
    season: null,
    episode: null,
    sources: 1,
    played: false,
    play_count: 0,
    resume_position_ms: null,
    replay_gain: rg as Item['replay_gain'],
  }
}

test('album gain levels a record, track gain levels a track', () => {
  // -6 dB album, -12 dB track: the same file wants a different factor
  // depending on why it is playing.
  const t = track({ album_gain_db: -6, track_gain_db: -12 })
  const asAlbum = replayGainFactor(t, 'album')
  const asTrack = replayGainFactor(t, 'track')
  assert.ok(asAlbum > asTrack, `album ${asAlbum} should be louder than track ${asTrack}`)
  // 10^(-6/20) and 10^(-12/20)
  assert.ok(Math.abs(asAlbum - 0.5012) < 0.001)
  assert.ok(Math.abs(asTrack - 0.2512) < 0.001)
})

test('each mode falls back to the other measurement rather than to nothing', () => {
  // An album rip missing its album tags still levels better by track.
  assert.ok(replayGainFactor(track({ track_gain_db: -6 }), 'album') < 1)
  assert.ok(replayGainFactor(track({ album_gain_db: -6 }), 'track') < 1)
})

test('a positive gain is capped so the peak cannot clip', () => {
  // +6 dB would double a signal that already reaches 0.9 of full scale.
  const capped = replayGainFactor(track({ track_gain_db: 6, track_peak: 0.9 }), 'track')
  assert.ok(Math.abs(capped - 1 / 0.9) < 0.0001, `expected 1/peak, got ${capped}`)
  assert.ok(capped * 0.9 <= 1.0000001, 'the peak must land at or under full scale')
})

test('an untagged file plays unlevelled rather than silent', () => {
  assert.equal(replayGainFactor(track(null), 'album'), 1)
  assert.equal(replayGainFactor(track({}), 'track'), 1)
  assert.equal(replayGainFactor(undefined, 'album'), 1)
})
