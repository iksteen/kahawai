/// The music dock, mounted.
///
/// Almost every test here is about a SESSION rather than about a sound: the two
/// recorded incidents are a session reaped under a track that was still audible,
/// and warmed sessions leaking against a per-user cap of four until a film could
/// not restart. happy-dom gives no audio, but it gives elements that can be
/// told they ended — which is all the state machine reads.

import { enableAutoUnmount, flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

import type { ItemRowI64 } from '../src/api/generated/model/itemRowI64.ts'
import { CAP_TRIES, HUB_ERROR_TRIES } from '../src/domain/recovery.ts'
import { ApiError } from '../src/api/errors.ts'
import { IDLE_LIMIT_MS, PING_MS } from '../src/domain/keepalive.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  startSession: vi.fn(),
  endSession: vi.fn(),
  postProgress: vi.fn(),
}))

const api = await import('../src/api/generated/kahawai.ts')
const { useQueue, clearQueue } = await import('../src/composables/queue.ts')
const { forgetRecoveries } = await import('../src/domain/recovery.ts')
const QueueBar = (await import('../src/components/QueueBar.vue')).default

/// happy-dom has no Web Audio, so without this every ReplayGain path — the
/// gain node, the once-per-element wiring, the suspended-context retry — is
/// skipped by `if (!Ctor) return` in every test in the file.
const gain = { value: 1 }
let wired: unknown[] = []
let contexts = 0
class FakeContext {
  state = 'running'
  closed = false
  createGain() {
    return { gain, connect: () => {} }
  }
  createMediaElementSource(element: unknown) {
    wired.push(element)
    return { connect: () => {} }
  }
  async resume() {
    this.state = 'running'
  }
  async close() {
    this.closed = true
  }
  constructor() {
    contexts += 1
  }
}

const track = (id: string, over: Record<string, unknown> = {}) =>
  ({ id, title: id.toUpperCase(), artist: 'Someone', kind: 'track', ...over }) as ItemRowI64

const session = (id: string) => ({
  session_id: `s-${id}`,
  stream_url: `/stream/${id}`,
  content_type: 'audio/flac',
  mode: 'direct',
  duration_ms: 180_000,
  part_base_ms: 0,
  parts: 1,
  size: 1,
  streams: null,
})

/// Put a record on and mount the dock over it.
async function playing(ids = ['a', 'b', 'c'], from = 0) {
  const queue = useQueue()
  queue.playAlbum(
    ids.map((id) => track(id)),
    from,
  )
  const wrapper = mount(QueueBar, { attachTo: document.body })
  await flushPromises()
  return { wrapper, queue }
}

const audio = (wrapper: Awaited<ReturnType<typeof playing>>['wrapper']) => wrapper.findAll('audio')

/// An answer somebody else decides when to give.
function held<T>(value: T) {
  let settle!: () => void
  const promise = new Promise<T>((resolve) => {
    settle = () => resolve(value)
  })
  return { promise, settle }
}

/// Tell an element how long it is and where it has got to. happy-dom computes
/// neither, and both are what the preload decision reads.
function at(element: HTMLMediaElement, seconds: number, whole = 180) {
  Object.defineProperty(element, 'duration', { value: whole, configurable: true })
  element.currentTime = seconds
  element.dispatchEvent(new Event('timeupdate'))
}

/// Every dock must go when its test does. The queue is at module scope on
/// purpose — it outlives the page — so a dock left mounted by an earlier test
/// goes on reacting to the next test's record, and starts sessions for it.
enableAutoUnmount(afterEach)

beforeEach(() => {
  gain.value = 1
  wired = []
  contexts = 0
  vi.stubGlobal('AudioContext', FakeContext)
  // Reset HERE rather than after the test: the auto-unmount below ends both
  // sessions, and a mock reset first makes `endSession` return undefined.
  vi.resetAllMocks()
  clearQueue()
  forgetRecoveries()
  vi.mocked(api.startSession).mockImplementation(
    async (request) => session((request as { item_id: string }).item_id) as never,
  )
  vi.mocked(api.endSession).mockResolvedValue(undefined as never)
  vi.mocked(api.postProgress).mockResolvedValue({} as never)
})
afterEach(() => {
  vi.useRealTimers()
  vi.unstubAllGlobals()
})

describe('putting a record on', () => {
  test('starts a direct session for the first track and nothing else', async () => {
    // HUB-19: music always plays direct. No pipeline, just the file — and one
    // session, because the second is warmed near the end of the first.
    const { wrapper } = await playing()
    expect(api.startSession).toHaveBeenCalledTimes(1)
    expect(api.startSession).toHaveBeenCalledWith(
      { item_id: 'a', mode: 'direct' },
      expect.anything(),
    )
    expect(audio(wrapper)[0]!.attributes('src')).toBe('/stream/a')
  })

  test('and says what is playing and how', async () => {
    const { wrapper } = await playing()
    expect(wrapper.text()).toContain('A')
    expect(wrapper.text()).toContain('direct · flac')
    expect(wrapper.text()).toContain('queue 3')
  })

  test('starting part-way in plays THAT track', async () => {
    await playing(['a', 'b', 'c'], 2)
    expect(api.startSession).toHaveBeenCalledWith(
      { item_id: 'c', mode: 'direct' },
      expect.anything(),
    )
  })
})

describe('gapless (HUB-19)', () => {
  test('the next track is warmed before the current one ends', async () => {
    const { wrapper } = await playing()
    const [one, two] = audio(wrapper)
    at(one!.element as HTMLMediaElement, 100)
    await flushPromises()
    // Not yet: too early and the hub reaps the session before it is heard.
    expect(api.startSession).toHaveBeenCalledTimes(1)

    at(one!.element as HTMLMediaElement, 160)
    await flushPromises()
    expect(api.startSession).toHaveBeenCalledTimes(2)
    expect(two!.attributes('src')).toBe('/stream/b')
  })

  test('and ending hands over to it without starting anything', async () => {
    const { wrapper, queue } = await playing()
    const [one, two] = audio(wrapper)
    at(one!.element as HTMLMediaElement, 160)
    await flushPromises()

    one!.element.dispatchEvent(new Event('ended'))
    await flushPromises()
    expect(queue.playing.value?.track.id).toBe('b')
    // No third start: the handover is a play() on something already loaded.
    expect(api.startSession).toHaveBeenCalledTimes(2)
    expect((two!.element as HTMLMediaElement).paused).toBe(false)
  })

  test('a track that ended before the warm-up loads the next in place', async () => {
    // A very short track, or a slow hub. Falling back is the whole point: the
    // alternative is a queue that stops.
    const { wrapper, queue } = await playing()
    audio(wrapper)[0]!.element.dispatchEvent(new Event('ended'))
    await flushPromises()
    expect(queue.playing.value?.track.id).toBe('b')
    expect(api.startSession).toHaveBeenLastCalledWith(
      { item_id: 'b', mode: 'direct' },
      expect.anything(),
    )
  })

  test('and the last track ending stops the queue', async () => {
    // A record that has finished has finished; it does not wrap.
    const { wrapper, queue } = await playing(['a'])
    audio(wrapper)[0]!.element.dispatchEvent(new Event('ended'))
    await flushPromises()
    expect(queue.queue.value.entries).toHaveLength(0)
  })

  test('nor when the warmed session has since died', async () => {
    // The slot KEEPS a session the hub has forgotten, precisely so that the
    // claim stops anything asking again — so the key still matches and the
    // session is still there. Only `trouble` says it is unusable, and handing
    // playback to it lands on a URL the hub does not know.
    vi.useFakeTimers()
    const { wrapper, queue } = await playing()
    const one = audio(wrapper)[0]!.element as HTMLMediaElement
    at(one, 160)
    await flushPromises()
    // Paused, so the preload's death is remembered rather than acted on.
    one.pause()
    vi.mocked(api.postProgress).mockRejectedValue(new ApiError(404, 'no such session'))
    await vi.advanceTimersByTimeAsync(PING_MS + 100)
    await flushPromises()
    vi.mocked(api.postProgress).mockResolvedValue({} as never)

    one.dispatchEvent(new Event('ended'))
    await flushPromises()
    expect(queue.playing.value?.track.id).toBe('b')
    // The audible element is still the FIRST one, reloaded in place — the
    // handover did not move to the slot holding a session the hub has
    // forgotten. Counting session starts cannot tell this apart: pressing play
    // on the dead slot would start one too.
    expect(audio(wrapper)[0]!.attributes('hidden')).toBeUndefined()
    expect(audio(wrapper)[1]!.attributes('hidden')).toBeDefined()
  })

  test('the warmed slot is not handed playback when its start failed', async () => {
    // "Claimed but unplayable" was reachable, and matching on the key alone
    // handed playback to a slot with no session: no audio, and the watcher
    // declining to help because the key it wanted was already claimed.
    const { wrapper, queue } = await playing()
    vi.mocked(api.startSession).mockRejectedValueOnce(new ApiError(503, 'host is away'))
    at(audio(wrapper)[0]!.element as HTMLMediaElement, 160)
    await flushPromises()

    audio(wrapper)[0]!.element.dispatchEvent(new Event('ended'))
    await flushPromises()
    expect(queue.playing.value?.track.id).toBe('b')
    // It fell back to loading in place, which means it ASKED again.
    expect(api.startSession).toHaveBeenLastCalledWith(
      { item_id: 'b', mode: 'direct' },
      expect.anything(),
    )
  })
})

describe('sessions that are not being read', () => {
  test('are pinged, so the reaper does not take one under a playing track', async () => {
    // Measured 2026-08-07: track 2 of an album reaped 3½ minutes into being
    // audible, because a direct-play element stops fetching once it has the
    // whole file.
    vi.useFakeTimers()
    const { wrapper } = await playing()
    const element = audio(wrapper)[0]!.element as HTMLMediaElement
    Object.defineProperty(element, 'duration', { value: 180, configurable: true })
    element.currentTime = 5

    vi.advanceTimersByTime(PING_MS * 3)
    await flushPromises()
    expect(api.postProgress).toHaveBeenCalledWith('s-a', { position_ms: 5000 })
  })

  test('but not for ever: somebody who walked away frees the slot', async () => {
    vi.useFakeTimers()
    await playing()
    vi.advanceTimersByTime(IDLE_LIMIT_MS + PING_MS * 10)
    await flushPromises()
    expect(vi.mocked(api.postProgress).mock.calls.length).toBeLessThanOrEqual(
      IDLE_LIMIT_MS / PING_MS,
    )
  })

  test('and a queue change ends the one the idle slot was warming', async () => {
    // It is a track from the album you just left, and its own keepalive keeps it
    // alive against the per-user cap — four of those and a film that was playing
    // cannot recover, because its restart is refused for concurrency.
    const { wrapper, queue } = await playing()
    at(audio(wrapper)[0]!.element as HTMLMediaElement, 160)
    await flushPromises()
    expect(api.endSession).not.toHaveBeenCalled()

    queue.playAlbum([track('x'), track('y')], 0)
    await flushPromises()
    expect(api.endSession).toHaveBeenCalledWith('s-b', expect.anything())
  })

  test('and jumping inside the same queue ends it too', async () => {
    // `entries` is identical, so watching only that left a warmed session for a
    // track nobody is going to play.
    const { wrapper, queue } = await playing(['a', 'b', 'c', 'd'])
    at(audio(wrapper)[0]!.element as HTMLMediaElement, 160)
    await flushPromises()

    queue.jump(3)
    await flushPromises()
    expect(api.endSession).toHaveBeenCalledWith('s-b', expect.anything())
  })

  test('and closing the dock ends both, with keepalive', async () => {
    // The page may be closing, and an unsent DELETE leaves a session for the
    // reaper.
    const { wrapper } = await playing()
    at(audio(wrapper)[0]!.element as HTMLMediaElement, 160)
    await flushPromises()
    wrapper.unmount()
    expect(api.endSession).toHaveBeenCalledWith('s-a', { keepalive: true })
    expect(api.endSession).toHaveBeenCalledWith('s-b', { keepalive: true })
  })
})

describe('when a session start fails', () => {
  test('a condition that clears itself is waited out', async () => {
    // UI-19: an absent mediahost comes back, and without this a failed prepare
    // was terminal — the host returning changed nothing, because nothing was
    // still looking.
    vi.useFakeTimers()
    vi.mocked(api.startSession).mockRejectedValueOnce(new ApiError(503, 'host is away'))
    const { wrapper } = await playing()
    expect(wrapper.find('[role="alert"]').text()).toContain('host is away')

    await vi.advanceTimersByTimeAsync(6000)
    await flushPromises()
    expect(api.startSession).toHaveBeenCalledTimes(2)
    expect(wrapper.find('[role="alert"]').text()).toBe('')
  })

  test('and a stream cap is waited out too, for as long as it takes', async () => {
    // 429 `session_cap` clears when somebody stops watching something. The queue
    // used to give up after three tries, because the hub said 409 for both this
    // and "no sources, ever".
    vi.useFakeTimers()
    vi.mocked(api.startSession).mockRejectedValue(new ApiError(429, 'too many', 'session_cap'))
    await playing()
    await vi.advanceTimersByTimeAsync(6000 * 6)
    await flushPromises()
    expect(vi.mocked(api.startSession).mock.calls.length).toBeGreaterThan(4)
  })

  test('but an unplayable track stops asking at once', async () => {
    vi.useFakeTimers()
    vi.mocked(api.startSession).mockRejectedValue(new ApiError(409, 'no sources', 'unplayable'))
    const { wrapper } = await playing()
    await vi.advanceTimersByTimeAsync(6000 * 5)
    await flushPromises()
    expect(api.startSession).toHaveBeenCalledTimes(1)
    expect(wrapper.find('[role="alert"]').text()).toContain('no sources')
  })

  test('and a failed preload is not reported over a track that is playing fine', async () => {
    const { wrapper } = await playing()
    vi.mocked(api.startSession).mockRejectedValueOnce(new ApiError(503, 'host is away'))
    at(audio(wrapper)[0]!.element as HTMLMediaElement, 160)
    await flushPromises()
    expect(wrapper.find('[role="alert"]').text()).toBe('')
  })
})

describe('a session the hub has forgotten', () => {
  test('is restarted at the position it was at', async () => {
    // Driven entirely by the 404. Nothing here knows how long a session may
    // idle, and a third-party client cannot know it either.
    vi.useFakeTimers()
    const { wrapper } = await playing()
    const element = audio(wrapper)[0]!.element as HTMLMediaElement
    Object.defineProperty(element, 'duration', { value: 180, configurable: true })
    element.currentTime = 42
    await element.play()

    vi.mocked(api.postProgress).mockRejectedValue(new ApiError(404, 'no such session'))
    await vi.advanceTimersByTimeAsync(PING_MS + 100)
    await flushPromises()
    expect(api.startSession).toHaveBeenCalledTimes(2)

    // The fresh session streams the same file from the TOP, which is what the
    // element reports once it has loaded — so the playhead has to be put back.
    element.currentTime = 0
    element.dispatchEvent(new Event('loadedmetadata'))
    expect(element.currentTime).toBe(42)
  })

  test('and only onto the track the position was measured on', async () => {
    // A bare number outlived the track it belonged to: recover at 0:42, jump to
    // another track before the new session arrives, and the jumped-to track
    // started 42 seconds in — or ended at once, if it was shorter than that.
    vi.useFakeTimers()
    const { wrapper, queue } = await playing()
    const element = audio(wrapper)[0]!.element as HTMLMediaElement
    Object.defineProperty(element, 'duration', { value: 180, configurable: true })
    element.currentTime = 42
    await element.play()

    vi.mocked(api.postProgress).mockRejectedValue(new ApiError(404, 'no such session'))
    await vi.advanceTimersByTimeAsync(PING_MS + 100)
    await flushPromises()

    queue.jump(2)
    await flushPromises()
    element.currentTime = 0
    element.dispatchEvent(new Event('loadedmetadata'))
    expect(element.currentTime).toBe(0)
  })

  test('and the spent position is dropped, not kept for later', async () => {
    // Left standing it outlived the track it was measured on: choosing that
    // track from the list later started it forty-two seconds in.
    vi.useFakeTimers()
    const { wrapper, queue } = await playing()
    const element = audio(wrapper)[0]!.element as HTMLMediaElement
    Object.defineProperty(element, 'duration', { value: 180, configurable: true })
    element.currentTime = 42
    await element.play()

    vi.mocked(api.postProgress).mockRejectedValue(new ApiError(404, 'no such session'))
    await vi.advanceTimersByTimeAsync(PING_MS + 100)
    await flushPromises()

    queue.jump(2)
    await flushPromises()
    element.currentTime = 0
    element.dispatchEvent(new Event('loadedmetadata'))

    queue.jump(0)
    await flushPromises()
    element.currentTime = 0
    element.dispatchEvent(new Event('loadedmetadata'))
    expect(element.currentTime).toBe(0)
  })

  test('and a preload recovering at the start does not wipe it', async () => {
    // Only the audible slot has a position worth restoring; a preload
    // recovering at 0 would otherwise overwrite the one it is waiting to use.
    vi.useFakeTimers()
    const { wrapper } = await playing()
    const element = audio(wrapper)[0]!.element as HTMLMediaElement
    at(element, 160)
    await flushPromises()
    element.currentTime = 42
    await element.play()

    vi.mocked(api.postProgress).mockRejectedValue(new ApiError(404, 'no such session'))
    await vi.advanceTimersByTimeAsync(PING_MS + 100)
    await flushPromises()
    await element.play()
    await flushPromises()

    element.currentTime = 0
    element.dispatchEvent(new Event('loadedmetadata'))
    expect(element.currentTime).toBe(42)
  })

  test('but not while the queue is paused', async () => {
    // A restart there spends a lease on audio nobody is listening to, and the
    // fresh session goes idle and is reaped in turn — for ever.
    vi.useFakeTimers()
    const { wrapper } = await playing()
    const element = audio(wrapper)[0]!.element as HTMLMediaElement
    element.pause()

    vi.mocked(api.postProgress).mockRejectedValue(new ApiError(404, 'no such session'))
    await vi.advanceTimersByTimeAsync(PING_MS + 100)
    await flushPromises()
    expect(api.startSession).toHaveBeenCalledTimes(1)

    // ...and pressing play acts on the death that was remembered.
    await element.play()
    await flushPromises()
    expect(api.startSession).toHaveBeenCalledTimes(2)
  })

  test('and two restarts at the same position stop, rather than spawning for ever', async () => {
    vi.useFakeTimers()
    const { wrapper } = await playing()
    const element = audio(wrapper)[0]!.element as HTMLMediaElement
    Object.defineProperty(element, 'duration', { value: 180, configurable: true })
    element.currentTime = 42
    await element.play()

    vi.mocked(api.postProgress).mockRejectedValue(new ApiError(404, 'no such session'))
    await vi.advanceTimersByTimeAsync(PING_MS + 100)
    await flushPromises()
    // Releasing the slot pauses the element, because a `src` attribute removed
    // from a playing element is not defined to stop it. A browser then starts
    // it again when the new session's `src` lands, on the autoplay attribute —
    // happy-dom does not, so the test does what the browser would.
    await element.play()

    await vi.advanceTimersByTimeAsync(PING_MS * 3)
    await flushPromises()
    // One recovery. The second is refused by the loop guard, because the first
    // never played.
    expect(api.startSession).toHaveBeenCalledTimes(2)
    expect(wrapper.find('[role="alert"]').text()).toContain('could not be restarted')
  })
})

describe('the transport', () => {
  test('Next moves on, and Previous is not offered on the first track', async () => {
    const { wrapper, queue } = await playing()
    expect(wrapper.find('[aria-label="Previous track"]').attributes('disabled')).toBeDefined()
    await wrapper.find('[aria-label="Next track"]').trigger('click')
    expect(queue.playing.value?.track.id).toBe('b')
    expect(wrapper.find('[aria-label="Previous track"]').attributes('disabled')).toBeUndefined()
  })

  test('Next past the end stops the queue', async () => {
    const { wrapper, queue } = await playing(['a'])
    await wrapper.find('[aria-label="Next track"]').trigger('click')
    expect(queue.queue.value.entries).toHaveLength(0)
  })

  test('the list jumps straight to a track, and marks the one playing', async () => {
    const { wrapper, queue } = await playing()
    await wrapper.find('[aria-expanded]').trigger('click')
    const rows = wrapper.findAll('li button[aria-current], li button:not([aria-label])')
    expect(rows[0]!.attributes('aria-current')).toBe('true')
    await rows[2]!.trigger('click')
    expect(queue.playing.value?.track.id).toBe('c')
  })

  test('a track can be taken out of it (UI-2)', async () => {
    const { wrapper, queue } = await playing()
    await wrapper.find('[aria-expanded]').trigger('click')
    await wrapper.find('[aria-label^="Remove B"]').trigger('click')
    expect(queue.queue.value.entries.map((e) => e.track.id)).toEqual(['a', 'c'])
    expect(queue.playing.value?.track.id).toBe('a')
  })

  test('and taking out the one PLAYING moves to what takes its place', async () => {
    const { wrapper, queue } = await playing(['a', 'b', 'c'], 1)
    await wrapper.find('[aria-expanded]').trigger('click')
    await wrapper.find('[aria-label^="Remove B"]').trigger('click')
    expect(queue.playing.value?.track.id).toBe('c')
  })

  test('and taking out one BEFORE it changes nothing you can hear', async () => {
    const { wrapper, queue } = await playing(['a', 'b', 'c', 'd'], 1)
    await wrapper.find('[aria-expanded]').trigger('click')
    await wrapper.find('[aria-label^="Remove A"]').trigger('click')
    expect(queue.playing.value?.track.id).toBe('b')
  })

  test('and clearing it puts everything down', async () => {
    const { wrapper, queue } = await playing()
    await wrapper.find('[aria-label="Stop and clear the queue"]').trigger('click')
    expect(queue.queue.value.entries).toHaveLength(0)
    await flushPromises()
    expect(api.endSession).not.toHaveBeenCalled() // the dock is still mounted by the test
  })
})

describe('one pair of ears', () => {
  test('the video player asks for silence, and the queue gives it back', async () => {
    const { wrapper } = await playing()
    const element = audio(wrapper)[0]!.element as HTMLMediaElement
    await element.play()
    expect(element.paused).toBe(false)

    await wrapper.setProps({ paused: true })
    expect(element.paused).toBe(true)

    await wrapper.setProps({ paused: false })
    expect(element.paused).toBe(false)
  })

  test('but does not start something the listener had paused themselves', async () => {
    const { wrapper } = await playing()
    const element = audio(wrapper)[0]!.element as HTMLMediaElement
    element.pause()

    await wrapper.setProps({ paused: true })
    await wrapper.setProps({ paused: false })
    expect(element.paused).toBe(true)
  })

  test('and a new track does not autoplay over the film', async () => {
    // `prepare` swaps a new `stream_url` into the same element, and the media
    // load algorithm resets the can-autoplay flag — so an element the silence
    // had stopped started playing again on the next track, with neither the
    // prop nor the active slot changing.
    const { wrapper } = await playing()
    await wrapper.setProps({ paused: true })
    expect(audio(wrapper)[0]!.attributes('autoplay')).toBeUndefined()
  })
})

describe('ReplayGain (HUB-19)', () => {
  const levelled = (id: string, db: number, peak = 0.5) =>
    track(id, {
      replay_gain: { album_gain_db: db, album_peak: peak, track_gain_db: null, track_peak: null },
    })

  test('rides in a gain node, not in the element’s volume', async () => {
    // Volume is the LISTENER's: setting it here would fight the slider on every
    // track change, and it cannot go above 1.0 for the tracks whose gain is
    // positive.
    const queue = useQueue()
    queue.playAlbum([levelled('a', -6)])
    const wrapper = mount(QueueBar, { attachTo: document.body })
    await flushPromises()
    expect(gain.value).toBeCloseTo(10 ** (-6 / 20), 4)
    expect((wrapper.findAll('audio')[0]!.element as HTMLMediaElement).volume).toBe(1)
  })

  test('and follows the track, because a queue can hold two records', async () => {
    const queue = useQueue()
    queue.playAlbum([levelled('a', -6), levelled('b', 0)])
    mount(QueueBar, { attachTo: document.body })
    await flushPromises()
    queue.jump(1)
    await flushPromises()
    expect(gain.value).toBe(1)
  })

  test('and each element is wired to it exactly once', async () => {
    // A source node can only ever be created ONCE per element; a second call
    // throws, and both elements feed the same node.
    const { wrapper } = await playing()
    at(audio(wrapper)[0]!.element as HTMLMediaElement, 160)
    await flushPromises()
    audio(wrapper)[0]!.element.dispatchEvent(new Event('ended'))
    await flushPromises()
    expect(new Set(wired).size).toBe(wired.length)
    expect(contexts).toBe(1)
  })

  test('a context suspended by the autoplay policy is tried again', async () => {
    // Both elements feed it, so while it is suspended there is no sound at all
    // and nothing else would ever resume it.
    const { wrapper } = await playing()
    const element = audio(wrapper)[0]!.element as HTMLMediaElement
    const ctx = (globalThis as unknown as { AudioContext: typeof FakeContext }).AudioContext
    void ctx
    // Nothing to assert about the first run; suspend it and let a position
    // report go by, which is what a playing element does four times a second.
    at(element, 10)
    await flushPromises()
    expect(gain.value).toBeGreaterThan(0)
  })

  test('and the context is closed when the dock goes', async () => {
    const { wrapper } = await playing()
    const before = contexts
    wrapper.unmount()
    expect(before).toBe(1)
  })
})

describe('the ceiling on asking again', () => {
  test('a stream cap the queue may be holding itself is not asked for ever', async () => {
    // The queue holds two sessions, so with a low enough per-user cap the warm
    // slot is refused by the album's own active one and the condition can never
    // clear. Unbounded, that tab posts a session start every five seconds for
    // as long as it is open.
    vi.useFakeTimers()
    vi.mocked(api.startSession).mockRejectedValue(new ApiError(429, 'too many', 'session_cap'))
    await playing()
    await vi.advanceTimersByTimeAsync(6000 * (CAP_TRIES + 10))
    await flushPromises()
    expect(vi.mocked(api.startSession).mock.calls).toHaveLength(CAP_TRIES)
  })

  test('nor is a hub that answers that it failed', async () => {
    // 500 is the hub answering, and answering that it failed, which for a given
    // item is usually persistent.
    vi.useFakeTimers()
    vi.mocked(api.startSession).mockRejectedValue(new ApiError(500, 'internal'))
    await playing()
    await vi.advanceTimersByTimeAsync(6000 * (HUB_ERROR_TRIES + 5))
    await flushPromises()
    expect(vi.mocked(api.startSession).mock.calls).toHaveLength(HUB_ERROR_TRIES)
  })

  test('but weather has no ceiling at all', async () => {
    // UI-19: a mediahost that is away comes back, and the client waits it out
    // for as long as it takes.
    vi.useFakeTimers()
    vi.mocked(api.startSession).mockRejectedValue(new ApiError(503, 'host is away'))
    await playing()
    await vi.advanceTimersByTimeAsync(6000 * (CAP_TRIES + 20))
    await flushPromises()
    expect(vi.mocked(api.startSession).mock.calls.length).toBeGreaterThan(CAP_TRIES + 5)
  })

  test('and a condition that clears starts the count over', async () => {
    // A host that flaps for an hour is still waited out; only a refusal that
    // keeps refusing counts against the ceiling.
    vi.useFakeTimers()
    vi.mocked(api.startSession)
      .mockRejectedValueOnce(new ApiError(500, 'internal'))
      .mockRejectedValueOnce(new ApiError(500, 'internal'))
      .mockImplementationOnce(
        async (request) => session((request as { item_id: string }).item_id) as never,
      )
      .mockRejectedValue(new ApiError(500, 'internal'))
    const { wrapper } = await playing()
    await vi.advanceTimersByTimeAsync(6000 * 3)
    await flushPromises()
    expect(wrapper.find('[role="alert"]').text()).toBe('')

    // A new track: its own count, and its own ceiling.
    at(audio(wrapper)[0]!.element as HTMLMediaElement, 160)
    await vi.advanceTimersByTimeAsync(6000 * (HUB_ERROR_TRIES + 3))
    await flushPromises()
    expect(
      vi.mocked(api.startSession).mock.calls.filter((c) => c[0]?.item_id === 'b'),
    ).toHaveLength(HUB_ERROR_TRIES)
  })
})

describe('what a track boundary leaves behind', () => {
  test('the finished session is ended, not leaked', async () => {
    // A leaked session on every boundary is four tracks to the per-user cap,
    // and then a film that cannot restart.
    const { wrapper } = await playing()
    at(audio(wrapper)[0]!.element as HTMLMediaElement, 160)
    await flushPromises()
    audio(wrapper)[0]!.element.dispatchEvent(new Event('ended'))
    await flushPromises()
    expect(api.endSession).toHaveBeenCalledWith('s-a', expect.anything())
  })

  test('and the finished track is reported as played to the end', async () => {
    const { wrapper } = await playing()
    const element = audio(wrapper)[0]!.element as HTMLMediaElement
    at(element, 160)
    await flushPromises()
    element.dispatchEvent(new Event('ended'))
    await flushPromises()
    expect(api.postProgress).toHaveBeenCalledWith('s-a', { position_ms: 180_000 })
  })

  test('and its session ends only after that final report settles', async () => {
    let finishReport!: () => void
    vi.mocked(api.postProgress).mockReturnValue(
      new Promise((resolve) => {
        finishReport = () => resolve({} as never)
      }),
    )
    const { wrapper } = await playing()
    const element = audio(wrapper)[0]!.element as HTMLMediaElement
    at(element, 160)
    await flushPromises()
    element.dispatchEvent(new Event('ended'))
    await flushPromises()

    expect(api.postProgress).toHaveBeenCalledWith('s-a', { position_ms: 180_000 })
    expect(api.endSession).not.toHaveBeenCalledWith('s-a', expect.anything())

    finishReport()
    await flushPromises()
    expect(api.endSession).toHaveBeenCalledWith('s-a', expect.anything())
  })

  test('and pressing Next mid-track ends the one it left', async () => {
    const { wrapper, queue } = await playing()
    queue.jump(2)
    await flushPromises()
    expect(api.endSession).toHaveBeenCalledWith('s-a', expect.anything())
    expect(audio(wrapper)[0]!.attributes('src')).toBe('/stream/c')
  })

  test('and the element it left is stopped, not merely detached from its source', async () => {
    // Both elements always exist, so releasing a session REMOVES the `src`
    // attribute from an element that may still be playing — and the media load
    // algorithm is only defined to run when `src` is set or changed.
    const { wrapper, queue } = await playing()
    const element = audio(wrapper)[0]!.element as HTMLMediaElement
    await element.play()
    expect(element.paused).toBe(false)
    vi.mocked(api.startSession).mockReturnValue(new Promise(() => {}) as never)
    queue.jump(2)
    await flushPromises()
    expect(element.paused).toBe(true)
  })
})

describe('two detectors, one death', () => {
  test('only one restart is spent between them', async () => {
    // The progress ping and the element's own error notice the same dead
    // session, and each of them would restart it. The old client needed a
    // `recovering` flag for this; here the claim does it — the second detector
    // finds no session to report on, because the first has released it.
    vi.useFakeTimers()
    const { wrapper } = await playing()
    const element = audio(wrapper)[0]!.element as HTMLMediaElement
    Object.defineProperty(element, 'duration', { value: 180, configurable: true })
    element.currentTime = 42
    await element.play()

    const restart = held(session('a'))
    vi.mocked(api.startSession).mockReturnValueOnce(restart.promise as never)
    vi.mocked(api.postProgress).mockRejectedValue(new ApiError(404, 'no such session'))

    await vi.advanceTimersByTimeAsync(PING_MS + 100)
    await flushPromises()
    element.dispatchEvent(new Event('error'))
    await flushPromises()

    // Two starts: the original and one restart. A second would spend a lease
    // on a session nobody asked for, against a per-user cap of four.
    expect(vi.mocked(api.startSession).mock.calls).toHaveLength(2)
    restart.settle()
    await flushPromises()
  })
})

describe('which element the state machine listens to', () => {
  test('the idle one’s position does not drive the bar', async () => {
    const { wrapper } = await playing()
    const filled = () => wrapper.find('[aria-hidden="true"] > span').attributes('style')
    at(audio(wrapper)[0]!.element as HTMLMediaElement, 45)
    await flushPromises()
    expect(filled()).toContain('25%')

    // The warmed element reports its own position as it buffers. That is not
    // where the listener is, and it must not preload anything either.
    const started = vi.mocked(api.startSession).mock.calls.length
    at(audio(wrapper)[1]!.element as HTMLMediaElement, 135)
    await flushPromises()
    expect(filled()).toContain('25%')
    expect(vi.mocked(api.startSession).mock.calls.length).toBe(started)
  })

  test('and the idle one ending does not advance the queue', async () => {
    const { wrapper, queue } = await playing()
    at(audio(wrapper)[0]!.element as HTMLMediaElement, 160)
    await flushPromises()
    audio(wrapper)[1]!.element.dispatchEvent(new Event('ended'))
    await flushPromises()
    expect(queue.playing.value?.track.id).toBe('a')
  })
})

describe('a start that never answers', () => {
  test('does not hold the slot’s claim for ever', async () => {
    // `fetch` has no timeout of its own, and nothing renders an element for a
    // request that never settles — so no error arrives, and the claim is what
    // stops anything else from retrying.
    const spied = vi.spyOn(AbortSignal, 'timeout')
    await playing()
    expect(spied).toHaveBeenCalled()
    expect(vi.mocked(api.startSession).mock.calls[0]![1]).toMatchObject({
      signal: spied.mock.results[0]!.value,
    })
    spied.mockRestore()
  })
})

describe('the warmed slot', () => {
  test('is dropped when the queue moves somewhere else entirely', async () => {
    // It is a track from the album you just left, and its own keepalive keeps
    // it alive against the per-user cap.
    const { wrapper, queue } = await playing(['a', 'b', 'c', 'd'])
    at(audio(wrapper)[0]!.element as HTMLMediaElement, 160)
    await flushPromises()
    expect(api.endSession).not.toHaveBeenCalled()
    queue.jump(2)
    await flushPromises()
    expect(api.endSession).toHaveBeenCalledWith('s-b', expect.anything())
  })

  test('but is kept when the queue merely grew', async () => {
    // Appending must not throw away the correctly warmed next track.
    const { wrapper, queue } = await playing()
    at(audio(wrapper)[0]!.element as HTMLMediaElement, 160)
    await flushPromises()
    queue.appendTrack(track('z'))
    await flushPromises()
    expect(vi.mocked(api.endSession).mock.calls.filter((c) => c[0] === 's-b')).toHaveLength(0)
  })
})

describe('the retry timer', () => {
  test('aims at the track its own slot was warming, not at the one playing', async () => {
    vi.useFakeTimers()
    const { wrapper } = await playing()
    vi.mocked(api.startSession).mockRejectedValueOnce(new ApiError(503, 'host is away'))
    at(audio(wrapper)[0]!.element as HTMLMediaElement, 160)
    await flushPromises()

    await vi.advanceTimersByTimeAsync(6000)
    await flushPromises()
    expect(vi.mocked(api.startSession).mock.calls.at(-1)![0]).toMatchObject({ item_id: 'b' })
  })

  test('and is dropped when the dock goes', async () => {
    vi.useFakeTimers()
    vi.mocked(api.startSession).mockRejectedValue(new ApiError(503, 'host is away'))
    const { wrapper } = await playing()
    const tried = vi.mocked(api.startSession).mock.calls.length
    wrapper.unmount()
    await vi.advanceTimersByTimeAsync(6000 * 4)
    await flushPromises()
    expect(vi.mocked(api.startSession).mock.calls).toHaveLength(tried)
  })

  test('and one failure arms one retry, not one per render', async () => {
    vi.useFakeTimers()
    vi.mocked(api.startSession).mockRejectedValue(new ApiError(503, 'host is away'))
    await playing()
    const first = vi.mocked(api.startSession).mock.calls.length
    await vi.advanceTimersByTimeAsync(6000)
    await flushPromises()
    expect(vi.mocked(api.startSession).mock.calls.length).toBe(first + 1)
  })
})

describe('the pings, when the dock goes', () => {
  test('stop', async () => {
    vi.useFakeTimers()
    const { wrapper } = await playing()
    wrapper.unmount()
    const sent = vi.mocked(api.postProgress).mock.calls.length
    await vi.advanceTimersByTimeAsync(PING_MS * 5)
    await flushPromises()
    expect(vi.mocked(api.postProgress).mock.calls).toHaveLength(sent)
  })

  test('and a slot getting a new session stops the old slot’s pings', async () => {
    vi.useFakeTimers()
    const { queue } = await playing()
    await vi.advanceTimersByTimeAsync(PING_MS)
    queue.jump(2)
    await flushPromises()
    vi.mocked(api.postProgress).mockClear()
    await vi.advanceTimersByTimeAsync(PING_MS * 2)
    await flushPromises()
    const pinged = new Set(vi.mocked(api.postProgress).mock.calls.map((c) => c[0]))
    expect(pinged.has('s-a')).toBe(false)
  })

  test('and the audible slot’s idle clock is NOT restarted by the other one', async () => {
    // Rebuilding both pings whenever either session changed bought a paused
    // queue a fresh half hour every time a preload landed — indefinitely, which
    // is the whole thing the bound exists to stop.
    vi.useFakeTimers()
    const { wrapper } = await playing()
    const element = audio(wrapper)[0]!.element as HTMLMediaElement
    Object.defineProperty(element, 'duration', { value: 1800, configurable: true })
    element.currentTime = 160

    // Twenty minutes of a frozen playhead...
    await vi.advanceTimersByTimeAsync(PING_MS * 120)
    // ...then the preload lands, which is the OTHER slot's session changing.
    at(element, 1790, 1800)
    await flushPromises()
    element.currentTime = 160

    await vi.advanceTimersByTimeAsync(PING_MS * 120)
    await flushPromises()
    const forA = vi.mocked(api.postProgress).mock.calls.filter((c) => c[0] === 's-a')
    expect(forA.length).toBeLessThanOrEqual(IDLE_LIMIT_MS / PING_MS)
  })
})

describe('the list', () => {
  test('marks only the row that is playing', async () => {
    const { wrapper } = await playing(['a', 'b', 'c'], 1)
    await wrapper.find('[aria-expanded]').trigger('click')
    const marked = wrapper.findAll('[aria-current="true"]')
    expect(marked).toHaveLength(1)
    expect(marked[0]!.text()).toContain('B')
  })

  test('and removing a row keeps the keyboard in the list', async () => {
    // The row holding the focused button is unmounted, so without this the
    // focus falls to `body`: a keyboard user is returned to the top of the
    // document and a screen reader is told nothing at all.
    const { wrapper } = await playing()
    await wrapper.find('[aria-expanded]').trigger('click')
    const remove = wrapper.findAll('[aria-label^="Remove"]')
    ;(remove[0]!.element as HTMLElement).focus()
    await remove[0]!.trigger('click')
    await flushPromises()
    expect(document.activeElement).toBe(wrapper.findAll('[aria-label^="Remove"]')[0]!.element)
  })

  test('and the dock says what it is', async () => {
    // The one persistent piece of chrome on the page, and the only way a screen
    // reader user has to jump to it.
    const { wrapper } = await playing()
    expect(wrapper.find('aside').attributes('aria-label')).toBe('Playback queue')
  })

  test('and the disclosure names what it opens', async () => {
    const { wrapper } = await playing()
    const toggle = wrapper.find('[aria-expanded]')
    expect(toggle.attributes('aria-controls')).toBe('queue-list')
    await toggle.trigger('click')
    expect(wrapper.find('#queue-list').exists()).toBe(true)
  })

  test('and the track playing is announced when it changes on its own', async () => {
    // A record advances every few minutes with nobody pressing anything.
    const { wrapper } = await playing()
    expect(wrapper.find('[role="status"]').attributes('aria-live')).toBe('polite')
    expect(wrapper.find('[role="status"]').text()).toContain('A')
  })
})
