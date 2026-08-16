/// What a client is allowed to pretend it cannot do, and the codec strings it
/// uses to ask an exact question.
///
/// The mask is a testing affordance that changes real behaviour — the profile
/// sent to the hub follows it — so the rule that matters most is that it can
/// only ever subtract.

import { describe, expect, test } from 'vitest'

import type { CapabilityProfile } from '../src/api/generated/model/capabilityProfile.ts'
import { applyMask, maskSummary, rfc6381 } from '../src/domain/capability-mask.ts'

const probed = (): CapabilityProfile => ({
  containers: ['mp4', 'webm'],
  video: [{ codec: 'h264' }, { codec: 'hevc' }],
  audio: ['aac', 'flac'],
  max_audio_channels: 0,
  hdr: true,
  ass_render: true,
  graphics_overlay: true,
  vtt_render: true,
  target_duration: { mode: 'ignore' },
})

describe('what a mask changes', () => {
  test('an inert mask says so', () => {
    // A mask left on can never be mistaken for a bug — which is the whole
    // trap this affordance would otherwise set.
    expect(maskSummary({})).toEqual([])
  })

  test('and every kind of subtraction is one token', () => {
    expect(maskSummary({ video: ['hevc'], audio: ['flac'] })).toEqual(['−hevc', '−flac'])
    expect(maskSummary({ max_height: 1080 })).toEqual(['≤1080p'])
    expect(maskSummary({ max_audio_channels: 2 })).toEqual(['2ch'])
    expect(maskSummary({ hdr: false })).toEqual(['hdr=false'])
    // Including one set the other way: a declaration can go either way, and
    // saying so is the point.
    expect(maskSummary({ ass_render: true })).toEqual(['ass_render=true'])
    expect(maskSummary({ target_duration: { mode: 'short', max_secs: 6 } })).toEqual([
      'target=short:6s',
    ])
    expect(maskSummary({ target_duration: { mode: 'accurate' } })).toEqual(['target=accurate'])
  })

  test('an empty list of drops is not a change', () => {
    expect(maskSummary({ video: [] })).toEqual([])
  })
})

describe('applying it', () => {
  test('drops the codecs it names and leaves the rest', () => {
    const out = applyMask(probed(), { video: ['hevc'], audio: ['flac'], containers: ['webm'] })
    expect(out.video).toEqual([{ codec: 'h264' }])
    expect(out.audio).toEqual(['aac'])
    expect(out.containers).toEqual(['mp4'])
  })

  test('it can empty a list, which is a real claim', () => {
    // The probe must never send an empty list; a mask doing it deliberately
    // is how the transcode path is reached.
    expect(applyMask(probed(), { video: ['h264', 'hevc'] }).video).toEqual([])
  })

  test('ceilings tighten and never loosen', () => {
    const capped = applyMask({ ...probed(), max_height: 720 }, { max_height: 1080 })
    expect(capped.max_height).toBe(720)
    expect(applyMask(probed(), { max_height: 1080 }).max_height).toBe(1080)
  })

  test('declarations go either way', () => {
    expect(applyMask(probed(), { hdr: false }).hdr).toBe(false)
    expect(applyMask({ ...probed(), hdr: false }, { hdr: true }).hdr).toBe(true)
  })

  test('and an inert mask changes nothing at all', () => {
    expect(applyMask(probed(), {})).toEqual(probed())
  })

  test('the profile it was given is left alone', () => {
    // The debug panel compares the masked answer against the baseline, so the
    // baseline has to survive being masked.
    const before = probed()
    applyMask(before, { video: ['hevc'] })
    expect(before.video).toHaveLength(2)
  })
})

describe('asking about one exact stream', () => {
  test('h264 becomes an avc1 string at its own profile and level', () => {
    expect(rfc6381({ codec: 'h264', profile: 'high', level: '4.1' })).toBe(
      'video/mp4; codecs="avc1.640029"',
    )
    expect(rfc6381({ codec: 'h264', profile: 'main', level: '3.0' })).toBe(
      'video/mp4; codecs="avc1.4D401E"',
    )
  })

  test('hevc becomes an hvc1 string, at three times the level', () => {
    expect(rfc6381({ codec: 'hevc', profile: 'main', level: '5.1' })).toBe(
      'video/mp4; codecs="hvc1.1.6.L153.B0"',
    )
    expect(rfc6381({ codec: 'hevc', profile: 'main-10', level: '5.1' })).toBe(
      'video/mp4; codecs="hvc1.2.4.L153.B0"',
    )
  })

  test('and nothing is claimed about a stream that did not say', () => {
    // Metadata predating the probe extension: the generic family floor
    // applies, and a made-up precise cap would admit a copy the browser
    // cannot play.
    expect(rfc6381({ codec: 'h264', profile: null, level: '4.1' })).toBeUndefined()
    expect(rfc6381({ codec: 'h264', profile: 'high', level: null })).toBeUndefined()
    expect(rfc6381({ codec: 'h264', profile: 'nonsense', level: '4.1' })).toBeUndefined()
    expect(rfc6381({ codec: 'h264', profile: 'high', level: 'nonsense' })).toBeUndefined()
    expect(rfc6381({ codec: 'hevc', profile: 'main', level: '0' })).toBeUndefined()
    expect(rfc6381({ codec: 'vp9', profile: 'profile-0', level: '4.1' })).toBeUndefined()
  })
})
