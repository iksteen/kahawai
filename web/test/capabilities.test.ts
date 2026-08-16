/// The browser probe. Everything it decides changes what the hub sends, so
/// the two things checked here are the two that would be invisible: that a
/// mask is applied at all, and that a browser answering "no" to everything
/// still asks for something it can play.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

const { buildProfile, forgetProbe, loadMask, probedProfile, saveMask } =
  await import('../src/api/capabilities.ts')

/// A browser that says yes to what it is told to.
function browser({
  says = true,
  mediaSource = true,
  hasIsTypeSupported = true,
  agent = 'Chrome',
} = {}) {
  forgetProbe()
  vi.stubGlobal('navigator', { userAgent: agent })
  if (mediaSource) {
    vi.stubGlobal('MediaSource', hasIsTypeSupported ? { isTypeSupported: () => says } : {})
  } else {
    vi.stubGlobal('MediaSource', undefined)
  }
  vi.spyOn(document, 'createElement').mockReturnValue({
    canPlayType: () => (says ? 'probably' : ''),
  } as never)
}

beforeEach(() => localStorage.clear())
afterEach(() => {
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
  forgetProbe()
  localStorage.clear()
})

describe('what the browser says it can play', () => {
  test('is asked of MediaSource first, and of the video element after', () => {
    browser({ says: true })
    expect(probedProfile().video?.map((v) => v.codec)).toEqual(['h264', 'hevc', 'vp9', 'av1'])
  })

  test('a browser with MediaSource but no isTypeSupported does not throw', () => {
    // Calling it unconditionally turns every item page into "Could not load
    // this item", because the probe runs inside the query.
    browser({ says: true, hasIsTypeSupported: false })
    expect(() => probedProfile()).not.toThrow()
    expect(probedProfile().video?.length).toBeGreaterThan(0)
  })

  test('and one that can play nothing still asks for something', () => {
    // An empty list would transcode everything, which is not what "I could
    // not probe" means.
    browser({ says: false })
    const probed = probedProfile()
    expect(probed.video).toEqual([{ codec: 'h264' }])
    expect(probed.audio).toEqual(['aac', 'mp3'])
    expect(probed.containers).toEqual(['mp4'])
  })

  test('Firefox asks the server to tone-map, because it will not', () => {
    // No feature probe exposes "I tone-map": this one is behavioural.
    browser({ agent: 'Mozilla/5.0 Firefox/130.0' })
    expect(probedProfile().hdr).toBe(false)
    browser({ agent: 'Mozilla/5.0 Chrome/130.0' })
    expect(probedProfile().hdr).toBe(true)
  })
})

describe('the profile that is sent', () => {
  test('applies the mask, and applies it last', () => {
    // A source-aware precise cap must not smuggle back a family the mask has
    // just dropped.
    browser({ says: true })
    saveMask({ video: ['hevc'] })
    const profile = buildProfile(null, [{ codec: 'hevc', profile: 'main', level: '5.1' }])
    expect(profile.video?.some((v) => v.codec === 'hevc')).toBe(false)
    expect(profile.video?.some((v) => v.codec === 'h264')).toBe(true)
  })

  test('carries a bandwidth ceiling only when there is one', () => {
    browser({ says: true })
    expect(buildProfile(4000).max_bandwidth_kbps).toBe(4000)
    expect(buildProfile(null).max_bandwidth_kbps).toBeUndefined()
    expect(buildProfile(0).max_bandwidth_kbps).toBeUndefined()
  })

  test('and asks the exact question for a stream that said what it is', () => {
    browser({ says: true })
    const profile = buildProfile(null, [{ codec: 'h264', profile: 'high', level: '4.1' }])
    expect(profile.video).toContainEqual({ codec: 'h264', max_profile: 'high', max_level: '4.1' })
    // Alongside the family floor, not instead of it: the hub admits a stream
    // when any cap for its codec does.
    expect(profile.video).toContainEqual({ codec: 'h264' })
  })

  test('a stream the browser cannot play precisely is not claimed', () => {
    browser({ says: false })
    const profile = buildProfile(null, [{ codec: 'h264', profile: 'high', level: '4.1' }])
    expect(profile.video).toEqual([{ codec: 'h264' }])
  })
})

describe('the mask on disk', () => {
  test('an inert one is removed rather than stored', () => {
    saveMask({ video: ['hevc'] })
    expect(loadMask()).toEqual({ video: ['hevc'] })
    saveMask({})
    expect(loadMask()).toEqual({})
    expect(localStorage.getItem('kahawai.capmask')).toBeNull()
  })

  test('and unreadable storage is no mask rather than a crash', () => {
    localStorage.setItem('kahawai.capmask', 'not json')
    expect(loadMask()).toEqual({})
  })
})
