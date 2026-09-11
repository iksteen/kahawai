/// The player as a page.
///
/// The subject here is the SESSION's lifetime: who starts one, who releases
/// one, and what happens to one that lands after the viewer has gone. A session
/// nobody releases holds a transcoder slot against a per-user cap of four until
/// the hub reaps it.

import { flushPromises, mount } from '@vue/test-utils'
import { VueQueryPlugin } from '@tanstack/vue-query'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { createMemoryHistory, createRouter } from 'vue-router'

import { ApiError } from '../src/api/errors.ts'
import { createQueryClient } from '../src/api/query.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  putPref: vi.fn(),
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

const film = (over: Record<string, unknown> = {}) => {
  const item = {
    id: 'heat',
    kind: 'movie',
    title: 'Heat',
    parent_id: null,
    resume_position_ms: null,
    metadata: null,
    negotiated: null,
    sources: [{ streams: { audio: [{ language: 'eng', codec: 'aac', channels: 2 }], video: [] } }],
    ...over,
  }
  return {
    ...item,
    sources: item.sources.map((source) => ({
      source_id: 1,
      collection_item_id: 'heat-copy',
      ...source,
    })),
  }
}

const session = (id = 's1', over: Record<string, unknown> = {}) => ({
  session_id: id,
  source_id: 1,
  source_fingerprint: 'physical-version-1',
  effective_start_ms: 0,
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
  // The page reads the library list through the shared cache — every route
  // into it already has one — so the harness has to provide it too.
  const wrapper = mount(Player, {
    global: { plugins: [router, [VueQueryPlugin, { queryClient: createQueryClient() }]] },
  })
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
  test.each([
    { swapped: false, override: false },
    { swapped: true, override: false },
    { swapped: false, override: true },
    { swapped: true, override: true },
  ])(
    'ranks preferred audio per source (swapped $swapped, override $override)',
    async ({ swapped, override }) => {
      const secondAudio = [
        { language: 'eng', codec: 'dts' },
        { language: 'jpn', codec: 'aac' },
      ]
      if (swapped) secondAudio.reverse()
      const sources = [
        {
          source_id: 1,
          collection_item_id: 'copy-a',
          streams: {
            video: [],
            audio: [
              { language: 'eng', codec: 'aac' },
              { language: 'jpn', codec: 'dts' },
            ],
          },
        },
        { source_id: 2, collection_item_id: 'copy-b', streams: { video: [], audio: secondAudio } },
      ]
      vi.mocked(api.getPrefs).mockResolvedValue({
        prefs: [{ scope: '', key: 'audio.movies', value: 'jpn' }],
      } as never)
      vi.mocked(api.itemQuery).mockImplementation(async (_id, body) => {
        // Simulate the hub judging each candidate's announced AAC/DTS stream.
        // A preview sees index0; only the final map can choose Japanese on B.
        const chosen =
          body?.source_id ??
          sources.find(
            (source) =>
              source.streams.audio[
                body?.source_audio_tracks?.[source.source_id] ?? body?.audio_track ?? 0
              ]?.codec === 'aac',
          )!.source_id
        return film({ sources, negotiated: { source: { source_id: chosen } } }) as never
      })
      vi.mocked(api.startSession).mockImplementation(
        async (body) => session('s1', { source_id: body.source_id }) as never,
      )
      const { wrapper } = await open(`/library/films/item/heat/play${override ? '?source=1' : ''}`)
      const chosen = override ? 1 : 2
      const index = override || !swapped ? 1 : 0
      expect(api.itemQuery).toHaveBeenLastCalledWith(
        'heat',
        expect.objectContaining({
          source_audio_tracks: { 1: 1, 2: swapped ? 0 : 1 },
          ...(override ? { source_id: 1 } : {}),
        }),
      )
      expect(api.startSession).toHaveBeenCalledWith(
        expect.objectContaining({ source_id: chosen, audio_track: index }),
      )
      const picture = wrapper.findComponent(Picture)
      expect(picture.props('item').negotiated?.source?.source_id).toBe(chosen)
      expect(picture.props('session').source_id).toBe(chosen)
      expect((wrapper.find('[aria-label="Audio track"]').element as HTMLSelectElement).value).toBe(
        String(index),
      )
      wrapper.unmount()
    },
  )

  test.each([
    { copy: 'old-copy', audio: 1 },
    { copy: 'new-copy', audio: 0 },
  ])(
    'an exact track belongs to collection copy $copy as well as the source number',
    async ({ copy, audio }) => {
      vi.mocked(api.getPrefs).mockResolvedValue({
        prefs: [
          { scope: 'source:old-copy:1', key: 'audio.track', value: '#1' },
          { scope: 'source:1', key: 'audio.track', value: '#1' },
          { scope: 'heat', key: 'audio', value: '#1' },
        ],
      } as never)
      vi.mocked(api.itemQuery).mockResolvedValue(
        film({
          negotiated: { source: { source_id: 1 } },
          sources: [
            {
              source_id: 1,
              collection_item_id: copy,
              streams: {
                audio: [{ language: 'eng' }, { language: 'eng' }],
                video: [],
              },
            },
          ],
        }) as never,
      )
      const { wrapper } = await open()
      expect(api.startSession).toHaveBeenCalledWith(
        expect.objectContaining({ source_id: 1, audio_track: audio }),
      )
      expect((wrapper.find('[aria-label="Audio track"]').element as HTMLSelectElement).value).toBe(
        String(audio),
      )
      expect(api.putPref).not.toHaveBeenCalled()
      wrapper.unmount()
    },
  )

  test('adopts a first-identification alias and retains its chapter request', async () => {
    vi.mocked(api.itemQuery).mockResolvedValue(
      film({ id: 'canonical', title: 'Canonical' }) as never,
    )
    const { router, wrapper } = await open('/library/films/item/heat/play?start=470512&source=1')
    await flushPromises()
    expect(router.currentRoute.value.params.id).toBe('canonical')
    expect(api.startSession).toHaveBeenCalledWith(
      expect.objectContaining({ item_id: 'canonical', start_ms: 470512 }),
    )
    expect(wrapper.find('h1').text()).toBe('Canonical')
    wrapper.findComponent(Picture).vm.$emit('restart', 'canonical', session('recovered'), 470512, {
      audio: 0,
      video: 0,
      subKey: '',
    })
    await flushPromises()
    expect(api.endSession).not.toHaveBeenCalledWith('recovered', expect.anything())
  })

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

  test.each([
    ['', true],
    ['&start=0', true],
    ['&start=0&chapter=1', false],
  ])(
    'uses the chosen source and preserves item/chapter positioning (%s)',
    async (position, resume) => {
      vi.mocked(api.itemQuery).mockResolvedValue(
        film({ negotiated: { source: { source_id: 2 } } }) as never,
      )
      const { router } = await open(`/library/films/item/heat/play?source=2${position}`)
      expect(api.itemQuery).toHaveBeenCalledWith('heat', expect.objectContaining({ source_id: 2 }))
      expect(api.startSession).toHaveBeenCalledWith(
        expect.objectContaining({ source_id: 2, resume }),
      )
      expect(router.currentRoute.value.query).toEqual({})
    },
  )

  test('a chapter position starts there, and is spent on arrival', async () => {
    vi.mocked(api.itemQuery).mockResolvedValue(film({ resume_position_ms: 90_000 }) as never)
    const { router } = await open('/library/films/item/heat/play?start=470512')
    expect(api.startSession).toHaveBeenCalledWith(expect.objectContaining({ start_ms: 470_512 }))
    // Spent: an hour later a reload must resume from progress, not jump the
    // viewer back to the chapter that opened the session.
    expect(router.currentRoute.value.query.start).toBeUndefined()
  })

  test('a failed start does not spend the asked-for position', async () => {
    // The chapter position is spent only once the session is UP: spent on
    // arrival, a transient 503 plus Try again silently resumed mid-film
    // instead of at the chapter that was pressed.
    vi.mocked(api.itemQuery).mockResolvedValue(film({ resume_position_ms: 90_000 }) as never)
    vi.mocked(api.startSession).mockRejectedValueOnce(new ApiError(503, 'host away'))
    const { router, wrapper } = await open('/library/films/item/heat/play?start=470512')
    expect(router.currentRoute.value.query.start).toBe('470512')

    vi.mocked(api.startSession).mockResolvedValue(session('s2') as never)
    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('Try again'))!
      .trigger('click')
    await flushPromises()
    expect(api.startSession).toHaveBeenLastCalledWith(
      expect.objectContaining({ start_ms: 470_512 }),
    )
    expect(router.currentRoute.value.query.start).toBeUndefined()
  })

  test('a start at or past the end is not a position, so it resumes', async () => {
    // Stale bookmark, hand-edited URL, or a file replaced by a shorter cut:
    // a position the file does not contain must not open a session there.
    vi.mocked(api.itemQuery).mockResolvedValue(
      film({ resume_position_ms: 90_000, duration_ms: 600_000 }) as never,
    )
    await open('/library/films/item/heat/play?start=600000')
    expect(api.startSession).toHaveBeenCalledWith(expect.objectContaining({ start_ms: 90_000 }))
  })

  test('an unknown running time lets the asked-for position through', async () => {
    // The range check needs a range; without one the hub clamps, which is
    // strictly better than silently resuming somewhere else.
    vi.mocked(api.itemQuery).mockResolvedValue(
      film({ resume_position_ms: 90_000, duration_ms: null }) as never,
    )
    await open('/library/films/item/heat/play?start=470512')
    expect(api.startSession).toHaveBeenCalledWith(expect.objectContaining({ start_ms: 470_512 }))
  })

  test('a mangled start is not a position, so it resumes', async () => {
    // Number('') is 0: a truncated ?start= would otherwise silently mean
    // "from the beginning".
    vi.mocked(api.itemQuery).mockResolvedValue(film({ resume_position_ms: 90_000 }) as never)
    await open('/library/films/item/heat/play?start=')
    expect(api.startSession).toHaveBeenCalledWith(expect.objectContaining({ start_ms: 90_000 }))
  })

  test('and asks for the track the viewer’s preferences name (HUB-33)', async () => {
    vi.mocked(api.itemQuery).mockResolvedValue(
      film({
        negotiated: { source: { source_id: 1 } },
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
    expect(api.startSession).toHaveBeenCalledWith(
      expect.objectContaining({ audio_track: 1, source_id: 1 }),
    )
  })

  test('and a preference read that fails does not cost the bandwidth cap silently', async () => {
    // `prefs` is assigned after the await, so a rejection left it `[]` — and
    // `[]` is not nullish, so the preferences that DID arrive were replaced by
    // nothing. That drops the cap and starts on track 0, which is the
    // anime-in-English bug.
    vi.mocked(api.getPrefs).mockRejectedValue(new ApiError(500, 'nope'))
    await open()
    expect(notice.value).toContain('Could not read your preferences')
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
    picture.vm.$emit('playNext', 'heat', film({ id: 'next' }), session('s2'))
    await flushPromises()
    expect(vi.mocked(api.startSession).mock.calls.length).toBe(started)
    // ...and the one it replaced is released, exactly once.
    expect(vi.mocked(api.endSession).mock.calls.filter((c) => c[0] === 's1')).toHaveLength(1)
  })

  test('a restart from a picture the route left behind is released, not adopted', async () => {
    // An async restart can land after the viewer has navigated to another
    // item; adopting it would put the previous episode's stream under this
    // page. Nobody else holds that session, so this handler ends it.
    const { wrapper } = await open()
    const picture = wrapper.findComponent(Picture)
    picture.vm.$emit('restart', 'somewhere-else', session('s9'), 1000, {
      audio: 0,
      video: 0,
      subKey: '',
    })
    await flushPromises()
    expect(api.endSession).toHaveBeenCalledWith('s9', { keepalive: true })
    expect(wrapper.find('video').exists()).toBe(true)
  })

  test('a handover from a picture the route left behind is released too', async () => {
    // Same guard as the restart twin below-left: `advanced` adopts nothing
    // from a picture whose item is not this page's any more.
    const { wrapper } = await open()
    const picture = wrapper.findComponent(Picture)
    picture.vm.$emit('playNext', 'somewhere-else', film({ id: 'next' }), session('s9'), [])
    await flushPromises()
    expect(api.endSession).toHaveBeenCalledWith('s9', { keepalive: true })
    // The page still plays what it was playing.
    expect(vi.mocked(api.endSession).mock.calls.filter((c) => c[0] === 's1')).toHaveLength(0)
  })

  test('an unmount in the same flush as a restart releases the retired one too', async () => {
    // The release rides a post-flush watcher, and a stopped watcher's
    // queued job is SKIPPED: a navigation landing in the same flush as a
    // restart unmounted the page with the retired session still unreleased,
    // holding one of the account's four slots until the idle reaper. The
    // owed ledger's unmount drain is what pays it.
    const { wrapper } = await open()
    const picture = wrapper.findComponent(Picture)
    picture.vm.$emit('restart', 'heat', session('s2'), 1000, {
      audio: 0,
      video: 0,
      subKey: '',
    })
    // No flush: unmount before the watcher's job can run.
    wrapper.unmount()
    await flushPromises()
    expect(vi.mocked(api.endSession).mock.calls.filter((c) => c[0] === 's1')).toHaveLength(1)
    expect(vi.mocked(api.endSession).mock.calls.filter((c) => c[0] === 's2')).toHaveLength(1)
  })

  test('and leaving the page releases the one that was playing', async () => {
    const { wrapper } = await open()
    wrapper.unmount()
    await flushPromises()
    expect(api.endSession).toHaveBeenCalledWith('s1', { keepalive: true })
  })
})

describe('refreshing the physical source after recovery', () => {
  const choice = {
    audio: 1,
    video: 1,
    subKey: '10',
    embeddedSub: 3,
    sourceFingerprint: 'physical-version-1',
  }
  const recovered = (sourceId = 99) =>
    film({
      negotiated: {
        source: { source_id: sourceId, display_width: 1920, display_height: 800 },
        subtitles: [],
      },
      sources: [
        {
          source_id: sourceId,
          collection_item_id: 'returned-copy',
          streams: {
            audio: [
              { language: 'eng', codec: 'aac', channels: 2 },
              { language: 'nld', codec: 'aac', channels: 2 },
            ],
            video: [
              { codec: 'h264', width: 1920, height: 800 },
              { codec: 'h264', width: 1280, height: 720 },
            ],
          },
        },
      ],
    })

  test.each([99, 1])(
    'refreshes the returned source %s, even when its number was reused',
    async (sourceId) => {
      const { wrapper } = await open()
      const slow = held(recovered(sourceId))
      // Automatic negotiation would choose a third copy. The recovery must ask
      // for the actual source in its response, not the original/default copy.
      vi.mocked(api.itemQuery).mockImplementation(
        (_id, body) =>
          (body?.source_id === sourceId
            ? slow.promise
            : Promise.resolve(film({ negotiated: { source: { source_id: 2 } } }))) as never,
      )
      const fresh = session('s2', {
        source_id: sourceId,
        effective_start_ms: 12_345,
        subtitle_listing: [
          {
            id: 20,
            origin: 'embedded',
            stream_index: 3,
            language: 'nld',
            format: 'srt',
            delivery: 'vtt',
          },
        ],
      })
      wrapper.findComponent(Picture).vm.$emit('restart', 'heat', fresh, 999, choice)
      await flushPromises()
      expect(api.itemQuery).toHaveBeenLastCalledWith(
        'heat',
        expect.objectContaining({
          source_id: sourceId,
          profile: { containers: ['mp4'] },
          audio_track: 1,
          video_track: 1,
        }),
      )
      expect(wrapper.findComponent(Picture).props('session').session_id).toBe('s1')
      slow.settle()
      await flushPromises()
      const picture = wrapper.findComponent(Picture)
      expect(picture.props('session').session_id).toBe('s2')
      expect(picture.props('item').sources[0]?.collection_item_id).toBe('returned-copy')
      expect(picture.props('resumeMs')).toBe(12_345)
      expect((wrapper.find('[aria-label="Audio track"]').element as HTMLSelectElement).value).toBe(
        '1',
      )
      expect((wrapper.find('[aria-label="Video track"]').element as HTMLSelectElement).value).toBe(
        '1',
      )
      expect((wrapper.find('[aria-label="Subtitles"]').element as HTMLSelectElement).value).toBe(
        '20',
      )

      vi.mocked(api.seekSession).mockResolvedValue({ part_base_ms: 0 } as never)
      vi.mocked(api.putPref).mockResolvedValue(undefined as never)
      await wrapper.find('[aria-label="Audio track"]').setValue('0')
      await flushPromises()
      expect(api.putPref).toHaveBeenCalledWith({
        scope: `source:returned-copy:${sourceId}`,
        key: 'audio.track',
        value: '#0',
      })
      wrapper.unmount()
    },
  )

  test.each(['refusal', 'missing source'])(
    'releases the new session if refreshing fails: %s',
    async (failure) => {
      const { wrapper } = await open()
      if (failure === 'refusal')
        vi.mocked(api.itemQuery).mockRejectedValueOnce(new ApiError(503, 'refresh unavailable'))
      else vi.mocked(api.itemQuery).mockResolvedValueOnce(film() as never)
      wrapper
        .findComponent(Picture)
        .vm.$emit('restart', 'heat', session('s2', { source_id: 99 }), 0, choice)
      await flushPromises()
      expect(api.endSession).toHaveBeenCalledWith('s2', { keepalive: true })
      expect(wrapper.text()).toContain('Could not refresh playback details')
      expect(wrapper.findComponent(Picture).exists()).toBe(false)
      vi.mocked(api.itemQuery).mockResolvedValue(film() as never)
      vi.mocked(api.startSession).mockResolvedValue(session('s3') as never)
      await wrapper
        .findAll('button')
        .find((b) => b.text().includes('Try again'))!
        .trigger('click')
      await flushPromises()
      expect(wrapper.findComponent(Picture).props('session').session_id).toBe('s3')
      wrapper.unmount()
      expect(vi.mocked(api.endSession).mock.calls.filter(([id]) => id === 's2')).toHaveLength(1)
    },
  )

  test('a route change cancels a pending refresh without adopting its late answer', async () => {
    const { wrapper, router } = await open()
    const slow = held(recovered())
    vi.mocked(api.itemQuery).mockReturnValueOnce(slow.promise as never)
    wrapper
      .findComponent(Picture)
      .vm.$emit('restart', 'heat', session('s2', { source_id: 99 }), 0, choice)
    await flushPromises()
    vi.mocked(api.itemQuery).mockResolvedValue(film({ id: 'other' }) as never)
    vi.mocked(api.startSession).mockResolvedValue(session('s3') as never)
    await router.push('/library/films/item/other/play')
    await flushPromises()
    expect(api.endSession).toHaveBeenCalledWith('s2', { keepalive: true })
    slow.settle()
    await flushPromises()
    expect(wrapper.findComponent(Picture).props('session').session_id).toBe('s3')
    expect(wrapper.findComponent(Picture).props('item').id).toBe('other')
    expect(vi.mocked(api.endSession).mock.calls.filter(([id]) => id === 's2')).toHaveLength(1)
    wrapper.unmount()
  })

  test('a superseded refresh cannot release or replace the newer recovery', async () => {
    const { wrapper } = await open()
    const old = held(recovered(98))
    const latest = held(recovered(99))
    vi.mocked(api.itemQuery)
      .mockReturnValueOnce(old.promise as never)
      .mockReturnValueOnce(latest.promise as never)
    const picture = wrapper.findComponent(Picture)
    picture.vm.$emit('restart', 'heat', session('s2', { source_id: 98 }), 0, choice)
    picture.vm.$emit('restart', 'heat', session('s3', { source_id: 99 }), 0, choice)
    expect(api.endSession).toHaveBeenCalledWith('s2', { keepalive: true })
    old.settle()
    await flushPromises()
    expect(wrapper.findComponent(Picture).props('session').session_id).toBe('s1')
    expect(api.endSession).not.toHaveBeenCalledWith('s3', { keepalive: true })
    latest.settle()
    await flushPromises()
    expect(wrapper.findComponent(Picture).props('session').session_id).toBe('s3')
    expect(vi.mocked(api.endSession).mock.calls.filter(([id]) => id === 's2')).toHaveLength(1)
    wrapper.unmount()
  })

  test('a refreshed canonical identity updates the route without starting another session', async () => {
    const { wrapper, router } = await open()
    vi.mocked(api.itemQuery).mockResolvedValueOnce({ ...recovered(), id: 'canonical' } as never)
    wrapper
      .findComponent(Picture)
      .vm.$emit('restart', 'heat', session('s2', { source_id: 99 }), 0, choice)
    await flushPromises()
    expect(router.currentRoute.value.params.id).toBe('canonical')
    expect(wrapper.findComponent(Picture).props('session').session_id).toBe('s2')
    expect(wrapper.findComponent(Picture).props('item').id).toBe('canonical')
    expect(api.startSession).toHaveBeenCalledTimes(1)
    wrapper.unmount()
  })

  test('unmount releases a pending recovery before its metadata request completes', async () => {
    const { wrapper } = await open()
    const slow = held(recovered())
    vi.mocked(api.itemQuery).mockReturnValueOnce(slow.promise as never)
    wrapper
      .findComponent(Picture)
      .vm.$emit('restart', 'heat', session('s2', { source_id: 99 }), 0, choice)
    await flushPromises()
    wrapper.unmount()
    expect(api.endSession).toHaveBeenCalledWith('s2', { keepalive: true })
    slow.settle()
    await flushPromises()
    expect(vi.mocked(api.endSession).mock.calls.filter(([id]) => id === 's2')).toHaveLength(1)
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
