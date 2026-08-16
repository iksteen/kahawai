/// The player as a page.
///
/// The subject here is the SESSION's lifetime: who starts one, who releases
/// one, and what happens to one that lands after the viewer has gone. A session
/// nobody releases holds a transcoder slot against a per-user cap of four until
/// the hub reaps it.

import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { createMemoryHistory, createRouter } from 'vue-router'

import { ApiError } from '../src/api/errors.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  itemQuery: vi.fn(),
  itemChildren: vi.fn(),
  getPrefs: vi.fn(),
  listLibraries: vi.fn(),
  startSession: vi.fn(),
  endSession: vi.fn(),
  postProgress: vi.fn(),
  seekSession: vi.fn(),
  adminSessionLog: vi.fn(),
  getItemArtworkUrl: (id: string) => `/art/${id}`,
  getItemFontUrl: (id: string, n: number) => `/font/${id}/${n}`,
  getItemSubtitleFileUrl: (id: string, file: string) => `/subs/${id}/${file}`,
  getSessionFileUrl: (id: string, file: string) => `/session/${id}/${file}`,
  itemFonts: vi.fn(async () => ({ fonts: [] })),
}))
vi.mock('../src/api/session.ts', () => ({
  whoAmI: () => ({ username: 'me', admin: false }),
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

const api = await import('../src/api/generated/kahawai.ts')
const { notice, clearNotices } = await import('../src/composables/notices.ts')
const Player = (await import('../src/views/Player.vue')).default
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

async function open(at = '/library/films/item/heat/play') {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/', name: 'libraries', component: { template: '<div />' } },
      { path: '/library/:library/item/:id', name: 'detail', component: { template: '<div />' } },
      { path: '/library/:library/item/:id/play', name: 'player', component: Player },
    ],
  })
  await router.push(at)
  await router.isReady()
  const wrapper = mount(Player, { global: { plugins: [router] } })
  await flushPromises()
  return { router, wrapper }
}

/// An answer somebody else decides when to give.
function held<T>(value: T) {
  let settle!: () => void
  const promise = new Promise<T>((resolve) => {
    settle = () => resolve(value)
  })
  return { promise, settle }
}

beforeEach(() => {
  // hls.js fetches the playlist as soon as it is attached, and there is no hub
  // here to answer. Stubbed so the noise does not drown the assertions.
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
  vi.mocked(api.startSession).mockResolvedValue(session() as never)
  vi.mocked(api.endSession).mockResolvedValue(undefined as never)
  vi.mocked(api.postProgress).mockResolvedValue({} as never)
  clearNotices()
})
afterEach(() => {
  vi.resetAllMocks()
  vi.unstubAllGlobals()
})

describe('opening the player', () => {
  test('starts a session for the item in the URL', async () => {
    // `/play` is an ADDRESS, not an instruction to the item page: a deep link, a
    // reload and a forward all have to land where pressing Play does.
    await open()
    expect(api.itemQuery).toHaveBeenCalledWith('heat', expect.anything())
    expect(api.startSession).toHaveBeenCalledWith(
      expect.objectContaining({ item_id: 'heat', start_ms: 0 }),
    )
  })

  test('and resumes where the film was left', async () => {
    vi.mocked(api.itemQuery).mockResolvedValue(film({ resume_position_ms: 90_000 }) as never)
    await open()
    expect(api.startSession).toHaveBeenCalledWith(expect.objectContaining({ start_ms: 90_000 }))
  })

  test('unless the button that was pressed said otherwise', async () => {
    // A bare URL always resumes, which is the safe default; the query is the
    // hint from "from start".
    vi.mocked(api.itemQuery).mockResolvedValue(film({ resume_position_ms: 90_000 }) as never)
    await open('/library/films/item/heat/play?start=0')
    expect(api.startSession).toHaveBeenCalledWith(expect.objectContaining({ start_ms: 0 }))
  })

  test('and asks for the track the viewer’s preferences name (HUB-33)', async () => {
    vi.mocked(api.itemQuery).mockResolvedValue(
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
      }) as never,
    )
    vi.mocked(api.getPrefs).mockResolvedValue({
      prefs: [{ scope: '', key: 'audio.movies', value: 'jpn' }],
    } as never)
    await open()
    expect(api.startSession).toHaveBeenCalledWith(expect.objectContaining({ audio_track: 1 }))
  })

  test('and a preference read that fails does not cost the bandwidth cap silently', async () => {
    // `prefs` is assigned after the await, so a rejection left it `[]` — and
    // `[]` is not nullish, so the preferences that DID arrive were replaced by
    // nothing. That drops the cap and starts on track 0, which is the
    // anime-in-English bug.
    vi.mocked(api.getPrefs).mockRejectedValue(new ApiError(500, 'nope'))
    await open()
    expect(notice.value).toContain('Could not resolve the audio track')
    expect(api.startSession).toHaveBeenCalled()
  })

  test('and the library details failing costs only the media type', async () => {
    vi.mocked(api.listLibraries).mockRejectedValue(new ApiError(500, 'nope'))
    await open()
    expect(notice.value).toContain('Could not load the library details')
    expect(api.startSession).toHaveBeenCalled()
  })
})

describe('a session nobody will play', () => {
  test('is ended rather than left for the reaper', async () => {
    // Started after the viewer left: nobody will play it, ping it or end it,
    // and it holds a slot against a per-user cap of four.
    const slow = held(session())
    vi.mocked(api.startSession).mockReturnValue(slow.promise as never)
    const { wrapper } = await open()
    wrapper.unmount()
    slow.settle()
    await flushPromises()
    expect(api.endSession).toHaveBeenCalledWith('s1', { keepalive: true })
  })

  test('and moving to another item releases the one it replaces', async () => {
    // Back and Forward across two `/play` entries reuse this component with a
    // new id: each pass overwrote the session and left a live one nobody could
    // reach. Four of those and the account is at its per-user cap.
    const { router } = await open()
    vi.mocked(api.itemQuery).mockResolvedValue(film({ id: 'other' }) as never)
    vi.mocked(api.startSession).mockResolvedValue(session('s2') as never)
    await router.push('/library/films/item/other/play')
    await flushPromises()
    expect(api.endSession).toHaveBeenCalledWith('s1', { keepalive: true })
  })

  test('and the heading is never the episode you just left', async () => {
    // `item` is a plain ref that `start` overwrites a round trip later, so
    // across two `/play` entries the screen kept naming the previous episode:
    // heading, tab strip and screen reader all confidently wrong, which is a
    // worse answer to "where am I" than no answer.
    const { wrapper, router } = await open()
    expect(wrapper.find('h1').text()).toBe('Heat')

    const late = held(film({ id: 'other', title: 'Sleepers' }))
    vi.mocked(api.itemQuery).mockReturnValue(late.promise as never)
    await router.push('/library/films/item/other/play')
    await flushPromises()
    expect(wrapper.find('h1').text()).toBe('Starting playback')

    late.settle()
    await flushPromises()
    expect(wrapper.find('h1').text()).toBe('Sleepers')
  })

  test('and the release happens AFTER the picture has said where the viewer got to', async () => {
    // The picture posts its final position in its teardown, and a release that
    // ran first sent that report to a session the route had already ended.
    const order: string[] = []
    vi.mocked(api.endSession).mockImplementation(async (id) => {
      order.push(`end ${id}`)
    })
    vi.mocked(api.postProgress).mockImplementation(async (id) => {
      order.push(`progress ${id}`)
      return {} as never
    })
    const { router } = await open()
    vi.mocked(api.itemQuery).mockResolvedValue(film({ id: 'other' }) as never)
    vi.mocked(api.startSession).mockResolvedValue(session('s2') as never)
    await router.push('/library/films/item/other/play')
    await flushPromises()
    expect(order.indexOf('progress s1')).toBeLessThan(order.indexOf('end s1'))
  })

  test('and the URL catching up with a handover does not start a second session', async () => {
    // The next-episode handover sets the item and the session together, and
    // the address follows it. Without the guard the route sees a new id and
    // starts another session for the episode already playing — and the one the
    // picture is holding is then the one nobody releases.
    const { wrapper } = await open()
    const started = vi.mocked(api.startSession).mock.calls.length

    const picture = wrapper.findComponent(Picture)
    picture.vm.$emit('playNext', film({ id: 'next' }), session('s2'))
    await flushPromises()
    expect(vi.mocked(api.startSession).mock.calls.length).toBe(started)
    // ...and the one it replaced is released, exactly once.
    expect(vi.mocked(api.endSession).mock.calls.filter((c) => c[0] === 's1')).toHaveLength(1)
  })

  test('and leaving the page releases the one that was playing', async () => {
    const { wrapper } = await open()
    wrapper.unmount()
    await flushPromises()
    expect(api.endSession).toHaveBeenCalledWith('s1', { keepalive: true })
  })
})

describe('when it cannot start', () => {
  test('a host that is away is a wait, not a fault', async () => {
    vi.mocked(api.startSession).mockRejectedValue(new ApiError(503, 'host is away'))
    const { wrapper } = await open()
    expect(wrapper.text()).toContain('not answering')
    expect(wrapper.text()).toContain('Try again in a moment')
  })

  test('and anything else says what the hub said', async () => {
    vi.mocked(api.startSession).mockRejectedValue(new ApiError(409, 'no sources', 'unplayable'))
    const { wrapper } = await open()
    expect(wrapper.text()).toContain('Could not start playback.')
    expect(wrapper.text()).toContain('no sources')
  })

  test('and Try again really does try again', async () => {
    vi.mocked(api.startSession).mockRejectedValueOnce(new ApiError(500, 'nope'))
    const { wrapper } = await open()
    expect(wrapper.text()).toContain('Could not start playback.')

    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('Try again'))!
      .trigger('click')
    await flushPromises()
    expect(wrapper.text()).not.toContain('Could not start playback.')
    expect(api.startSession).toHaveBeenCalledTimes(2)
  })

  test('and a failed start does not leave the OLD session beside the new item', async () => {
    // The guard that skips a start reads "a session AND the same item", so a
    // failure that keeps the old session makes Try again return early and hand
    // the picture one item's metadata over another item's stream.
    const { router, wrapper } = await open()
    vi.mocked(api.itemQuery).mockResolvedValue(film({ id: 'other' }) as never)
    vi.mocked(api.startSession).mockRejectedValue(new ApiError(500, 'nope'))
    await router.push('/library/films/item/other/play')
    await flushPromises()
    expect(wrapper.text()).toContain('Could not start playback.')

    vi.mocked(api.startSession).mockResolvedValue(session('s2') as never)
    vi.mocked(api.startSession).mockClear()
    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('Try again'))!
      .trigger('click')
    await flushPromises()
    expect(api.startSession).toHaveBeenCalledWith(expect.objectContaining({ item_id: 'other' }))
  })

  test('and there is a way back to the item', async () => {
    vi.mocked(api.startSession).mockRejectedValue(new ApiError(500, 'nope'))
    const { router, wrapper } = await open()
    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('Back to the item'))!
      .trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/library/films/item/heat')
  })
})

describe('the frame', () => {
  test('is one element for the whole visit, whatever is behind it', async () => {
    // A veil while the session is being started, then the picture, then a
    // different picture each time a restart replaces the session — and none of
    // those swaps may touch the window or the way out of it.
    const slow = held(session())
    vi.mocked(api.startSession).mockReturnValue(slow.promise as never)
    const { wrapper } = await open()
    expect(wrapper.find('.starting').exists()).toBe(true)
    expect(wrapper.text()).toContain('Starting playback')
    expect(wrapper.findAll('button').some((b) => b.text().includes('Back'))).toBe(true)

    slow.settle()
    await flushPromises()
    expect(wrapper.find('.starting').exists()).toBe(false)
    expect(wrapper.find('video').exists()).toBe(true)
  })

  test('and the starting box is the shape the picture will be', async () => {
    // The alternative is a visible jump the moment the video arrives.
    vi.mocked(api.itemQuery).mockResolvedValue(
      film({ negotiated: { source: { display_width: 1920, display_height: 800 } } }) as never,
    )
    vi.mocked(api.startSession).mockReturnValue(new Promise(() => {}) as never)
    const { wrapper } = await open()
    expect(wrapper.find('.starting').attributes('style')).toContain('1920 / 800')
  })
})
