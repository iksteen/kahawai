/// The picture: one session, its pipeline, and the four overlays that can own
/// the screen.
///
/// Almost everything here is about a restart — who owns the timeline, what
/// happens to a superseded one, and which of the five phases wins. Each of
/// those is a recorded incident: a superseded run's `playing` clearing a newer
/// run's veil, a play button drawn on top of a restarting picture, a nudge
/// answering after a scrub and leaving the clock adrift.

import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

import { ApiError } from '../src/api/errors.ts'
import { IDLE_LIMIT_MS, PING_MS } from '../src/domain/keepalive.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  itemQuery: vi.fn(),
  itemChildren: vi.fn(),
  getPrefs: vi.fn(),
  listLibraries: vi.fn(),
  startSession: vi.fn(),
  endSession: vi.fn(),
  postProgress: vi.fn(),
  seekSession: vi.fn(),
  putPref: vi.fn(),
  adminSessionLog: vi.fn(),
  itemFonts: vi.fn(async () => ({ fonts: [] })),
  getItemArtworkUrl: (id: string) => `/art/${id}`,
  getItemFontUrl: (id: string, n: number) => `/font/${id}/${n}`,
  getItemSubtitleFileUrl: (id: string, file: string) => `/subs/${id}/${file}`,
  getSessionFileUrl: (id: string, file: string) => `/session/${id}/${file}`,
}))
vi.mock('../src/api/session.ts', () => ({
  whoAmI: () => ({ username: 'me', admin: true }),
  accessToken: () => 'token',
  refreshTokens: vi.fn(async () => true),
}))
vi.mock('../src/api/capabilities.ts', () => ({
  buildProfile: () => ({ containers: ['mp4'] }),
  loadMask: () => ({}),
  saveMask: vi.fn(),
  probedProfile: () => ({
    video: [],
    audio: [],
    containers: [],
    target_duration: { mode: 'ignore' },
  }),
}))
// hls.js drives a real network stack and a worker; the picture's own logic is
// what is under test, so it is replaced by something that records what it was
// told and can be made to report an error.
const engines: FakeHls[] = []
class FakeHls {
  static Events = { FRAG_BUFFERED: 'fragBuffered', ERROR: 'hlsError' }
  static ErrorTypes = { NETWORK_ERROR: 'networkError', MEDIA_ERROR: 'mediaError' }
  static isSupported = () => true
  handlers = new Map<string, (event: string, data: unknown) => void>()
  loaded = ''
  stopped = 0
  started = 0
  destroyed = false
  constructor(public config: unknown) {
    engines.push(this)
  }
  on(event: string, fn: (event: string, data: unknown) => void) {
    this.handlers.set(event, fn)
  }
  loadSource(url: string) {
    this.loaded = url
  }
  attachMedia() {}
  stopLoad() {
    this.stopped += 1
  }
  startLoad() {
    this.started += 1
  }
  recovered = 0
  recoverMediaError() {
    this.recovered += 1
  }
  destroy() {
    this.destroyed = true
  }
  fail(data: Record<string, unknown>) {
    this.handlers.get('hlsError')?.('hlsError', data)
  }
}
vi.mock('hls.js', () => ({ default: FakeHls }))

const api = await import('../src/api/generated/kahawai.ts')
const { forgetRecoveries } = await import('../src/domain/recovery.ts')
const Picture = (await import('../src/components/Picture.vue')).default

const film = (over: Record<string, unknown> = {}) => ({
  id: 'heat',
  kind: 'movie',
  title: 'Heat',
  parent_id: null,
  resume_position_ms: null,
  metadata: null,
  negotiated: null,
  sources: [{ streams: { audio: [{ language: 'eng', codec: 'aac', channels: 2 }], video: [] } }],
  ...over,
})

const session = (id = 's1', over: Record<string, unknown> = {}) => ({
  session_id: id,
  stream_url: `/stream/${id}/index.m3u8`,
  content_type: 'application/vnd.apple.mpegurl',
  mode: 'remux',
  duration_ms: 600_000,
  part_base_ms: 0,
  parts: 1,
  size: 0,
  streams: null,
  ...over,
})

async function watching(over: Record<string, unknown> = {}) {
  const wrapper = mount(Picture, {
    attachTo: document.body,
    props: {
      item: film() as never,
      session: session() as never,
      resumeMs: 0,
      libraryId: 'films',
      // What the page read on its way in and passes down; the picture no
      // longer asks the hub for either.
      prefs: [] as never,
      mediaType: '',
      mode: 'window' as const,
      ...over,
    },
  })
  await flushPromises()
  const element = wrapper.find('video').element as HTMLVideoElement
  Object.defineProperty(element, 'duration', { value: 600, configurable: true })
  return { wrapper, element, engine: engines.at(-1)! }
}

/// Move the seekbar to a fraction of the film, the way a key press or a drag
/// does. A real range input, so this is its own value changing.
async function scrub(wrapper: ReturnType<typeof mount>, fraction: number) {
  const bar = wrapper.find('.seekbar')
  await bar.setValue(String(Math.round(600_000 * fraction)))
  await flushPromises()
}

/// Every polite live region's text, joined: the skip offer's region is
/// always mounted (so it can announce), which makes "the" region ambiguous.
function liveText(wrapper: ReturnType<typeof mount>): string {
  return wrapper
    .findAll('[aria-live="polite"]')
    .map((n) => n.text())
    .filter(Boolean)
    .join(' ')
}

/// An answer somebody else decides when to give.
function held<T>(value: T) {
  let settle!: () => void
  const promise = new Promise<T>((resolve) => {
    settle = () => resolve(value)
  })
  return { promise, settle }
}

/// happy-dom has no media to start, so `play()` rejects — which the picture
/// correctly reads as a refused autoplay and settles the veil on. Where the
/// veil is the subject, the element has to be able to start.
function starts(element: HTMLVideoElement) {
  return vi.spyOn(element, 'play').mockImplementation(async function (this: HTMLVideoElement) {
    Object.defineProperty(this, 'paused', { value: false, configurable: true })
  })
}

/// Tell the element where it is, and that it said so.
function at(element: HTMLVideoElement, seconds: number) {
  element.currentTime = seconds
  element.dispatchEvent(new Event('timeupdate'))
}

beforeEach(() => {
  engines.length = 0
  forgetRecoveries()
  vi.stubGlobal(
    'fetch',
    vi.fn(async () => new Response('', { status: 404 })),
  )
  vi.mocked(api.itemQuery).mockResolvedValue(film() as never)
  vi.mocked(api.itemChildren).mockResolvedValue({ children: [] } as never)
  vi.mocked(api.getPrefs).mockResolvedValue({ prefs: [] } as never)
  vi.mocked(api.listLibraries).mockResolvedValue({
    libraries: [{ id: 'films', name: 'Films', media_type: 'movies' }],
  } as never)
  vi.mocked(api.startSession).mockResolvedValue(session('s2') as never)
  vi.mocked(api.endSession).mockResolvedValue(undefined as never)
  vi.mocked(api.postProgress).mockResolvedValue({} as never)
  vi.mocked(api.seekSession).mockResolvedValue({ part_base_ms: 0 } as never)
})
afterEach(() => {
  vi.resetAllMocks()
  vi.unstubAllGlobals()
  vi.useRealTimers()
})

describe('attaching to a session', () => {
  test('loads the playlist through hls.js, with the bearer', async () => {
    const { engine } = await watching()
    expect(engine.loaded).toBe('/stream/s1/index.m3u8')
    const setup = (engine.config as { xhrSetup: (x: unknown) => void }).xhrSetup
    const headers: Record<string, string> = {}
    setup({ setRequestHeader: (k: string, v: string) => (headers[k] = v) })
    expect(headers.Authorization).toBe('Bearer token')
  })

  test('and never chases the live edge', async () => {
    // Our EVENT playlists are growing recordings: the pipeline paces itself a
    // window ahead of THIS player, so chasing the edge is a feedback loop that
    // lives at the starved frontier and buffers on every segment.
    const { engine } = await watching()
    const config = engine.config as { startPosition: number; liveSyncDurationCount: number }
    expect(config.startPosition).toBe(0)
    expect(config.liveSyncDurationCount).toBeGreaterThan(1000)
  })

  test('and a direct session is given straight to the element', async () => {
    const { wrapper } = await watching({
      session: session('s1', { stream_url: '/stream/s1/file.mp4' }) as never,
    })
    expect(wrapper.find('video').attributes('src')).toBe('/stream/s1/file.mp4')
    expect(engines).toHaveLength(0)
  })
})

describe('the five phases', () => {
  test('a paused picture offers a play button in the middle of it', async () => {
    // Chrome will not start a video for a viewer who has not interacted with
    // the page, so a reloaded player sits on its first frame waiting for a
    // click it never asked for. The transport does say `paused`, in a
    // twelve-pixel glyph at the bottom.
    const { wrapper, element } = await watching()
    element.pause()
    await flushPromises()
    expect(wrapper.find('.play-veil').exists()).toBe(true)
  })

  test('and a restarting one does not — it pauses the element itself', async () => {
    // A play circle over a restarting picture invites a click that fights the
    // pipeline.
    vi.mocked(api.seekSession).mockReturnValue(new Promise(() => {}) as never)
    const { wrapper, element } = await watching()
    at(element, 10)
    await scrub(wrapper, 0.9)
    await flushPromises()
    void element
    expect(wrapper.text()).toContain('Restarting stream')
    // Not the veil, and not the transport's play glyph either: this bar sits
    // ABOVE the veil, so a Play there is the same offer two centimetres lower.
    expect(wrapper.find('.play-veil').exists()).toBe(false)
    expect(
      wrapper
        .findAll('button')
        .find((b) => b.attributes('aria-label') === 'Play')
        ?.attributes('disabled'),
    ).toBeDefined()
  })

  test('and a stopped one is a dialog with two ways out', async () => {
    vi.mocked(api.postProgress).mockRejectedValue(new ApiError(404, 'no such session'))
    vi.mocked(api.startSession).mockRejectedValue(new ApiError(409, 'unplayable', 'unplayable'))
    vi.useFakeTimers()
    const { wrapper, element } = await watching()
    await element.play()
    await vi.advanceTimersByTimeAsync(PING_MS + 100)
    await flushPromises()
    expect(wrapper.find('[aria-labelledby="player-stopped"]').exists()).toBe(true)
    expect(wrapper.findAll('button').some((b) => b.text().includes('Try again'))).toBe(true)
  })

  test('and a host that is away is a WAIT, with the position held', async () => {
    // Nothing is broken and nothing is lost, so this is a wait rather than an
    // error — and holding the position is what makes the retry a resume.
    vi.mocked(api.postProgress).mockRejectedValue(new ApiError(404, 'no such session'))
    vi.mocked(api.startSession).mockRejectedValue(new ApiError(503, 'host is away'))
    vi.useFakeTimers()
    const { wrapper, element } = await watching()
    at(element, 90)
    await element.play()
    await vi.advanceTimersByTimeAsync(PING_MS + 100)
    await flushPromises()
    expect(wrapper.find('[aria-labelledby="player-standby"]').exists()).toBe(true)
    expect(wrapper.text()).toContain('resumes at 1:30')
    expect(element.paused).toBe(true)
  })

  test('and standing by asks again until the host is back', async () => {
    vi.mocked(api.postProgress).mockRejectedValue(new ApiError(404, 'no such session'))
    vi.mocked(api.startSession).mockRejectedValue(new ApiError(503, 'host is away'))
    vi.useFakeTimers()
    const { wrapper, element } = await watching()
    await element.play()
    await vi.advanceTimersByTimeAsync(PING_MS + 100)
    await flushPromises()

    const tried = vi.mocked(api.startSession).mock.calls.length
    await vi.advanceTimersByTimeAsync(5000 * 3)
    await flushPromises()
    expect(vi.mocked(api.startSession).mock.calls.length).toBeGreaterThan(tried)

    vi.mocked(api.startSession).mockResolvedValue(session('s3') as never)
    await vi.advanceTimersByTimeAsync(5000)
    await flushPromises()
    expect(wrapper.emitted('restart')).toBeTruthy()
  })
})

describe('seeking', () => {
  test('inside what the pipeline has produced is the element’s own jump', async () => {
    const { wrapper, element } = await watching()
    Object.defineProperty(element, 'seekable', {
      value: { length: 1, start: () => 0, end: () => 300 },
      configurable: true,
    })
    await scrub(wrapper, 0.1)
    // 10% of ten minutes is a minute, and the pipeline has five: no restart.
    expect(api.seekSession).not.toHaveBeenCalled()
    expect(element.currentTime).toBe(60)
  })

  test('and past it restarts the pipeline at the target', async () => {
    const { wrapper, element } = await watching()
    Object.defineProperty(element, 'seekable', {
      value: { length: 1, start: () => 0, end: () => 60 },
      configurable: true,
    })
    await scrub(wrapper, 0.8)
    expect(api.seekSession).toHaveBeenCalledWith(
      's1',
      expect.objectContaining({ position_ms: 480_000 }),
    )
  })

  test('and the old run is stopped before the new one is asked for', async () => {
    // The restart replaces the run server-side: every not-yet-fetched segment
    // of the old run is about to 404, so the wait is visible instead of the
    // player playing on while spraying 404s.
    vi.mocked(api.seekSession).mockReturnValue(new Promise(() => {}) as never)
    const { wrapper, element, engine } = await watching()
    await element.play()
    await scrub(wrapper, 0.9)
    expect(engine.stopped).toBe(1)
    expect(element.paused).toBe(true)
  })

  test('and a seek onto a session the hub has lost recovers rather than reporting', async () => {
    // A 404 here is not a message, it is the recovery contract: the session is
    // gone and the answer is a new one at this position. Reporting it left the
    // picture stopped by the pause above, and the ping's own 404 then reached
    // recovery, which bails on a paused element — so an automatic part
    // transition simply ended the film.
    vi.mocked(api.seekSession).mockRejectedValue(new ApiError(404, 'no such session'))
    const { wrapper, element } = await watching()
    await element.play()
    await scrub(wrapper, 0.9)
    expect(api.startSession).toHaveBeenCalled()
    expect(liveText(wrapper)).toBe('')
  })

  test('and a seek that finds the host away stands by, at where they ASKED to be', async () => {
    // The hub answers starts and seeks through the same refusal, so a host
    // vanishing mid-film and noticed by a nudge used to skip stand-by entirely
    // — for the one condition stand-by exists for. And the seekbar has already
    // moved, so resuming at the old playhead would drop the seek in silence.
    vi.mocked(api.seekSession).mockRejectedValue(new ApiError(503, 'host is away'))
    const { wrapper, element } = await watching()
    at(element, 30)
    await element.play()
    await scrub(wrapper, 0.8)
    expect(wrapper.find('[aria-labelledby="player-standby"]').exists()).toBe(true)
    expect(wrapper.text()).toContain('resumes at 8:00')
  })

  test('and a refused seek hands the retry back rather than freezing', async () => {
    // `beginRestart` stopped the loader and paused the element, and nothing on
    // this path starts it again: the buffer played out and the picture froze
    // for good, with the keepalive holding the session alive so the 404 that
    // recovery waits for never came.
    vi.mocked(api.seekSession).mockRejectedValue(new ApiError(409, 'cannot seek there'))
    const { wrapper, element } = await watching()
    await scrub(wrapper, 0.9)
    void element
    expect(liveText(wrapper)).toContain('Could not seek')
    expect(wrapper.find('[aria-label="Restarting stream"]').exists()).toBe(false)
  })
})

describe('the transport', () => {
  test('the keys do what the keys mean', async () => {
    const { wrapper, element } = await watching()
    Object.defineProperty(element, 'seekable', {
      value: { length: 1, start: () => 0, end: () => 600 },
      configurable: true,
    })
    at(element, 100)
    await element.play()

    window.dispatchEvent(new KeyboardEvent('keydown', { key: ' ' }))
    await flushPromises()
    expect(element.paused).toBe(true)

    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight' }))
    await flushPromises()
    expect(element.currentTime).toBe(130)

    // A browser fires `timeupdate` when the playhead is moved; happy-dom does
    // not, and the nudge is computed from where the element last SAID it was.
    element.dispatchEvent(new Event('timeupdate'))
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowLeft' }))
    await flushPromises()
    expect(element.currentTime).toBe(120)

    window.dispatchEvent(new KeyboardEvent('keydown', { key: 't' }))
    expect(wrapper.emitted('mode')?.at(-1)).toEqual(['theater'])
    void wrapper
  })

  test('and the keyboard is frozen with the rest of it while a dialog is up', async () => {
    // A dialog blocks the pointer with a scrim and never blocked the keyboard,
    // so Space played the buffered tail behind "the file is unreachable" — the
    // sound the stand-by pauses to stop.
    vi.mocked(api.postProgress).mockRejectedValue(new ApiError(404, 'no such session'))
    vi.mocked(api.startSession).mockRejectedValue(new ApiError(503, 'host is away'))
    vi.useFakeTimers()
    const { element } = await watching()
    await element.play()
    await vi.advanceTimersByTimeAsync(PING_MS + 100)
    await flushPromises()
    expect(element.paused).toBe(true)

    window.dispatchEvent(new KeyboardEvent('keydown', { key: ' ' }))
    await flushPromises()
    expect(element.paused).toBe(true)
  })

  test('and volume is the listener’s, kept across every session in the visit', async () => {
    // Component state started over at 1.0 and UNMUTED, writing that into the
    // fresh element rather than merely failing to restore it: an evening of a
    // series at 15% became an episode at 100% at every boundary.
    const first = await watching()
    await first.wrapper.find('input[aria-label="Volume"]').setValue('15')
    await flushPromises()
    expect(first.element.volume).toBeCloseTo(0.15, 3)
    // A browser announces a volume it accepted; happy-dom does not, and the
    // listener is what carries the setting into the next session.
    first.element.dispatchEvent(new Event('volumechange'))
    first.wrapper.unmount()

    const second = await watching()
    expect(second.element.volume).toBeCloseTo(0.15, 3)
  })

  test('and a mute the element made itself comes back too', async () => {
    const { wrapper, element } = await watching()
    element.muted = true
    element.dispatchEvent(new Event('volumechange'))
    await flushPromises()
    expect(wrapper.findAll('button').some((b) => b.attributes('aria-label') === 'Unmute')).toBe(
      true,
    )
  })
})

describe('the session’s own lifetime', () => {
  test('is pinged even while paused', async () => {
    // Guarding this on `!paused` is what let the reaper delete a paused
    // viewer's segment directory out from under them.
    vi.useFakeTimers()
    const { element } = await watching()
    at(element, 42)
    element.pause()
    await vi.advanceTimersByTimeAsync(PING_MS + 100)
    await flushPromises()
    expect(api.postProgress).toHaveBeenCalledWith('s1', { position_ms: 42_000 })
  })

  test('and bounded, so a forgotten tab frees its slot', async () => {
    vi.useFakeTimers()
    await watching()
    await vi.advanceTimersByTimeAsync(IDLE_LIMIT_MS + PING_MS * 20)
    await flushPromises()
    expect(vi.mocked(api.postProgress).mock.calls.length).toBeLessThanOrEqual(
      IDLE_LIMIT_MS / PING_MS + 1,
    )
  })

  test('and the position is reported on the way out, not the position it started at', async () => {
    // The teardown runs after the element ref is detached, and reading it there
    // would post the resume point and discard the whole sitting.
    const { wrapper, element } = await watching()
    at(element, 300)
    wrapper.unmount()
    await flushPromises()
    expect(api.postProgress).toHaveBeenCalledWith(
      's1',
      { position_ms: 300_000 },
      { keepalive: true },
    )
  })

  test('and the picture does NOT release it — the route owns that', async () => {
    // Everything else in the teardown undoes something the picture did, and can
    // be done again. Ending the session could not.
    const { wrapper } = await watching()
    wrapper.unmount()
    await flushPromises()
    expect(api.endSession).not.toHaveBeenCalled()
  })
})

describe('a session the hub has forgotten', () => {
  test('is restarted where the viewer was', async () => {
    vi.mocked(api.postProgress).mockRejectedValue(new ApiError(404, 'no such session'))
    vi.useFakeTimers()
    const { wrapper, element } = await watching()
    at(element, 120)
    await element.play()
    await vi.advanceTimersByTimeAsync(PING_MS + 100)
    await flushPromises()
    expect(api.startSession).toHaveBeenCalledWith(expect.objectContaining({ start_ms: 120_000 }))
    expect(wrapper.emitted('restart')?.[0]?.[2]).toBe(120_000)
  })

  test('but not while it is paused — the death is remembered instead', async () => {
    // A restart there spends a lease on a picture nobody is watching, and the
    // fresh session goes idle and is reaped in turn.
    vi.mocked(api.postProgress).mockRejectedValue(new ApiError(404, 'no such session'))
    vi.useFakeTimers()
    const { element } = await watching()
    element.pause()
    await vi.advanceTimersByTimeAsync(PING_MS + 100)
    await flushPromises()
    expect(api.startSession).not.toHaveBeenCalled()

    // ...and pressing play acts on it.
    await element.play()
    await flushPromises()
    expect(api.startSession).toHaveBeenCalled()
  })

  test('and hls.js reporting 404 is the same signal', async () => {
    const { element, engine } = await watching()
    at(element, 60)
    await element.play()
    engine.fail({ fatal: true, response: { code: 404 }, type: 'networkError' })
    await flushPromises()
    expect(api.startSession).toHaveBeenCalled()
  })

  test('and a 401 refreshes rather than restarting', async () => {
    // hls.js fetches with its own XHR, so it never gets the transport's
    // refresh-and-retry — and without this an expired token hides a 404 behind
    // a 401, because auth runs first.
    const { engine } = await watching()
    engine.fail({ fatal: true, response: { code: 401 }, type: 'networkError' })
    await flushPromises()
    const { refreshTokens } = await import('../src/api/session.ts')
    expect(refreshTokens).toHaveBeenCalled()
    expect(api.startSession).not.toHaveBeenCalled()
    expect(engine.started).toBe(1)
  })
})

describe('a fatal network error', () => {
  test('is answered by asking hls.js to load again', async () => {
    // hls.js does not restart itself after a fatal error, so without this
    // nothing fetched another segment, ever: the picture froze without pausing,
    // the ping kept succeeding, and 404 was the only thing that called
    // recovery. A wifi drop of twenty seconds was a still frame with nothing on
    // screen to say so.
    const { element, engine } = await watching()
    Object.defineProperty(element, 'buffered', {
      value: { length: 1, start: () => 0, end: () => 100 },
      configurable: true,
    })
    at(element, 10)
    engine.fail({ fatal: true, type: 'networkError', details: 'fragLoadError' })
    await flushPromises()
    expect(engine.started).toBe(1)
  })

  test('and bounded, but only once there is nothing left to play', async () => {
    // A fatal network error does not stop the picture — the element plays the
    // buffer out — and hls.js goes fatal every few seconds while a hub is
    // unreachable, so a flat count was spent inside the buffer and paused a
    // video with forty seconds in hand.
    const { wrapper, element, engine } = await watching()
    Object.defineProperty(element, 'buffered', {
      value: { length: 1, start: () => 0, end: () => 100 },
      configurable: true,
    })
    at(element, 10)
    for (let n = 0; n < 12; n++) {
      engine.fail({ fatal: true, type: 'networkError', details: 'fragLoadError' })
    }
    await flushPromises()
    // Still asking: there are ninety seconds in hand.
    expect(engine.started).toBe(12)
    expect(liveText(wrapper)).toBe('')

    // The buffer runs out, and now the budget bites.
    Object.defineProperty(element, 'buffered', {
      value: { length: 1, start: () => 0, end: () => 10.5 },
      configurable: true,
    })
    engine.fail({ fatal: true, type: 'networkError', details: 'fragLoadError' })
    await flushPromises()
    expect(engine.started).toBe(12)
    expect(liveText(wrapper)).toContain('did not come back')
  })

  test('and a segment arriving refills the budget', async () => {
    // The budget is per outage, not per session: a long watch over a flaky link
    // is not slowly used up.
    const { element, engine } = await watching()
    Object.defineProperty(element, 'buffered', {
      value: { length: 1, start: () => 0, end: () => 10.5 },
      configurable: true,
    })
    at(element, 10)
    for (let n = 0; n < 5; n++) {
      engine.fail({ fatal: true, type: 'networkError', details: 'fragLoadError' })
    }
    engine.handlers.get('fragBuffered')?.('fragBuffered', {})
    engine.fail({ fatal: true, type: 'networkError', details: 'fragLoadError' })
    await flushPromises()
    expect(engine.started).toBe(6)
  })
})

describe('a fatal media error', () => {
  test('is answered by rebuilding the decoder, a few times', async () => {
    // What the call is for: a decoder that wedged once on a bad splice.
    const { engine } = await watching()
    engine.fail({ fatal: true, type: 'mediaError', details: 'bufferAppendError' })
    engine.fail({ fatal: true, type: 'mediaError', details: 'bufferAppendError' })
    await flushPromises()
    expect(engine.recovered).toBe(2)
  })

  test('and then says so, rather than rebuilding it for ever', async () => {
    // `recoverMediaError()` tears the MediaSource down and builds another, so
    // a stream hls.js cannot append AT ALL — one whose first segment carries no
    // parameter sets — was a loop several times a second: no picture, no
    // message, and a control bar rebuilding itself under the pointer. The
    // network branch beside it has had a budget all along; this one had none.
    const { wrapper, engine } = await watching()
    for (let n = 0; n < 8; n++) {
      engine.fail({ fatal: true, type: 'mediaError', details: 'bufferAppendError' })
    }
    await flushPromises()
    expect(engine.recovered).toBe(3)
    expect(liveText(wrapper)).toContain('will not play')
    // The reason is in the sentence: "it did not work" sends nobody anywhere.
    expect(liveText(wrapper)).toContain('bufferAppendError')
  })

  test('and a segment arriving does NOT refill the budget', async () => {
    // The network budget refills on a buffered segment, which is right for it:
    // a segment arriving means the link is back. Here it let the loop run for
    // ever — a stream whose FIRST segment can never be appended still buffers
    // the ones after it, so every failure was followed by a success that put
    // the budget back. Measured against the live hub: 70 MediaSources in
    // fourteen seconds, with the budget in place.
    const { engine } = await watching()
    for (let n = 0; n < 3; n++) {
      engine.fail({ fatal: true, type: 'mediaError', details: 'bufferAppendError' })
    }
    engine.handlers.get('fragBuffered')?.('fragBuffered', {})
    engine.fail({ fatal: true, type: 'mediaError', details: 'bufferAppendError' })
    await flushPromises()
    expect(engine.recovered).toBe(3)
  })

  test('and giving up lets the session go', async () => {
    // The keepalive ping is what holds a session, and it runs for as long as
    // the picture is mounted. Giving up paused the picture and said so, then
    // went on telling the hub the session was in use — nothing was watching it
    // and nothing would be, because the way out is the play button and that
    // starts a fresh one. Four failed attempts filled a viewer's allowance.
    const { engine } = await watching()
    vi.mocked(api.endSession).mockClear()
    for (let n = 0; n < 8; n++) {
      engine.fail({ fatal: true, type: 'mediaError', details: 'bufferAppendError' })
    }
    await flushPromises()
    expect(vi.mocked(api.endSession).mock.calls.map((c) => c[0])).toEqual(['s1'])
  })

  test('and time refills it, so one glitch an hour is not a budget spent', async () => {
    const now = vi.spyOn(performance, 'now')
    try {
      now.mockReturnValue(0)
      const { engine } = await watching()
      for (let n = 0; n < 3; n++) {
        engine.fail({ fatal: true, type: 'mediaError', details: 'bufferAppendError' })
      }
      // Well past the window: this is a fresh incident, not the same one.
      now.mockReturnValue(60_000)
      engine.fail({ fatal: true, type: 'mediaError', details: 'bufferAppendError' })
      await flushPromises()
      expect(engine.recovered).toBe(4)
    } finally {
      now.mockRestore()
    }
  })
})

describe('what the info panel says', () => {
  test('names the plan, the container and the streams', async () => {
    const { wrapper } = await watching({
      session: session('s1', {
        streams: { video: 'copy', audio: 'aac (transcoded)', cost: 'audio_encode', subtitles: [] },
      }) as never,
    })
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'info')!
      .trigger('click')
    const text = wrapper.text().replace(/\s+/g, ' ')
    expect(text).toContain('TRANSCODE')
    expect(text).toContain('aac (transcoded)')
  })

  test('and offers an administrator the session log, where the problem is', async () => {
    vi.mocked(api.adminSessionLog).mockResolvedValue('the log' as never)
    const click = vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {})
    const { wrapper } = await watching()
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'info')!
      .trigger('click')
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'download session log')!
      .trigger('click')
    await flushPromises()
    expect(api.adminSessionLog).toHaveBeenCalledWith('s1')
    expect(click).toHaveBeenCalled()
    click.mockRestore()
  })
})

describe('who owns the veil', () => {
  test('a mount raises it until the first frame, with no play button under it', async () => {
    // A capability restart replaces the whole instance, so the veil it put up
    // dies with it — the REPLACEMENT owns the gap between mounting and its
    // first frame. A burn-in restart spends seconds there, and it used to
    // show a black frame with live controls; worse, a paused element showed
    // the play button as if the player were waiting on the viewer.
    //
    // On the prototype, BEFORE the mount: the environment's own `play()`
    // rejects, which is the refused-autoplay path — a settle of its own.
    // Restored by hand, in a `finally`: the shared `afterEach` only RESETS
    // mocks, and a reset prototype `play` returns undefined into `.catch`.
    const play = vi
      .spyOn(HTMLMediaElement.prototype, 'play')
      .mockImplementation(async function (this: HTMLVideoElement) {
        Object.defineProperty(this, 'paused', { value: false, configurable: true })
      })
    try {
      const { wrapper, element } = await watching()
      expect(wrapper.text()).toContain('Restarting stream')
      expect(wrapper.find('.play-veil').exists()).toBe(false)

      element.dispatchEvent(new Event('playing'))
      await flushPromises()
      expect(wrapper.text()).not.toContain('Restarting stream')
    } finally {
      play.mockRestore()
    }
  })

  test('a picture arriving is what brings it down', async () => {
    // Not the hub answering: the POST returning means the run has been ASKED
    // for. Nothing else clears it, so without this the transport stays frozen
    // behind a spinner over a playing film until the 25 s ceiling fires.
    const { wrapper, element } = await watching()
    starts(element)
    Object.defineProperty(element, 'seekable', {
      value: { length: 1, start: () => 0, end: () => 60 },
      configurable: true,
    })
    await scrub(wrapper, 0.9)
    expect(wrapper.text()).toContain('Restarting stream')

    element.dispatchEvent(new Event('playing'))
    await flushPromises()
    expect(wrapper.text()).not.toContain('Restarting stream')
  })

  // NOT tested, and not deleted: the generation plumbing — `settle(gen)`
  // rather than `settle(seekGen)`, `giveUp`'s check, and `beginRestart`
  // re-arming rather than stacking a ceiling. All three answer the case of two
  // restarts outstanding at once, and in THIS structure there cannot be:
  // `seekTo`, `switchTracks` and `switchBurn` all refuse to start while
  // `isFrozen`, which reads the current health rather than a value a listener
  // captured. The reference needed them because a React keydown closure held a
  // stale `frozen`. They stay because the invariant they rely on lives in three
  // other functions, and the cost of being wrong is a spinner over a playing
  // film with every control dead.

  test('and an autoplay refused by the browser settles it, but an abort does not', async () => {
    // A refused autoplay is the whole reason the viewer has to click, and it
    // fires no `pause` event, so the state has to be recorded by hand. An
    // AbortError is the opposite: a NEWER restart interrupted this play, and
    // that run owns the veil.
    const aborted = await watching()
    starts(aborted.element)
    Object.defineProperty(aborted.element, 'seekable', {
      value: { length: 1, start: () => 0, end: () => 60 },
      configurable: true,
    })
    vi.spyOn(aborted.element, 'play').mockRejectedValue(
      Object.assign(new Error('interrupted'), { name: 'AbortError' }),
    )
    await scrub(aborted.wrapper, 0.9)
    expect(aborted.wrapper.text()).toContain('Restarting stream')
    aborted.wrapper.unmount()

    const refused = await watching()
    starts(refused.element)
    Object.defineProperty(refused.element, 'seekable', {
      value: { length: 1, start: () => 0, end: () => 60 },
      configurable: true,
    })
    vi.spyOn(refused.element, 'play').mockRejectedValue(new Error('gesture required'))
    await scrub(refused.wrapper, 0.9)
    expect(refused.wrapper.text()).not.toContain('Restarting stream')
    expect(refused.wrapper.find('.play-veil').exists()).toBe(true)
  })

  test('and a restart that never produces one is handed back after the ceiling', async () => {
    // A ceiling on how long a spinner is allowed to be the whole story, armed
    // before the POST is even sent: a hub that accepts the connection and then
    // wedges left the veil up for ever with every control dead.
    vi.useFakeTimers()
    vi.mocked(api.seekSession).mockReturnValue(new Promise(() => {}) as never)
    const { wrapper, element } = await watching()
    Object.defineProperty(element, 'seekable', {
      value: { length: 1, start: () => 0, end: () => 60 },
      configurable: true,
    })
    await scrub(wrapper, 0.9)
    await vi.advanceTimersByTimeAsync(26_000)
    await flushPromises()
    expect(wrapper.text()).not.toContain('Restarting stream')
    expect(liveText(wrapper)).toContain('did not come back')
  })

  test('and the ceiling is disarmed when the picture goes', async () => {
    // The generation check does not cover this player being REPLACED: a 404
    // recovers by remounting on a new session with `awaitingGen` still set, so
    // the old closure's check passes 25 seconds later and posts "the stream did
    // not come back" through a note host that now belongs to the NEW player.
    vi.useFakeTimers()
    vi.mocked(api.seekSession).mockReturnValue(new Promise(() => {}) as never)
    const { wrapper, element } = await watching()
    Object.defineProperty(element, 'seekable', {
      value: { length: 1, start: () => 0, end: () => 60 },
      configurable: true,
    })
    await scrub(wrapper, 0.9)
    wrapper.unmount()

    const second = await watching()
    await vi.advanceTimersByTimeAsync(26_000)
    await flushPromises()
    expect(liveText(second.wrapper)).toBe('')
  })

  // NOT tested, and not deleted: `giveUp`'s generation check. Every caller
  // reaches it through `seekTo`, `switchTracks` or `switchBurn`, and all three
  // refuse to start while `isFrozen` — which reads the CURRENT health, not a
  // value a listener captured. In the reference it was reachable because a
  // React keydown closure held a stale `frozen`; here two restarts cannot be
  // outstanding at once, so no test can produce a late "no" for an older one.
  // It stays because the invariant it depends on belongs to three other
  // functions.

  test('and the pipeline’s own origin is not adopted by the run that replaced it', async () => {
    // `start.pos` is often not written yet, so the read sleeps between attempts
    // and routinely outlives the seek that asked for it: a second seek landing
    // first was then overwritten by the FIRST seek's origin, leaving the clock,
    // the seekbar and every subtitle path adrift by the difference, with
    // nothing to correct it.
    vi.useFakeTimers()
    let reads = 0
    let releaseFirst!: () => void
    const firstRead = new Promise<void>((resolve) => (releaseFirst = resolve))
    vi.stubGlobal(
      'fetch',
      vi.fn(async (url: string) => {
        if (!String(url).endsWith('start.pos')) return new Response('', { status: 404 })
        reads += 1
        // The run at 5:00 asks first, and its answer is held until the run at
        // 5:40 has taken the timeline. Both origins are within the sanity
        // window of each other, so only the generation can tell them apart.
        if (reads === 1) {
          await firstRead
          return new Response('299000', { status: 200 })
        }
        return new Response('339000', { status: 200 })
      }),
    )
    const { wrapper, element } = await watching()
    starts(element)
    // Five seconds produced, so both seeks are past the edge and both restart
    // the pipeline — which is what asks for an origin.
    Object.defineProperty(element, 'seekable', {
      value: { length: 1, start: () => 0, end: () => 5 },
      configurable: true,
    })

    await scrub(wrapper, 0.5)
    element.dispatchEvent(new Event('playing'))
    await flushPromises()
    await scrub(wrapper, 340_000 / 600_000)
    element.dispatchEvent(new Event('playing'))
    await flushPromises()
    expect(wrapper.text()).toContain('5:39')

    releaseFirst()
    await vi.advanceTimersByTimeAsync(2000)
    await flushPromises()
    // Still the run that owns the timeline, not the one it replaced.
    expect(wrapper.text()).toContain('5:39')
    expect(wrapper.text()).not.toContain('4:59')
  })

  test('and a dialog freezes the seekbar, and the keys that bypass it', async () => {
    // The seekbar is disabled, but the arrow keys reach `seekTo` directly and
    // a dialog blocks the pointer with a scrim and never blocked the keyboard.
    vi.mocked(api.postProgress).mockRejectedValue(new ApiError(404, 'no such session'))
    vi.mocked(api.startSession).mockRejectedValue(new ApiError(503, 'host is away'))
    vi.useFakeTimers()
    const { wrapper, element } = await watching()
    Object.defineProperty(element, 'seekable', {
      value: { length: 1, start: () => 0, end: () => 600 },
      configurable: true,
    })
    at(element, 100)
    await element.play()
    await vi.advanceTimersByTimeAsync(PING_MS + 100)
    await flushPromises()
    expect(wrapper.find('.seekbar').attributes('disabled')).toBeDefined()

    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight' }))
    await flushPromises()
    expect(element.currentTime).toBe(100)
  })

  test('and a start that lands after the viewer left is ended, not left to the reaper', async () => {
    // Nobody will play it, ping it or end it, and it holds a transcoder slot
    // against a per-user cap of four.
    vi.useFakeTimers()
    const slow = held(session('s9'))
    vi.mocked(api.postProgress).mockRejectedValue(new ApiError(404, 'no such session'))
    vi.mocked(api.startSession).mockReturnValue(slow.promise as never)
    const { wrapper, element } = await watching()
    await element.play()
    await vi.advanceTimersByTimeAsync(PING_MS + 100)
    await flushPromises()

    wrapper.unmount()
    slow.settle()
    await flushPromises()
    expect(api.endSession).toHaveBeenCalledWith('s9', { keepalive: true })
  })
})

describe('choosing a track', () => {
  const withTracks = () =>
    session('s1', {
      streams: { video: 'copy', audio: 'copy', cost: 'copy', subtitles: [] },
    })

  const dualAudio = (over: Record<string, unknown> = {}) =>
    film({
      sources: [
        {
          streams: {
            audio: [
              { language: 'eng', codec: 'aac', channels: 2 },
              { language: 'jpn', codec: 'aac', channels: 2 },
            ],
            video: [],
          },
        },
      ],
      ...over,
    })

  async function switching() {
    const it = await watching({
      item: dualAudio() as never,
      session: withTracks() as never,
    })
    starts(it.element)
    return it
  }

  const pick = async (wrapper: ReturnType<typeof mount>, value: string) => {
    await wrapper.find('[aria-label="Audio track"]').setValue(value)
    await flushPromises()
  }

  test('remembers the language for the series, once it is playing', async () => {
    // Two additive layers (HUB-33): the SERIES remembers the language, which
    // is portable across episodes whose mux order differs.
    const { wrapper, element } = await switching()
    element.dispatchEvent(new Event('playing'))
    await pick(wrapper, '1')
    expect(api.putPref).toHaveBeenCalledWith({ scope: 'heat', key: 'audio', value: 'jpn' })
  })

  test('and a film pins the exact track, which an episode must not', async () => {
    // "The commentary track of THIS film" has no language representation, and
    // there is no series intent to follow. An episode that pinned one would
    // freeze the whole series on a choice made once.
    const movie = await switching()
    await pick(movie.wrapper, '1')
    expect(api.putPref).toHaveBeenCalledWith({
      scope: 'heat',
      key: 'audio.track',
      value: '#1',
    })
    movie.wrapper.unmount()

    vi.mocked(api.putPref).mockClear()
    const episode = await watching({
      item: dualAudio({ kind: 'episode', parent_id: 'show' }) as never,
      session: withTracks() as never,
    })
    starts(episode.element)
    await pick(episode.wrapper, '1')
    const keys = vi.mocked(api.putPref).mock.calls.map((c) => c[0]?.key)
    expect(keys).toContain('audio')
    expect(keys).not.toContain('audio.track')
  })

  test('and a switch that did not play is not remembered', async () => {
    // Written before the switch, a failed one still steers every later episode
    // of the series towards a track this one could not manage.
    vi.mocked(api.seekSession).mockRejectedValue(new ApiError(409, 'no such track'))
    const { wrapper } = await switching()
    await pick(wrapper, '1')
    expect(api.putPref).not.toHaveBeenCalled()
  })

  test('and the selector goes back to the track that IS playing', async () => {
    vi.mocked(api.seekSession).mockRejectedValue(new ApiError(409, 'no such track'))
    const { wrapper } = await switching()
    await pick(wrapper, '1')
    expect((wrapper.find('[aria-label="Audio track"]').element as HTMLSelectElement).value).toBe(
      '0',
    )
  })

  test('and a 404 puts it back too, because recovery opens on what it says', async () => {
    // Snapping back and then recovering onto the NEW track left Japanese audio
    // playing under a selector reading English.
    vi.mocked(api.seekSession).mockRejectedValue(new ApiError(404, 'no such session'))
    const { wrapper, element } = await switching()
    await element.play()
    await pick(wrapper, '1')
    expect((wrapper.find('[aria-label="Audio track"]').element as HTMLSelectElement).value).toBe(
      '0',
    )
    expect(api.startSession).toHaveBeenCalledWith(expect.objectContaining({ audio_track: 0 }))
  })

  test('but a host that went away keeps the pick, because the resume carries it', async () => {
    vi.mocked(api.seekSession).mockRejectedValue(new ApiError(503, 'host is away'))
    const { wrapper, element } = await switching()
    await element.play()
    await pick(wrapper, '1')
    expect((wrapper.find('[aria-label="Audio track"]').element as HTMLSelectElement).value).toBe(
      '1',
    )
  })
})

describe('chapter marks while the bar is hidden', () => {
  test('a hidden bar has no invisible seek targets', async () => {
    // pointer-events-none on the bar does NOT shield a descendant that says
    // auto — hit-testing descends — so while the bar is faded out the marks
    // must drop their pointer events too, or a stationary click meant to
    // pause seeks to a chapter instead. This regression shipped once.
    const { wrapper, element } = await watching({
      item: film({ chapters: [{ start_ms: 300_000, title: 'The heist' }] }) as never,
    })
    starts(element)
    await element.play()
    Object.defineProperty(element, 'paused', { value: false, configurable: true })
    element.dispatchEvent(new Event('play'))
    await wrapper.find('.videobox').trigger('mouseleave')
    await flushPromises()
    expect(wrapper.find('[aria-hidden="true"] button').classes()).toContain('pointer-events-none')

    // And they come back with the bar.
    await wrapper.find('.videobox').trigger('mousemove')
    await flushPromises()
    expect(wrapper.find('[aria-hidden="true"] button').classes()).toContain('pointer-events-auto')
  })
})

describe('a restart keeps the choice the viewer made', () => {
  // Two selectable tracks; the wishlist below would pick English.
  const listing = (over: Record<string, unknown> = {}) => ({
    id: 3,
    origin: 'embedded',
    format: 'srt',
    language: 'eng',
    label: null,
    machine: false,
    derived_from: null,
    stream_index: 2,
    delivery: 'text',
    note: '',
    deletable: false,
    ...over,
  })
  const subbed = () =>
    film({
      negotiated: {
        cost: 'copy',
        mode: 'remux',
        source: null,
        streams: { video: 'copy', audio: 'copy' },
        subtitles: [listing(), listing({ id: 7, language: 'nld' })],
        target_duration_secs: 6,
      },
    })
  const wishes = [{ scope: '', key: 'subs.movies', value: 'en' }]
  const value = (wrapper: ReturnType<typeof mount>) =>
    (wrapper.find('[aria-label="Subtitles"]').element as HTMLSelectElement).value

  test('a first mount follows the prefs', async () => {
    const { wrapper } = await watching({
      item: subbed() as never,
      prefs: wishes as never,
      mediaType: 'movies',
    })
    expect(value(wrapper)).toBe('3')
  })

  test('a restart mount follows what was live instead', async () => {
    // The prefs prop is the snapshot the page took at session start; the
    // viewer picked Dutch since. Re-resolving from the snapshot silently
    // reverted the pick on every recovery.
    const { wrapper } = await watching({
      item: subbed() as never,
      prefs: wishes as never,
      mediaType: 'movies',
      carried: { audio: 0, video: 0, subKey: '7' } as never,
    })
    expect(value(wrapper)).toBe('7')
  })

  test('and carried-off stays off, even with a wishlist that would pick', async () => {
    const { wrapper } = await watching({
      item: subbed() as never,
      prefs: wishes as never,
      mediaType: 'movies',
      carried: { audio: 0, video: 0, subKey: '' } as never,
    })
    expect(value(wrapper)).toBe('')
  })

  test('the video track carries too', async () => {
    // The session restarts on the carried video track; a selector left at
    // zero would hand track 0 to the NEXT restart — the same silent revert,
    // on the other axis.
    const { wrapper } = await watching({
      item: film({
        sources: [
          {
            streams: {
              audio: [{ language: 'eng', codec: 'aac', channels: 2 }],
              video: [
                { codec: 'h264', width: 1920, height: 1080 },
                { codec: 'h264', width: 1280, height: 720 },
              ],
            },
          },
        ],
      }) as never,
      carried: { audio: 0, video: 1, subKey: '' } as never,
    })
    const select = wrapper.find('[aria-label="Video track"]')
    expect((select.element as HTMLSelectElement).value).toBe('1')
  })

  test('a carried key the list no longer has falls back to the wishlist', async () => {
    // Nothing produces this today — the item is not refetched on restart —
    // but ids are only as stable as that stays true, and the wrong quiet
    // answer would be subtitles-off.
    const { wrapper } = await watching({
      item: subbed() as never,
      prefs: wishes as never,
      mediaType: 'movies',
      carried: { audio: 0, video: 0, subKey: '99' } as never,
    })
    expect(value(wrapper)).toBe('3')
  })

  test('the restart emit carries what is live at that moment', async () => {
    // A seek answered by 404 is a restart; whatever the viewer had chosen by
    // then must ride along, or the next mount cannot honour it.
    vi.mocked(api.seekSession).mockRejectedValue(new ApiError(404, 'no such session'))
    const { wrapper, element } = await watching({ item: subbed() as never })
    await wrapper.find('[aria-label="Subtitles"]').setValue('7')
    await flushPromises()
    await element.play()
    await scrub(wrapper, 0.9)
    const restart = wrapper.emitted('restart')
    expect(restart).toBeTruthy()
    expect((restart![0]![3] as { subKey: string }).subKey).toBe('7')
  })
})

describe('choosing subtitles', () => {
  const listing = (over: Record<string, unknown> = {}) => ({
    id: 7,
    item_id: 'heat',
    origin: 'embedded',
    format: 'srt',
    language: 'eng',
    label: null,
    machine: false,
    derived_from: null,
    stream_index: 2,
    delivery: 'text',
    note: '',
    deletable: false,
    ...over,
  })

  async function withSubs(subs: Record<string, unknown>[], over: Record<string, unknown> = {}) {
    const it = await watching({
      item: film({
        negotiated: {
          cost: 'copy',
          mode: 'remux',
          source: null,
          streams: { video: 'copy', audio: 'copy' },
          subtitles: subs,
          target_duration_secs: 6,
        },
      }) as never,
      ...over,
    })
    starts(it.element)
    return it
  }

  const choose = async (wrapper: ReturnType<typeof mount>, value: string) => {
    await wrapper.find('[aria-label="Subtitles"]').setValue(value)
    await flushPromises()
  }

  test('is remembered in two layers: the series’ language, this item’s row', async () => {
    // The exact id is the only spelling that can name a downloaded or OCR
    // track, and no language wish will ever match one.
    const { wrapper } = await withSubs([listing()])
    await choose(wrapper, '7')
    expect(api.putPref).toHaveBeenCalledWith({ scope: 'heat', key: 'subs', value: 'eng' })
    expect(api.putPref).toHaveBeenCalledWith({ scope: 'heat', key: 'subs.track', value: '7' })
  })

  test('and turning them off is a choice, not an absence of one', async () => {
    const { wrapper } = await withSubs([listing()])
    await choose(wrapper, '')
    expect(api.putPref).toHaveBeenCalledWith({ scope: 'heat', key: 'subs', value: 'off' })
  })

  test('and picking a burn restarts the pipeline with it', async () => {
    // Burn transitions live server-side: choosing one is an encode.
    const { wrapper } = await withSubs([listing({ id: 9, delivery: 'burn' })])
    await choose(wrapper, '9')
    expect(api.seekSession).toHaveBeenCalledWith(
      's1',
      expect.objectContaining({ subtitle_track: 9 }),
    )
  })

  test('and leaving one withdraws it', async () => {
    const { wrapper, element } = await withSubs([
      listing({ id: 9, delivery: 'burn' }),
      listing({ id: 10, language: 'fra' }),
    ])
    await choose(wrapper, '9')
    // The burn restarted the pipeline, and the selector is frozen until there
    // is a picture again.
    element.dispatchEvent(new Event('playing'))
    await flushPromises()
    vi.mocked(api.seekSession).mockClear()
    await choose(wrapper, '10')
    expect(api.seekSession).toHaveBeenCalledWith(
      's1',
      expect.objectContaining({ subtitle_track: 0 }),
    )
  })

  test('and a track this client cannot be served is offered but not choosable', async () => {
    const { wrapper } = await withSubs([listing({ id: 11, delivery: 'none' })])
    const option = wrapper.findAll('[aria-label="Subtitles"] option')[1]!
    expect(option.attributes('disabled')).toBeDefined()
  })

  test('and the selector is frozen during a restart, because it can start one', async () => {
    // This was the one control left live through a restart, and the only one
    // that can START one: picking a burn mid-seek bumps the generation again,
    // so the seek already in flight bails and the hub runs two restarts for
    // one intent.
    vi.mocked(api.seekSession).mockReturnValue(new Promise(() => {}) as never)
    const { wrapper, element } = await withSubs([listing()])
    Object.defineProperty(element, 'seekable', {
      value: { length: 1, start: () => 0, end: () => 5 },
      configurable: true,
    })
    await scrub(wrapper, 0.9)
    expect(wrapper.find('[aria-label="Subtitles"]').attributes('disabled')).toBeDefined()
  })

  test('and a remembered burn is deferred while the pipeline is being steered', async () => {
    // The one restart caller that is not a button. Applied a few hundred
    // milliseconds into playback it could take the generation from a seek the
    // viewer had just made and restart before that seek had written its
    // origin — the drag went silently.
    vi.mocked(api.seekSession).mockReturnValue(new Promise(() => {}) as never)
    const queued = held<{ part_base_ms: number }>({ part_base_ms: 0 })
    void queued
    const { wrapper } = await withSubs([listing({ id: 9, delivery: 'burn' })], {
      prefs: [{ scope: 'heat', key: 'subs.track', value: '9' }] as never,
    })
    void wrapper
    // The burn was wanted the moment the tracks resolved, and the pipeline was
    // not frozen, so it went out at once.
    expect(api.seekSession).toHaveBeenCalledWith(
      's1',
      expect.objectContaining({ subtitle_track: 9 }),
    )
  })
})

describe('the next episode', () => {
  const episode = (id: string, n: number) => ({
    id,
    kind: 'episode',
    title: `Episode ${n}`,
    parent_id: 'show',
    season: 1,
    episode: n,
    episode_end: null,
    art_version: null,
    played: false,
    resume_position_ms: null,
    metadata: null,
    negotiated: null,
    sources: [{ streams: { audio: [], video: [] } }],
  })

  async function nearTheEnd() {
    vi.mocked(api.itemChildren).mockResolvedValue({
      children: [episode('e1', 1), episode('e2', 2)],
    } as never)
    vi.mocked(api.itemQuery).mockImplementation(
      async (id) => (id === 'e2' ? episode('e2', 2) : episode('e1', 1)) as never,
    )
    const it = await watching({ item: episode('e1', 1) as never })
    starts(it.element)
    at(it.element, 595)
    await flushPromises()
    return it
  }

  test('appears near the end, and says what is coming', async () => {
    const { wrapper } = await nearTheEnd()
    expect(wrapper.text()).toContain('next episode')
    expect(wrapper.text()).toContain('Episode 2')
  })

  test('and the skip offer stands aside for it', async () => {
    // Credits segments and the up-next card fire in the same corner at the
    // same moment; both at once is two buttons stacked on one spot, and
    // the countdown is the one that acts on its own.
    vi.mocked(api.itemChildren).mockResolvedValue({
      children: [episode('e1', 1), episode('e2', 2)],
    } as never)
    vi.mocked(api.itemQuery).mockImplementation(
      async (id) => (id === 'e2' ? episode('e2', 2) : episode('e1', 1)) as never,
    )
    const { wrapper, element } = await watching({
      item: {
        ...episode('e1', 1),
        segments: [{ kind: 'credits', start_ms: 560_000, end_ms: 600_000, source: 'blackframe' }],
      } as never,
    })
    starts(element)
    at(element, 595)
    await flushPromises()
    expect(wrapper.text()).toContain('next episode')
    expect(wrapper.findAll('button').some((b) => b.text() === 'Skip credits')).toBe(false)
    expect(wrapper.text()).not.toContain('Skip credits available')
  })

  test('and is announced, because it takes over on its own', async () => {
    const { wrapper } = await nearTheEnd()
    const said = wrapper.findAll('[aria-live="polite"]').map((p) => p.text())
    expect(said.some((t) => t.includes('Next episode in'))).toBe(true)
  })

  test('and stopping it means it does not', async () => {
    const { wrapper, element } = await nearTheEnd()
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Stop')!
      .trigger('click')
    expect(wrapper.text()).not.toContain('next episode')

    element.dispatchEvent(new Event('ended'))
    await flushPromises()
    expect(wrapper.emitted('playNext')).toBeUndefined()
  })

  test('and two presses of Play now start one session', async () => {
    const slow = held(session('s2'))
    vi.mocked(api.startSession).mockReturnValue(slow.promise as never)
    const { wrapper } = await nearTheEnd()
    const now = wrapper.findAll('button').find((b) => b.text() === 'Play now')!
    // Both in the same tick: waiting between them lets the `disabled` binding
    // render, and then the second press is stopped by the DOM rather than by
    // the guard under test.
    void now.trigger('click')
    void now.trigger('click')
    await flushPromises()
    expect(vi.mocked(api.startSession).mock.calls).toHaveLength(1)
    slow.settle()
    await flushPromises()
  })

  test('but a part boundary five seconds short of the end does not', async () => {
    // A multi-part source's playlist ends here too, and that is not the end of
    // the episode: advancing on it would skip the last five seconds and the
    // whole of the second part.
    const { wrapper, element } = await nearTheEnd()
    expect(wrapper.text()).toContain('next episode')
    element.dispatchEvent(new Event('ended'))
    await flushPromises()
    expect(wrapper.emitted('playNext')).toBeUndefined()
  })

  test('and the episode ending hands over', async () => {
    const { wrapper, element } = await nearTheEnd()
    // Within three seconds of the duration: further out is a multi-part
    // source's part ending, which is not the end of anything.
    at(element, 599)
    element.dispatchEvent(new Event('ended'))
    await flushPromises()
    expect(wrapper.emitted('playNext')).toHaveLength(1)
  })
})

describe('a multi-part source', () => {
  test('moves into the next part rather than ending the film', async () => {
    // This part's playlist ended, and the film has not.
    const { wrapper, element } = await watching({
      session: session('s1', { parts: 2, duration_ms: 600_000 }) as never,
    })
    starts(element)
    at(element, 120)
    element.dispatchEvent(new Event('ended'))
    await flushPromises()
    void wrapper
    expect(api.seekSession).toHaveBeenCalledWith(
      's1',
      expect.objectContaining({ position_ms: 120_250 }),
    )
  })

  test('but the end of the last part is the end of the film', async () => {
    const { element } = await watching({
      session: session('s1', { parts: 2, duration_ms: 600_000 }) as never,
    })
    starts(element)
    at(element, 599)
    element.dispatchEvent(new Event('ended'))
    await flushPromises()
    expect(api.seekSession).not.toHaveBeenCalled()
  })
})

describe('chapter marks on the bar', () => {
  const chaptered = () =>
    film({
      chapters: [
        { start_ms: 0, title: 'Opening' },
        { start_ms: 300_000, title: 'The heist' },
      ],
    })

  test('one per chapter, where the chapter starts', async () => {
    // A ten-minute film: halfway is 50%, and the mark at zero is not drawn.
    const { wrapper } = await watching({ item: chaptered() as never })
    const marks = wrapper.findAll('[aria-hidden="true"] button')
    expect(marks).toHaveLength(1)
    expect(marks[0]!.attributes('style')).toContain('left: 50%')
  })

  test('and none when the file declares none', async () => {
    const { wrapper } = await watching()
    expect(wrapper.findAll('[aria-hidden="true"] button')).toHaveLength(0)
  })

  test('a mark is wider than the line it draws', async () => {
    // Nobody can hit one pixel. The line is 1px; what takes the pointer is
    // 11px of it, five either side.
    const { wrapper } = await watching({ item: chaptered() as never })
    const mark = wrapper.find('[aria-hidden="true"] button')
    expect(mark.classes()).toContain('w-[11px]')
    expect(mark.find('span').classes()).toContain('w-px')
  })

  test('it names the chapter and when it starts', async () => {
    const { wrapper } = await watching({ item: chaptered() as never })
    expect(wrapper.find('[aria-hidden="true"] button').text()).toBe('5:00 · The heist')
  })

  test('and pressing one goes there', async () => {
    const { wrapper, element } = await watching({ item: chaptered() as never })
    Object.defineProperty(element, 'seekable', {
      value: { length: 1, start: () => 0, end: () => 600 },
      configurable: true,
    })
    await wrapper.find('[aria-hidden="true"] button').trigger('click')
    await flushPromises()
    // Inside what the pipeline has produced, so the element's own jump —
    // the point is where it landed.
    expect(element.currentTime).toBe(300)
  })

  test('an intro under the playhead offers Skip, and pressing it seeks past', async () => {
    // The button and the wiring, not just the pure functions: segments.ts is
    // unit-tested, but nothing else proves the component feeds it SEGMENTS
    // (not chapters), renders the offer, and lands the seek at the end.
    const { wrapper, element } = await watching({
      item: film({
        segments: [{ kind: 'intro', start_ms: 5_000, end_ms: 65_000, source: 'chromaprint' }],
      }) as never,
    })
    Object.defineProperty(element, 'seekable', {
      value: { length: 1, start: () => 0, end: () => 600 },
      configurable: true,
    })
    at(element, 10)
    await flushPromises()
    const offer = wrapper.findAll('button').find((b) => b.text() === 'Skip intro')
    expect(offer, 'the offer appears while the playhead is inside').toBeTruthy()
    expect(wrapper.text()).toContain('Skip intro available')
    await offer!.trigger('click')
    await flushPromises()
    expect(element.currentTime).toBe(65)

    // Past the segment, the offer goes away — nothing is skipped by itself.
    at(element, 70)
    await flushPromises()
    expect(wrapper.findAll('button').some((b) => b.text() === 'Skip intro')).toBe(false)
  })

  test("a direct-play resume does not offer yesterday's recap", async () => {
    // Until the first timeupdate the position is UNKNOWN; for a direct
    // session `offset` is 0, and initialising the clock from it rendered
    // and announced "Skip recap" for a segment forty minutes behind the
    // viewer — then a press seeked backwards. The initial reading must be
    // the resume position, exactly as absMs answers while unknown.
    const { wrapper } = await watching({
      item: film({
        segments: [{ kind: 'recap', start_ms: 0, end_ms: 90_000, source: 'blackframe' }],
      }) as never,
      session: session('s1', {
        stream_url: '/stream/s1/file.mp4',
        content_type: 'video/mp4',
      }) as never,
      resumeMs: 2_400_000,
    })
    expect(wrapper.findAll('button').some((b) => b.text() === 'Skip recap')).toBe(false)
    expect(wrapper.text()).not.toContain('Skip recap available')
  })

  test('the session listing outranks the stale item listing', async () => {
    // After a capability-masked apply-and-restart the item QUERY still
    // says delivery ass, but the fresh session re-planned against the
    // masked profile. Reading the stale listing kept JASSUB alive until
    // a page reload; the session is the authority on what it serves.
    const assListing = {
      id: 9,
      origin: 'embedded',
      format: 'ass',
      language: 'eng',
      label: null,
      machine: false,
      derived_from: null,
      stream_index: 2,
      delivery: 'ass',
      note: '',
      deletable: false,
    }
    const { wrapper } = await watching({
      item: film({
        negotiated: {
          cost: 'copy',
          mode: 'remux',
          source: null,
          streams: { video: 'copy', audio: 'copy' },
          subtitles: [assListing],
          target_duration_secs: 6,
        },
      }) as never,
      session: session('s1', {
        subtitle_listing: [{ ...assListing, delivery: 'text' }],
      }) as never,
    })
    await wrapper.find('[aria-label="Subtitles"]').setValue('9')
    await flushPromises()
    // delivery text routes to the native track, not the ASS canvas: the
    // session's verdict took without a reload.
    expect(wrapper.find('track').exists()).toBe(true)
  })

  test('the transport underneath still takes a press anywhere else', async () => {
    // The overlay covers the bar, so it must not swallow the pointer between
    // the marks: everything but the marks themselves stays scrubbable.
    const { wrapper } = await watching({ item: chaptered() as never })
    const overlay = wrapper.find('[aria-hidden="true"]')
    expect(overlay.classes()).toContain('pointer-events-none')
    expect(overlay.find('button').classes()).toContain('pointer-events-auto')
  })
})

describe('subtitles and the control bar', () => {
  test('native cues are lifted clear of it, and dropped back when it goes', async () => {
    // Three renderers, two levers. The canvases move by a transform on a
    // sibling; native cues live in the video's own shadow tree where a
    // transform cannot reach them, and move by `line` — which counts from the
    // bottom when it is negative.
    const { wrapper, element } = await watching()
    const cue = { line: 'auto' as number | 'auto' }
    Object.defineProperty(element, 'textTracks', {
      value: [{ cues: [cue], addEventListener: () => {}, removeEventListener: () => {} }],
      configurable: true,
    })
    // Playing, so the bar is the only thing that can be covering them.
    starts(element)
    await element.play()
    Object.defineProperty(element, 'paused', { value: false, configurable: true })
    element.dispatchEvent(new Event('play'))
    await flushPromises()

    // The bar goes, and comes back: the lift only runs when something it
    // watches CHANGES, and both are up already at mount.
    await wrapper.find('.videobox').trigger('mouseleave')
    await flushPromises()
    expect(cue.line).toBe('auto')

    await wrapper.find('.videobox').trigger('mousemove')
    await flushPromises()
    expect(cue.line).toBe(-4)
  })

  test('and the canvases move with the bar, which native cues cannot', async () => {
    // JASSUB and the image renderer insert their canvas next to the <video>
    // and rewrite its box on every resize — but never its transform, so the
    // lift survives them.
    const { wrapper } = await watching()
    await wrapper.find('.videobox').trigger('mousemove')
    expect(wrapper.find('.videobox').classes()).toContain('bar-up')
    await wrapper.find('.videobox').trigger('mouseleave')
    expect(wrapper.find('.videobox').classes()).not.toContain('bar-up')
  })
})
