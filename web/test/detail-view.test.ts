const detailForEnrichment = vi.hoisted(() =>
  vi.fn<(id: string) => Promise<import('../src/api/catalogue-model.ts').ItemDetail>>(),
)
// Layout fixtures exercise the existing playback-capable presentation as well as
// unavailable states. The real catalogue adapter is covered separately and live.

vi.mock('../src/api/catalogue.ts', () => ({
  listItems: vi.fn(),
  listArtists: vi.fn(),
  artistAlbums: vi.fn(),
  upNext: vi.fn(),
  catalogueDetail: vi.fn(),
  catalogueChildren: vi.fn(),
}))
/// The item pages, mounted. UI-13 is the shape of this file: three failures
/// live on an item page and they are three different things, and one `error`
/// state doing two of those jobs is what put "Could not load this item" over
/// an item that had loaded perfectly and a Play that had been refused.

import { QueryClient, VueQueryPlugin } from '@tanstack/vue-query'
import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { defineComponent, h, ref } from 'vue'
import { createMemoryHistory, createRouter } from 'vue-router'

import { ApiError } from '../src/api/errors.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  libraries: vi.fn(),
  enrichmentDetail: vi.fn(),
  enrichmentIdentities: vi.fn().mockResolvedValue([]),
  getEnrichmentArtworkUrl: (id: string) => `/api/v1/catalogue/collection-items/${id}/artwork`,
  enrichmentSearch: vi.fn(),
  enrichmentCorrect: vi.fn(),
  catalogueSetWatched: vi.fn(),
  adminItemLog: vi.fn(),
  getPrefs: vi.fn(),
  putPref: vi.fn(),
  catalogueSubtitleSearch: vi.fn(),
  catalogueSubtitleDownload: vi.fn(),
  catalogueSubtitleDelete: vi.fn(),
  getCatalogueArtworkUrl: (library: string, id: string) =>
    `/api/v1/catalogue/libraries/${library}/items/${id}/artwork`,
}))
const admin = { value: false }
vi.mock('../src/api/session.ts', () => ({ whoAmI: () => ({ username: 'me', admin: admin.value }) }))
vi.mock('../src/api/capabilities.ts', () => ({
  buildProfile: () => ({ containers: ['mp4'] }),
  loadMask: vi.fn(() => ({})),
}))

const {
  adminItemLog,
  enrichmentDetail,
  enrichmentIdentities,
  listItems,
  enrichmentSearch,
  enrichmentCorrect,
  getPrefs,
  catalogueChildren,
  catalogueDetail,
  catalogueSetWatched,
  libraries,
  catalogueSubtitleDelete,
  catalogueSubtitleDownload,
  catalogueSubtitleSearch,
} = await import('./api-fixture.ts')
const { loadMask } = await import('../src/api/capabilities.ts')
const { notice, clearNotices } = await import('../src/composables/notices.ts')
const { clearQueue, useQueue } = await import('../src/composables/queue.ts')
const queue = useQueue()
const Detail = (await import('../src/views/Detail.vue')).default
const Season = (await import('../src/views/Season.vue')).default

const film = (over: Record<string, unknown> = {}) => {
  const detail = {
    id: 'heat',
    library_id: 'films',
    kind: 'movie',
    title: 'Heat',
    year: 1995,
    played: false,

    art_version: null,
    duration_ms: 170 * 60_000,
    resume_position_ms: null,
    resume_duration_ms: null,
    parent_id: null,
    show_title: null,
    season: null,
    episode: null,
    episode_end: null,
    metadata: null,
    negotiated: null,
    copies: [
      {
        id: 'heat-copy',
        match_confidence: null as string | null,
        title: 'Heat',
        year: 1995,
        collection_id: 'c',
        paths: ['Heat.mkv'],
        assignment: { revision: 1, library_item_ids: ['heat'] },
      },
    ],
    sources: [
      {
        collection_item_id: 'heat-copy',
        available: true,
        collection_id: 'c',
        module_id: 'm',
        part: 1,
        parts: 1,
        path_rel: 'Heat.mkv',
        revision: 1,
        size: 8 * 1024 ** 3,
        source_id: 1,
        streams: null,
      },
    ],
    ...over,
  }
  return {
    ...detail,
    subtitle_source: {
      media_entry_id: String(
        (detail.negotiated as { source?: { source_id?: number } } | null)?.source?.source_id ??
          detail.sources[0]?.source_id ??
          1,
      ),
      source_version: 'fixture',
    },
  }
}

const episode = (n: number, over: Record<string, unknown> = {}) => ({
  id: `e${n}`,
  library_id: 'films',
  kind: 'episode',
  title: `Episode ${n}`,
  season: 1,
  proj_season: null,
  episode: n,
  proj_episode: null,
  episode_end: null,
  played: false,
  art_version: null,
  resume_position_ms: null,
  resume_duration_ms: null,
  parent_id: 'show',
  ...over,
})

function pages(at: string) {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/library/:library', name: 'library', component: { template: '<div />' } },
      {
        path: '/library/:library/artist/:artist',
        name: 'artist',
        component: { template: '<div />' },
      },
      {
        path: '/library/:library/artist/:artist/item/:id',
        name: 'artist-album',
        component: Detail,
      },
      { path: '/library/:library/item/:id', name: 'detail', component: Detail },
      { path: '/library/:library/item/:id/season/:season', name: 'season', component: Season },
      {
        path: '/library/:library/item/:id/play',
        name: 'player',
        component: { template: '<div />' },
      },
    ],
  })
  return { router, at }
}

async function open(view: typeof Detail | typeof Season, at: string) {
  const { router } = pages(at)
  await router.push(at)
  await router.isReady()
  const wrapper = mount(view, {
    global: {
      plugins: [
        router,
        [
          VueQueryPlugin,
          { queryClient: new QueryClient({ defaultOptions: { queries: { retry: false } } }) },
        ] as [typeof VueQueryPlugin, { queryClient: QueryClient }],
      ],
    },
  })
  await flushPromises()
  return { router, wrapper }
}

beforeEach(() => {
  vi.mocked(enrichmentIdentities).mockResolvedValue([])
  admin.value = false
  vi.mocked(detailForEnrichment).mockResolvedValue(film() as never)
  vi.mocked(enrichmentDetail).mockImplementation(async (id) => {
    const current = await detailForEnrichment('heat')
    const copy = current.copies.find((c) => c.id === id)!
    return {
      input: {
        item_id: id,
        library_item_id: current.id,
        title: copy.title,
        year: copy.year,
        revision: copy.assignment.revision,
        manual: copy.match_confidence === 'manual' || current.kind === 'series',
        media_type: 'movies',
        mediahost_id: copy.module_id,
        remote_id: copy.collection_id,
        selected: ['record', { title: current.title, provider: 'tmdb' }],
        sources: copy.paths.map((path, i) => ({ file_id: String(i), root_token: '', path })),
      },
      candidates: [],
      metadata: { description: {} },
      entries: [],
    } as never
  })
  vi.mocked(listItems).mockResolvedValue({ items: [] } as never)
  vi.mocked(enrichmentSearch).mockResolvedValue({ candidates: [] } as never)
  vi.mocked(enrichmentCorrect).mockResolvedValue({ library_item_ids: ['heat'] } as never)
  vi.mocked(catalogueDetail).mockResolvedValue(film() as never)
  vi.mocked(catalogueChildren).mockResolvedValue({ children: [] } as never)
  vi.mocked(catalogueSetWatched).mockResolvedValue({ updated: 1 } as never)
  vi.mocked(loadMask).mockReturnValue({})
  vi.mocked(libraries).mockResolvedValue([
    { id: 'films', name: 'Films', media_type: 'movies' },
  ] as never)
  vi.mocked(getPrefs).mockResolvedValue({ prefs: [] } as never)
  vi.mocked(catalogueSubtitleSearch).mockResolvedValue({
    candidates: [],
    quota: { remaining: null, total: null, resets_in_secs: null, per_account: false },
  } as never)
  vi.mocked(catalogueSubtitleDelete).mockResolvedValue({ removed: true } as never)
  clearNotices()
  clearQueue()
})
afterEach(() => vi.resetAllMocks())

describe('a film', () => {
  test('says what it is and offers to play it', async () => {
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.find('h1').text()).toContain('Heat')
    expect(wrapper.text()).toContain('1995')
    expect(wrapper.findAll('button').some((b) => b.text().includes('Play'))).toBe(true)
  })

  test('resumes where it was left, and offers the start as well', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(
      film({ resume_position_ms: 300, resume_duration_ms: 1200 }) as never,
    )
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.findAll('button').some((b) => b.text().includes('Resume'))).toBe(true)
    expect(wrapper.findAll('button').some((b) => b.text().includes('from start'))).toBe(true)
  })

  test('and once it is nearly over, Play starts it again', async () => {
    // Resuming into the credits is not resuming.
    vi.mocked(catalogueDetail).mockResolvedValue(
      film({ resume_position_ms: 1180, resume_duration_ms: 1200 }) as never,
    )
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.findAll('button').some((b) => b.text().includes('Resume'))).toBe(false)
    expect(wrapper.findAll('button').some((b) => b.text().includes('from start'))).toBe(false)
  })

  test('an offline file cannot be played', async () => {
    const offline = film()
    offline.sources[0]!.available = false
    vi.mocked(catalogueDetail).mockResolvedValue(offline as never)
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    const play = wrapper.findAll('button').find((b) => b.text().includes('Play'))!
    expect(play.attributes('disabled')).toBeDefined()
    expect(wrapper.text()).toContain('offline')
  })

  test('pressing Play goes to the player, under this library', async () => {
    const { router, wrapper } = await open(Detail, '/library/films/item/heat')
    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('Play'))!
      .trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/library/films/item/heat/play')
  })

  test('pressing a chapter starts there', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(
      film({
        chapters: [
          { start_ms: 0, title: 'Opening' },
          { start_ms: 470_512, title: 'The heist' },
          { start_ms: 1_800_000 },
        ],
      }) as never,
    )
    const { router, wrapper } = await open(Detail, '/library/films/item/heat')
    // A chapter with no name is still somewhere to jump to.
    expect(wrapper.text()).toContain('7:50')
    expect(wrapper.text()).toContain('Chapter 3')

    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('The heist'))!
      .trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/library/films/item/heat/play')
    expect(router.currentRoute.value.query.start).toBe('470512')
  })

  test('no chapters, no section', async () => {
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.text()).not.toContain('CHAPTERS')
    expect(wrapper.text().toLowerCase()).not.toContain('chapters')
  })
})

describe('what the hub says it would do with the file', () => {
  test('names the work, and every stream’s verdict', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(
      film({
        negotiated: {
          cost: 'audio_encode',
          mode: 'remux',
          source: null,
          streams: { video: 'copy', audio: 'dts → aac (transcoded) — 7.1 → 5.1' },
          subtitles: [],
          target_duration_secs: 6,
        },
      }) as never,
    )
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.text()).toContain('TRANSCODE')
    // The chip cannot describe two streams, so the rows do.
    expect(wrapper.text()).toContain('copy')
    expect(wrapper.text()).toContain('7.1 → 5.1')
  })

  test('and nothing at all when the hub did not say', async () => {
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.text()).not.toContain('Playback plan')
  })
})

describe('choosing a playback source', () => {
  test('default subtitle downloads follow the same preferred-audio source as Play', async () => {
    const base = film().sources[0]!
    vi.mocked(getPrefs).mockResolvedValue({
      prefs: [{ scope: '', key: 'audio.movies', value: 'jpn' }],
    } as never)
    vi.mocked(catalogueDetail).mockImplementation(async (_library, _id, body) => {
      const selected = body?.source_id ?? (body?.source_audio_tracks?.['1'] === 1 ? 2 : 1)
      return film({
        sources: [
          {
            ...base,
            path_rel: 'Preview.mkv',
            streams: {
              video: [],
              audio: [
                { language: 'eng', codec: 'aac' },
                { language: 'jpn', codec: 'dts' },
              ],
            },
          },
          {
            ...base,
            source_id: 2,
            collection_item_id: 'preferred-copy',
            path_rel: 'Preferred.mkv',
            streams: {
              video: [],
              audio: [
                { language: 'eng', codec: 'dts' },
                { language: 'jpn', codec: 'aac' },
              ],
            },
          },
        ],
        negotiated: {
          source: { source_id: selected },
          cost: 'copy',
          streams: { video: 'copy', audio: 'copy' },
          subtitles: [],
        },
      }) as never
    })
    vi.mocked(catalogueSubtitleSearch).mockResolvedValue({
      candidates: [
        { file_id: 'preferred-sub', language: 'eng', release_name: 'Preferred release' },
      ],
      quota: null,
    } as never)
    vi.mocked(catalogueSubtitleDownload).mockResolvedValue({ track_id: 9, quota: null } as never)
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.find('#playback-source option').text()).toContain('Preferred.mkv')
    expect(wrapper.find('#subtitle-source option').text()).toContain('Preferred.mkv')
    await wrapper
      .findAll('button')
      .find((button) => button.text() === 'Find subtitles online')!
      .trigger('click')
    await flushPromises()
    expect(catalogueSubtitleSearch).toHaveBeenLastCalledWith(expect.any(String), 'heat', {
      languages: [],
      source: { media_entry_id: '2', source_version: 'fixture' },
    })
    await wrapper
      .findAll('button')
      .find((button) => button.text() === 'Download')!
      .trigger('click')
    await flushPromises()
    expect(catalogueSubtitleDownload).toHaveBeenLastCalledWith(expect.any(String), 'heat', {
      file_id: 'preferred-sub',
      language: 'eng',
      source: { media_entry_id: '2', source_version: 'fixture' },
    })
    wrapper.unmount()
  })

  test('a subtitle download target is not offered before playback preferences arrive', async () => {
    let answer!: () => void
    vi.mocked(getPrefs).mockReturnValue(
      new Promise((resolve) => {
        answer = () => resolve({ prefs: [] } as never)
      }),
    )
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    const search = wrapper
      .findAll('button')
      .find((button) => button.text() === 'Find subtitles online')
    expect(!search || search.attributes('disabled') !== undefined).toBe(true)
    answer()
    await flushPromises()
    expect(
      wrapper.findAll('button').some((button) => button.text() === 'Find subtitles online'),
    ).toBe(true)
    wrapper.unmount()
  })

  const rendition = (selected = 2) => {
    const base = film().sources[0]!
    return film({
      sources: [
        { ...base, source_id: 1, parts: 2, path_rel: 'Heat CD1.avi' },
        { ...base, source_id: 1, parts: 2, part: 2, path_rel: 'Heat CD2.avi' },
        { ...base, source_id: 2, path_rel: 'Heat 1080p.mkv' },
        { ...base, source_id: 3, available: false, path_rel: 'Offline.mkv' },
        { ...base, source_id: 4, parts: 2, path_rel: 'Incomplete CD1.avi' },
      ],
      negotiated: {
        source: { source_id: selected },
        cost: selected === 1 ? 'video_encode' : 'copy',
        streams: { video: selected === 1 ? 'encode' : 'copy', audio: 'copy' },
        subtitles: [],
      },
      chapters: [{ start_ms: selected === 1 ? 45_000 : 60_000, title: 'The heist' }],
      resume_position_ms: 90_000,
      resume_duration_ms: 600_000,
    })
  }
  const playButton = (wrapper: Awaited<ReturnType<typeof open>>['wrapper']) =>
    wrapper.findAll('button').find((button) => button.text() === '▶ Resume')!

  beforeEach(() => {
    vi.mocked(catalogueDetail).mockImplementation(
      async (_library, id, body) => ({ ...rendition(body?.source_id ?? 2), id }) as never,
    )
  })

  test('defaults to the hub choice and groups CDs into one selectable copy', async () => {
    const { wrapper, router } = await open(Detail, '/library/films/item/heat')
    const options = wrapper.find('#playback-source').findAll('option')
    expect(options).toHaveLength(5)
    expect(options[0]!.text()).toContain('Automatic · m · c · Heat 1080p.mkv')
    expect((options[0]!.element as HTMLOptionElement).selected).toBe(true)
    expect(options[1]!.text()).toContain('Heat CD1.avi + Heat CD2.avi')
    expect(options[3]!.attributes('disabled')).toBeDefined()
    expect(options[4]!.attributes('disabled')).toBeDefined()
    await playButton(wrapper).trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.query.source).toBeUndefined()
  })

  test('offline encodes remain selectable for subtitle lookup without changing playback', async () => {
    const offline = rendition()
    offline.sources = offline.sources.slice(0, 3).map((source) => ({ ...source, available: false }))
    offline.negotiated = null
    vi.mocked(catalogueDetail).mockResolvedValue(offline as never)
    vi.mocked(catalogueSubtitleSearch).mockResolvedValue({
      candidates: [{ file_id: 'offline-sub', language: 'eng', release_name: 'Heat 1080p' }],
      quota: null,
    } as never)
    vi.mocked(catalogueSubtitleDownload).mockResolvedValue({ track_id: 9, quota: null } as never)
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(playButton(wrapper).attributes('disabled')).toBeDefined()
    expect((wrapper.find('#playback-source option').element as HTMLOptionElement).selected).toBe(
      true,
    )
    const subtitleOptions = wrapper.find('#subtitle-source').findAll('option')
    expect(subtitleOptions).toHaveLength(3)
    expect(subtitleOptions.every((option) => option.attributes('disabled') === undefined)).toBe(
      true,
    )
    await wrapper.find('#subtitle-source').setValue('2')
    await flushPromises()
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Find subtitles online')!
      .trigger('click')
    await flushPromises()
    expect(catalogueSubtitleSearch).toHaveBeenLastCalledWith(expect.any(String), 'heat', {
      languages: [],
      source: { media_entry_id: '2', source_version: 'fixture' },
    })
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Download')!
      .trigger('click')
    await flushPromises()
    expect(catalogueSubtitleDownload).toHaveBeenLastCalledWith(expect.any(String), 'heat', {
      file_id: 'offline-sub',
      language: 'eng',
      source: { media_entry_id: '2', source_version: 'fixture' },
    })
    expect((wrapper.find('#playback-source option').element as HTMLOptionElement).selected).toBe(
      true,
    )
    expect(playButton(wrapper).attributes('disabled')).toBeDefined()
    expect(wrapper.text()).toContain('Subtitle listing is unavailable for this source.')
    wrapper.unmount()
  })

  test('a subtitle override leaves automatic playback on its negotiated source', async () => {
    const { wrapper, router } = await open(Detail, '/library/films/item/heat')
    await wrapper.find('#subtitle-source').setValue('1')
    await flushPromises()
    expect(catalogueDetail).toHaveBeenLastCalledWith(
      expect.any(String),
      'heat',
      expect.objectContaining({ source_id: 1 }),
    )
    expect(wrapper.text()).toContain('REMUX')
    expect(wrapper.text()).not.toContain('TRANSCODE')
    await playButton(wrapper).trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.query.source).toBeUndefined()
    wrapper.unmount()
  })

  test.each(['resume', 'start', 'chapter'] as const)(
    'uses the override for %s and updates the preview',
    async (action) => {
      const { wrapper, router } = await open(Detail, '/library/films/item/heat')
      await wrapper.find('#playback-source').setValue('1')
      await flushPromises()
      expect(catalogueDetail).toHaveBeenLastCalledWith(
        expect.any(String),
        'heat',
        expect.objectContaining({ source_id: 1 }),
      )
      expect(wrapper.text()).toContain('TRANSCODE')
      expect(wrapper.text()).not.toContain('REMUX')
      const button =
        action === 'resume'
          ? playButton(wrapper)
          : wrapper
              .findAll('button')
              .find((b) =>
                action === 'start'
                  ? b.text() === 'Play from start'
                  : b.text().includes('The heist'),
              )!
      await button.trigger('click')
      await flushPromises()
      expect(router.currentRoute.value.query.source).toBe('1')
      expect(router.currentRoute.value.query.start).toBe(
        action === 'resume' ? undefined : action === 'start' ? '0' : '45000',
      )
      expect(router.currentRoute.value.query.chapter).toBe(action === 'chapter' ? '1' : undefined)
    },
  )

  test('a pending or failed source check keeps the page and allows returning to automatic', async () => {
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    let fail!: (error: Error) => void
    vi.mocked(catalogueDetail).mockImplementationOnce(
      () =>
        new Promise((_resolve, reject) => {
          fail = reject
        }),
    )
    await wrapper.find('#playback-source').setValue('1')
    await flushPromises()
    expect(wrapper.text()).toContain('Checking this source')
    expect(playButton(wrapper).attributes('disabled')).toBeDefined()
    expect(wrapper.text()).not.toContain('Playback plan')
    fail(new Error('hub unavailable'))
    await flushPromises()
    expect(wrapper.find('h1').text()).toContain('Heat')
    expect(wrapper.text()).toContain('Could not check this source')
    expect(playButton(wrapper).attributes('disabled')).toBeDefined()
    await wrapper.find('#playback-source').setValue('Automatic · m · c · Heat 1080p.mkv · 8.0 GB')
    await flushPromises()
    expect(wrapper.text()).not.toContain('Could not check this source')
    expect(playButton(wrapper).attributes('disabled')).toBeUndefined()
  })

  test('changing items resets the override without writing preferences', async () => {
    const { wrapper, router } = await open(Detail, '/library/films/item/heat')
    await wrapper.find('#playback-source').setValue('1')
    await flushPromises()
    await router.push('/library/films/item/another')
    await flushPromises()
    expect(catalogueDetail).toHaveBeenLastCalledWith(
      expect.any(String),
      'another',
      expect.not.objectContaining({ source_id: 1 }),
    )
    expect((wrapper.find('#playback-source option').element as HTMLOptionElement).selected).toBe(
      true,
    )
    expect((await import('./api-fixture.ts')).putPref).not.toHaveBeenCalled()
  })

  test('returning to automatic during subtitle search cannot reuse the override’s results', async () => {
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    await wrapper.find('#playback-source').setValue('1')
    await flushPromises()
    let finish!: (value: unknown) => void
    vi.mocked(catalogueSubtitleSearch).mockReturnValueOnce(
      new Promise((resolve) => {
        finish = resolve
      }) as never,
    )
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Find subtitles online')!
      .trigger('click')
    await flushPromises()
    expect(catalogueSubtitleSearch).toHaveBeenLastCalledWith(expect.any(String), 'heat', {
      languages: [],
      source: { media_entry_id: '1', source_version: 'fixture' },
    })
    await wrapper.find('#playback-source').setValue('Automatic · m · c · Heat 1080p.mkv · 8.0 GB')
    await flushPromises()
    finish({
      candidates: [{ file_id: 'wrong-version', language: 'eng', release_name: 'CD release' }],
      quota: null,
    })
    await flushPromises()
    expect(wrapper.find('[role="dialog"]').exists()).toBe(false)
    expect(wrapper.text()).not.toContain('CD release')
    wrapper.unmount()
  })
})

describe('the files it is made of', () => {
  test('identical paths on different hosts remain identifiable in every source control', async () => {
    admin.value = true
    const base = film()
    const detail = film({
      sources: ['a', 'b'].map((suffix, at) => ({
        ...base.sources[0],
        source_id: at + 1,
        module_id: `host-${suffix}`,
        collection_id: 'movies',
        collection_item_id: `copy-${suffix}`,
        path_rel: 'Heat (1995).mkv',
      })),
      copies: ['a', 'b'].map((suffix) => ({
        ...base.copies[0],
        id: `copy-${suffix}`,
        module_id: `host-${suffix}`,
        collection_id: 'movies',
        paths: ['Heat (1995).mkv'],
      })),
      negotiated: {
        source: { source_id: 1 },
        cost: 'copy',
        streams: { video: 'copy', audio: 'copy' },
        subtitles: [],
      },
    })
    vi.mocked(catalogueDetail).mockResolvedValue(detail as never)
    vi.mocked(detailForEnrichment).mockResolvedValue(detail as never)
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    for (const selector of ['#playback-source', '#subtitle-source']) {
      const choices = wrapper.findAll(`${selector} option`)
      expect(choices.find((option) => option.attributes('value') === '1')!.text()).toContain(
        'host-a · movies',
      )
      expect(choices.find((option) => option.attributes('value') === '2')!.text()).toContain(
        'host-b · movies',
      )
    }
    const buttons = wrapper.findAll('button[title="Search metadata for this source"]')
    expect(buttons[0]!.attributes('aria-label')).toContain('host-a · movies')
    expect(buttons[1]!.attributes('aria-label')).toContain('host-b · movies')
    expect(buttons[1]!.element.closest('li')!.textContent).toContain('host-b · movies')
    await buttons[1]!.trigger('click')
    await flushPromises()
    expect(wrapper.find('[role="dialog"]').text()).toContain('host-b · movies')
    wrapper.unmount()
  })

  test('a magnifier targets its source copy and keeps its CDs together', async () => {
    admin.value = true
    const detail = film()
    detail.sources = [1, 2].map((part) => ({
      ...detail.sources[0]!,
      part,
      parts: 2,
      path_rel: `Heat CD${part}.avi`,
    }))
    detail.sources.push({
      ...detail.sources[0]!,
      source_id: 2,
      collection_item_id: 'other-copy',
      collection_id: 'other',
      part: 1,
      parts: 1,
      path_rel: 'Heat CD1.avi',
    })
    detail.copies = [
      { ...detail.copies[0]!, paths: ['Heat CD1.avi', 'Heat CD2.avi'], match_confidence: 'weak' },
      {
        ...detail.copies[0]!,
        id: 'other-copy',
        match_confidence: 'manual',
        collection_id: 'other',
        paths: ['Heat CD1.avi'],
        assignment: { revision: 7, library_item_ids: ['heat'] },
      },
    ]
    vi.mocked(catalogueDetail).mockResolvedValue(detail as never)
    vi.mocked(detailForEnrichment).mockResolvedValue(detail as never)
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    const buttons = wrapper.findAll('button[title="Search metadata for this source"]')
    expect(buttons).toHaveLength(2)
    expect(buttons[0]!.classes()).toContain('text-sand')
    expect(buttons[1]!.classes()).toContain('text-dim')
    expect(buttons[1]!.classes()).not.toContain('opacity-0')
    expect(buttons[0]!.find('svg').exists()).toBe(true)
    expect(buttons[0]!.element.closest('li')!.textContent).toContain('Heat CD2.avi')
    expect(wrapper.text()).not.toContain('Match collection copy')
    await buttons[1]!.trigger('click')
    await flushPromises()
    const dialog = wrapper.find('[role="dialog"]')
    expect(dialog.find('#match-copy').exists()).toBe(false)
    expect(dialog.text()).toContain('other')
    expect(dialog.text()).not.toContain('Heat CD2.avi')
    expect(enrichmentDetail).toHaveBeenCalledWith('other-copy')
    await dialog
      .findAll('button')
      .find((b) => b.text() === 'Reject current')!
      .trigger('click')
    await flushPromises()
    expect(enrichmentCorrect).toHaveBeenCalledWith(
      'other-copy',
      expect.objectContaining({ revision: 7, action: 'reject' }),
    )
    wrapper.unmount()
  })

  test('source matching is hidden from non-admins', async () => {
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.find('button[title="Search metadata for this source"]').exists()).toBe(false)
  })

  test('series copies have their own source actions too', async () => {
    admin.value = true
    vi.mocked(catalogueDetail).mockResolvedValue(film({ kind: 'series', sources: [] }) as never)
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.findAll('button[title="Search metadata for this source"]')).toHaveLength(1)
  })

  test('parts of one work are one entry, not one each', async () => {
    // UI-27: a film in seven numbered parts read as seven alternative encodes.
    const multi = film()
    multi.sources = [1, 2, 3].map((part) => ({
      ...multi.sources[0]!,
      part,
      parts: 3,
      path_rel: `Heat.part${part}.mkv`,
      source_id: 1,
    }))
    vi.mocked(catalogueDetail).mockResolvedValue(multi as never)
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.text()).toContain('Source')
    expect(wrapper.text()).toContain('3 parts')
    expect(wrapper.text()).toContain('Heat.part1.mkv')
    expect(wrapper.text()).toContain('Heat.part2.mkv')
    expect(wrapper.text()).toContain('Heat.part3.mkv')
    expect(wrapper.findAll('li')).toHaveLength(1)
  })

  test('and a work missing a part says so', async () => {
    const missing = film()
    missing.sources = [1, 2].map((part) => ({
      ...missing.sources[0]!,
      part,
      parts: 3,
      path_rel: `Heat.part${part}.mkv`,
      source_id: 1,
    }))
    vi.mocked(catalogueDetail).mockResolvedValue(missing as never)
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.text()).toContain('incomplete')
  })
})

describe('when something goes wrong', () => {
  test('the item failing takes the screen, with a way out', async () => {
    // There is no page without it, and a page you can only leave by editing
    // the URL is a dead end.
    vi.mocked(catalogueDetail).mockRejectedValue(new ApiError(503, 'the hub is restarting'))
    const { router, wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.text()).toContain('Could not load this item.')
    expect(wrapper.text()).toContain('restarting')

    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Back to library')!
      .trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/library/films')
  })

  test('the episodes failing is a line, not the screen', async () => {
    // The head is real and already on screen: the title, the poster and the
    // way back are all in hand.
    vi.mocked(catalogueDetail).mockResolvedValue(film({ kind: 'series', id: 'show' }) as never)
    vi.mocked(catalogueChildren).mockRejectedValue(new ApiError(500, 'no'))
    const { wrapper } = await open(Detail, '/library/films/item/show')
    expect(wrapper.find('h1').text()).toContain('Heat')
    expect(wrapper.text()).toContain('Could not load the episodes')
    expect(wrapper.text()).not.toContain('Could not load this item')
  })

  test('and a mark that would not stick is a notice, not either of those', async () => {
    // The page is intact and you are still looking at it — and the control
    // that caused it is right there, so pressing it again IS the retry.
    vi.mocked(catalogueSetWatched).mockRejectedValue(new ApiError(500, 'nope'))
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('Mark watched'))!
      .trigger('click')
    await flushPromises()
    expect(notice.value).toContain('Could not change the watched mark')
    expect(wrapper.find('h1').exists()).toBe(true)
  })
})

describe('a series', () => {
  const show = () => film({ kind: 'series', id: 'show', title: 'Fringe', duration_ms: null })

  test('a same-ID match refreshes the reconciled episode grouping', async () => {
    admin.value = true
    const detail = { ...show(), sources: [] }
    vi.mocked(catalogueDetail).mockResolvedValue(detail as never)
    vi.mocked(detailForEnrichment).mockResolvedValue(detail as never)
    vi.mocked(catalogueChildren).mockResolvedValue({ children: [episode(1)] } as never)
    vi.mocked(enrichmentCorrect).mockImplementation(async () => {
      vi.mocked(catalogueChildren).mockResolvedValue({
        children: [episode(1, { season: 2, title: 'Corrected episode' })],
      } as never)
      return { library_item_ids: ['show'] } as never
    })
    const { router, wrapper } = await open(Detail, '/library/shows/item/show')
    expect(wrapper.text()).toContain('Season 1')
    await wrapper.find('button[title="Search metadata for this source"]').trigger('click')
    await flushPromises()
    await wrapper
      .findAll('[role="dialog"] button')
      .find((button) => button.text() === 'Use automatic matching')!
      .trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.params.id).toBe('show')
    expect(catalogueChildren).toHaveBeenCalledTimes(2)
    expect(wrapper.text()).toContain('Season 2')
    expect(wrapper.text()).not.toContain('Season 1')
    wrapper.unmount()
  })

  test('counts its episodes, and says where to carry on', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockResolvedValue({
      children: [episode(1, { played: true }), episode(2), episode(3)],
    } as never)
    const { wrapper } = await open(Detail, '/library/shows/item/show')
    expect(wrapper.text()).toContain('3 episodes · 1 watched')
    expect(wrapper.text()).toContain('Continue · S01E02')
  })

  test('and says nothing about where to carry on until the list answers', async () => {
    // "Start from the beginning" is the wrong answer to "we have not asked
    // yet", and it flashed in as the list arrived.
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockReturnValue(new Promise(() => {}) as never)
    const { wrapper } = await open(Detail, '/library/shows/item/show')
    expect(wrapper.find('h1').text()).toContain('Fringe')
    expect(wrapper.text()).not.toContain('Continue')
    expect(wrapper.text()).not.toContain('episodes ·')
  })

  test('and it is numbered the way the list under it is', async () => {
    // Reading the native fields here put "Continue · E10" above a row reading
    // "S01E10", and on a show whose projection spans seasons the two numbers
    // are not even close.
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockResolvedValue({
      children: [
        episode(1, { season: null, proj_season: 1, proj_episode: 1, played: true }),
        episode(26, { season: null, proj_season: 2, proj_episode: 1 }),
      ],
    } as never)
    const { wrapper } = await open(Detail, '/library/shows/item/show')
    expect(wrapper.text()).toContain('Continue · S02E01')
    expect(wrapper.text()).not.toContain('Continue · E26')
  })

  test('a season heading opens the season', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockResolvedValue({ children: [episode(1)] } as never)
    const { router, wrapper } = await open(Detail, '/library/shows/item/show')
    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('Season 1'))!
      .trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/library/shows/item/show/season/1')
  })

  test('one press marks a whole season, naming its episodes', async () => {
    // WHICH episodes are in it is decided here, because the season a viewer
    // sees can be a projection of absolute numbering — the hub would guess.
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockResolvedValue({
      children: [episode(1), episode(2)],
    } as never)
    const { wrapper } = await open(Detail, '/library/shows/item/show')
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Mark season watched')!
      .trigger('click')
    await flushPromises()
    expect(catalogueSetWatched).toHaveBeenCalledWith(expect.any(String), 'show', {
      played: true,
      items: ['e1', 'e2'],
    })
  })

  test('and ticking one episode is its own control, not the row', async () => {
    // A button within a button is invalid, and a click that both ticked the
    // episode and opened it would be neither.
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockResolvedValue({ children: [episode(1)] } as never)
    const { router, wrapper } = await open(Detail, '/library/shows/item/show')
    const tick = wrapper.find('[aria-label^="Mark as watched: Episode 1"]')
    expect(tick.exists()).toBe(true)

    await tick.trigger('click')
    await flushPromises()
    expect(catalogueSetWatched).toHaveBeenCalledWith(expect.any(String), 'e1', { played: true })
    // And it did not navigate.
    expect(router.currentRoute.value.path).toBe('/library/shows/item/show')
  })
})

describe('a season', () => {
  const show = () => film({ kind: 'series', id: 'show', title: 'Fringe', duration_ms: null })

  test.each([null, 60_000])(
    'plays the negotiated online copy when the first copy is offline (resume %s)',
    async (resume) => {
      const base = film().sources[0]!
      const detail = film({
        ...episode(1),
        resume_position_ms: resume,
        resume_duration_ms: 600_000,
        negotiated: { source: { source_id: 2 } },
        sources: [
          { ...base, available: false },
          { ...base, source_id: 2, collection_item_id: 'online-copy' },
        ],
      })
      vi.mocked(catalogueDetail).mockImplementation(
        async (id) => (id === 'show' ? show() : detail) as never,
      )
      vi.mocked(catalogueChildren).mockResolvedValue({ children: [episode(1)] } as never)
      const { router, wrapper } = await open(Season, '/library/shows/item/show/season/1')
      const play = wrapper
        .findAll('button')
        .find((b) => b.text() === `▶ ${resume ? 'Resume' : 'Play'}`)!
      expect(play.attributes('disabled')).toBeUndefined()
      await play.trigger('click')
      await flushPromises()
      expect(router.currentRoute.value.fullPath).toBe('/library/shows/item/e1/play')
      wrapper.unmount()
    },
  )

  test('an online first copy cannot enable an unavailable negotiated copy', async () => {
    const base = film().sources[0]!
    const detail = film({
      ...episode(1),
      resume_position_ms: 60_000,
      resume_duration_ms: 600_000,
      negotiated: { source: { source_id: 2 } },
      sources: [base, { ...base, source_id: 2, available: false }],
    })
    vi.mocked(catalogueDetail).mockImplementation(
      async (id) => (id === 'show' ? show() : detail) as never,
    )
    vi.mocked(catalogueChildren).mockResolvedValue({ children: [episode(1)] } as never)
    const { wrapper } = await open(Season, '/library/shows/item/show/season/1')
    for (const text of ['▶ Resume', 'Play from start']) {
      expect(
        wrapper
          .findAll('button')
          .find((b) => b.text() === text)!
          .attributes('disabled'),
      ).toBeDefined()
    }
    wrapper.unmount()
  })

  test('shows its episodes as stills', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockResolvedValue({
      children: [episode(1), episode(2)],
    } as never)
    const { wrapper } = await open(Season, '/library/shows/item/show/season/1')
    expect(wrapper.find('h1').text()).toBe('Season 1')
    expect(wrapper.text()).toContain('2 episodes · 0 watched')
    expect(wrapper.text()).toContain('Episode 1')
    // Not the one in season 2.
    expect(wrapper.text()).not.toContain('Episode 3')
  })

  test('a season with nothing in it says so rather than looking broken', async () => {
    // A hand-typed or stale season number renders a heading, an empty strip
    // and two dead arrows.
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockResolvedValue({ children: [episode(1)] } as never)
    const { wrapper } = await open(Season, '/library/shows/item/show/season/9')
    expect(wrapper.text()).toContain('No episodes in season 9')
  })

  test('and absolute numbering is a season of its own, not a missing one', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockResolvedValue({
      children: [episode(11, { season: null })],
    } as never)
    const { wrapper } = await open(Season, '/library/shows/item/show/season/all')
    expect(wrapper.find('h1').text()).toBe('Episodes')
    expect(wrapper.text()).toContain('Episode 11')
  })
})

describe('a mark, and what it costs', () => {
  test('asks for the item and its children again, so no tick can lie', async () => {
    const show = film({ kind: 'series', id: 'show' })
    vi.mocked(catalogueDetail).mockResolvedValue(show as never)
    vi.mocked(catalogueChildren).mockResolvedValue({ children: [episode(1)] } as never)
    const { wrapper } = await open(Detail, '/library/shows/item/show')
    vi.mocked(catalogueChildren).mockClear()
    vi.mocked(catalogueDetail).mockClear()

    await wrapper.find('[aria-label^="Mark as watched"]').trigger('click')
    await flushPromises()
    expect(catalogueChildren).toHaveBeenCalled()
    expect(catalogueDetail).toHaveBeenCalled()
  })

  test('and a re-ask that fails is a notice, not the screen', async () => {
    // The write LANDED. Replacing the page with "Could not load this item"
    // over a successful mark is the incident this whole split exists for.
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    vi.mocked(catalogueDetail).mockRejectedValue(new ApiError(503, 'blip'))

    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('Mark watched'))!
      .trigger('click')
    await flushPromises()

    expect(wrapper.text()).not.toContain('Could not load this item')
    expect(wrapper.find('h1').text()).toContain('Heat')
    expect(notice.value).toContain('re-read')
  })

  test('a tick can be taken back as well as put on', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(film({ played: true }) as never)
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('Watched'))!
      .trigger('click')
    await flushPromises()
    expect(catalogueSetWatched).toHaveBeenCalledWith(expect.any(String), 'heat', { played: false })
  })

  test('and pressing it twice while it is out sends one write', async () => {
    let settle = () => {}
    vi.mocked(catalogueSetWatched).mockReturnValue(
      new Promise((resolve) => (settle = () => resolve({ updated: 1 } as never))) as never,
    )
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    const tick = wrapper.findAll('button').find((b) => b.text().includes('Mark watched'))!
    await tick.trigger('click')
    await tick.trigger('click')
    await flushPromises()
    expect(catalogueSetWatched).toHaveBeenCalledTimes(1)
    settle()
    await flushPromises()
  })
})

describe('what a series page says about an episode', () => {
  const show = () => film({ kind: 'series', id: 'show', duration_ms: null })

  test('how far into it you are', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockResolvedValue({
      children: [episode(1, { resume_position_ms: 300, resume_duration_ms: 1200 })],
    } as never)
    const { wrapper } = await open(Detail, '/library/shows/item/show')
    expect(wrapper.text()).toContain('25% in')
  })

  test('which one is next up', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockResolvedValue({
      children: [episode(1, { played: true }), episode(2)],
    } as never)
    const { wrapper } = await open(Detail, '/library/shows/item/show')
    expect(wrapper.text()).toContain('next up')
  })

  test('and the file’s own number, under a projection', async () => {
    // HUB-31: the projected number is what the viewer navigates by, and the
    // native one is what the filename says.
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockResolvedValue({
      children: [episode(26, { season: null, proj_season: 2, proj_episode: 1 })],
    } as never)
    const { wrapper } = await open(Detail, '/library/shows/item/show')
    expect(wrapper.text()).toContain('#26')
  })

  test('its seasons are headings, so they can be walked as headings', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockResolvedValue({ children: [episode(1)] } as never)
    const { wrapper } = await open(Detail, '/library/shows/item/show')
    expect(wrapper.findAll('h2').some((h) => h.text().includes('Season 1'))).toBe(true)
  })
})

describe('going back up', () => {
  test('an episode goes to its series', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(
      film({ kind: 'episode', id: 'e1', parent_id: 'show', show_title: 'Fringe' }) as never,
    )
    const { router, wrapper } = await open(Detail, '/library/shows/item/e1')
    expect(wrapper.findAll('button')[0]!.text()).toContain('Fringe')
    await wrapper.findAll('button')[0]!.trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/library/shows/item/show')
  })

  test('and everything else goes to the library it was opened from', async () => {
    const { router, wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.findAll('button')[0]!.text()).toContain('Library')
    await wrapper.findAll('button')[0]!.trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/library/films')
  })

  test('an album opened under an Album Artist returns to that artist', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(
      film({ kind: 'album', id: 'album', title: 'Hot Space', artist: 'Queen' }) as never,
    )
    const { router, wrapper } = await open(Detail, '/library/music/artist/queen/item/album')
    expect(wrapper.findAll('button')[0]!.text()).toContain('Queen')
    await wrapper.findAll('button')[0]!.trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/library/music/artist/queen')
  })
})

describe('the files, in detail', () => {
  test('the size is the whole work, not its first part', async () => {
    const multi = film()
    multi.sources = [1, 2].map((part) => ({
      ...multi.sources[0]!,
      part,
      parts: 2,
      size: 2 * 1024 ** 3,
      path_rel: `Heat.part${part}.mkv`,
      source_id: 1,
    }))
    vi.mocked(catalogueDetail).mockResolvedValue(multi as never)
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.text()).toContain('4.0 GB')
  })

  test('one missing part makes the whole work offline', async () => {
    const multi = film()
    multi.sources = [1, 2].map((part) => ({
      ...multi.sources[0]!,
      part,
      parts: 2,
      available: part === 1,
      path_rel: `Heat.part${part}.mkv`,
      source_id: 1,
    }))
    vi.mocked(catalogueDetail).mockResolvedValue(multi as never)
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.text()).toContain('offline')
  })

  test('and a corrected release says which', async () => {
    // Two files of the same work otherwise look like the same file twice.
    const fixed = film()
    fixed.sources[0]!.revision = 2
    vi.mocked(catalogueDetail).mockResolvedValue(fixed as never)
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.text()).toContain('v2')
  })
})

describe('who the metadata came from', () => {
  test('is said, because for TMDB that is a term of use', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(
      film({ provider: 'tmdb', metadata: { overview: null } }) as never,
    )
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.text()).toContain('not endorsed, certified')
    expect(wrapper.find('img[alt="TMDB"]').exists()).toBe(true)
  })

  test('and each provider is credited in its own words', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(
      film({ provider: 'tvdb', metadata: { overview: null } }) as never,
    )
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.text()).toContain('TheTVDB')
  })

  test('and nothing is claimed for a provider nobody named', async () => {
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.find('footer').exists()).toBe(false)
  })
})

describe('a capability mask', () => {
  test('is announced, because the plan above it is not what a real browser would get', async () => {
    // `buildProfile` already applies the mask, so a silent one is the exact
    // trap the badge exists to prevent.
    vi.mocked(loadMask).mockReturnValue({ video: ['hevc'] })
    vi.mocked(catalogueDetail).mockResolvedValue(
      film({
        negotiated: {
          cost: 'direct',
          mode: 'direct',
          source: null,
          streams: { video: 'copy', audio: 'copy' },
          subtitles: [],
          target_duration_secs: 6,
        },
      }) as never,
    )
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.text()).toContain('masked')
  })

  test('and nothing is said when there is none', async () => {
    vi.mocked(loadMask).mockReturnValue({})
    vi.mocked(catalogueDetail).mockResolvedValue(
      film({
        negotiated: {
          cost: 'direct',
          mode: 'direct',
          source: null,
          streams: { video: 'copy', audio: 'copy' },
          subtitles: [],
          target_duration_secs: 6,
        },
      }) as never,
    )
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.text()).not.toContain('masked')
  })
})

describe('what this item is connected to', () => {
  test('is listed, and a row in the library is a way there', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(
      film({
        related: [
          { kind: 'sequel', title: 'Heat 2', item_id: 'heat2' },
          { kind: 'remake_of', title: 'L.A. Takedown', item_id: null },
        ],
      }) as never,
    )
    const { router, wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.text()).toContain('Heat 2')
    // One that is not in the library says so rather than offering a link to
    // nothing.
    expect(wrapper.text()).toContain('not in library')

    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Heat 2')!
      .trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/library/films/item/heat2')
  })
})

describe('a record', () => {
  const record = async (tracks = [episode(1, { kind: 'song', title: 'Staying Power' })]) => {
    vi.mocked(catalogueDetail).mockResolvedValue(
      film({ kind: 'album', id: 'album', title: 'Hot Space', artist: 'Queen' }) as never,
    )
    vi.mocked(catalogueChildren).mockResolvedValue({ children: tracks } as never)
    return open(Detail, '/library/music/item/album')
  }

  test('lists its tracks', async () => {
    const { wrapper } = await record()
    expect(wrapper.text()).toContain('Staying Power')
    expect(wrapper.text()).toContain('1 track')
  })

  test('does not offer video subtitle management', async () => {
    const { wrapper } = await record()
    expect(wrapper.findAll('h2').some((heading) => heading.text() === 'Subtitles')).toBe(false)
    expect(
      wrapper.findAll('button').some((button) => button.text().startsWith('Find subtitles')),
    ).toBe(false)
  })

  test('a multi-disc release can play or append either disc on its own', async () => {
    const { wrapper } = await record([
      episode(1, { kind: 'song', season: 1, title: 'Disc one, track one' }),
      episode(2, { kind: 'song', season: 1, title: 'Disc one, track two' }),
      episode(1, { id: 'd2t1', kind: 'song', season: 2, title: 'Disc two, track one' }),
    ])

    expect(wrapper.text()).toContain('Disc 1')
    expect(wrapper.text()).toContain('Disc 2')
    await wrapper
      .findAll('button')
      .find((button) => button.text() === 'Add disc 2 to queue')!
      .trigger('click')
    expect(queue.queue.value.entries.map((entry) => entry.track.title)).toEqual([
      'Disc two, track one',
    ])

    await wrapper
      .findAll('button')
      .find((button) => button.text() === '▶ Play disc 1')!
      .trigger('click')
    expect(queue.queue.value.entries.map((entry) => entry.track.title)).toEqual([
      'Disc one, track one',
      'Disc one, track two',
    ])
  })

  test('a single-disc release keeps the ordinary uncluttered track list', async () => {
    const { wrapper } = await record([
      episode(1, { kind: 'song', season: null, title: 'Unnumbered disc' }),
      episode(2, { kind: 'song', season: 1, title: 'Explicit disc one' }),
    ])
    expect(wrapper.findAll('button').some((button) => button.text().includes('Play disc'))).toBe(
      false,
    )
    expect(wrapper.text()).not.toContain('Disc 1')
  })

  test('and its two actions are the queue’s, which are different questions', async () => {
    // Play replaces what is playing; Add does not disturb it.
    const { wrapper } = await record()
    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('Play'))!
      .trigger('click')
    expect(queue.queue.value.entries.map((e) => e.track.title)).toEqual(['Staying Power'])

    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Add to queue')!
      .trigger('click')
    expect(queue.queue.value.entries).toHaveLength(2)
    expect(queue.playing.value?.track.title).toBe('Staying Power')
  })

  test('and Play REPLACES whatever was queued, which Add never does', async () => {
    const { wrapper } = await record()
    queue.appendTrack({ id: 'elsewhere', title: 'Elsewhere' } as never)
    expect(queue.queue.value.entries).toHaveLength(1)

    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('Play'))!
      .trigger('click')
    expect(queue.queue.value.entries.map((e) => e.track.title)).toEqual(['Staying Power'])
  })

  test('neither is offered until there is a track list to act on', async () => {
    // Both of them ARE the track list; a Play that queues nothing is a broken
    // control with no reason to give.
    const { wrapper } = await record([])
    expect(
      wrapper
        .findAll('button')
        .find((b) => b.text().includes('Play'))!
        .attributes('disabled'),
    ).toBeDefined()
  })

  test('and each reason is SAID, because they are different reasons', async () => {
    // Absent data and an empty record are not the same thing, and neither of
    // them is "no". In text as well as in a title: a disabled button is out of
    // the tab order, so its tooltip is unreachable by exactly the people who
    // need the sentence.
    const { wrapper } = await record([])
    expect(wrapper.text()).toContain('This record has no tracks')

    vi.mocked(catalogueChildren).mockRejectedValue(new ApiError(500, 'nope'))
    const failed = await open(Detail, '/library/music/item/album')
    expect(failed.wrapper.text()).toContain('The track list could not be read')
    expect(
      failed.wrapper
        .findAll('button')
        .find((b) => b.text().includes('Play'))!
        .attributes('title'),
    ).toContain('could not be read')
  })

  test('and the track list is a section with a name', async () => {
    const { wrapper } = await record()
    expect(wrapper.findAll('h2').some((h) => h.text() === 'Tracks')).toBe(true)
  })

  test('pressing a track plays the RECORD from there', async () => {
    // The numbered list is the record, and somebody pressing track 4 of nine
    // means "start here" — not "play this one and stop".
    const { wrapper } = await record([
      episode(1, { kind: 'song', title: 'Staying Power' }),
      episode(2, { kind: 'song', title: 'Dancer' }),
    ])
    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('Dancer'))!
      .trigger('click')
    expect(queue.queue.value.entries).toHaveLength(2)
    expect(queue.playing.value?.track.title).toBe('Dancer')
  })

  test('and one track can be added on its own, levelled by itself', async () => {
    // A single track dropped into a queue wants track gain, so it does not
    // arrive at a different loudness from its neighbours.
    const { wrapper } = await record()
    await wrapper.find('[aria-label^="Add Staying Power"]').trigger('click')
    expect(queue.queue.value.entries[0]!.gain).toBe('track')
  })

  test('and nothing on this page is marked while another record is playing', async () => {
    // By id, not by position: the queue may hold something else entirely, and
    // reading its index into THIS record's list marks a track nobody is
    // playing.
    const { wrapper } = await record([
      episode(1, { kind: 'song', title: 'Staying Power' }),
      episode(2, { kind: 'song', title: 'Dancer' }),
    ])
    queue.playAlbum([{ id: 'other', title: 'Another Record' } as never])
    await flushPromises()
    expect(wrapper.findAll('[aria-current="true"]')).toHaveLength(0)
  })

  test('and the track playing is marked, wherever the queue got it', async () => {
    const { wrapper } = await record([
      episode(1, { kind: 'song', title: 'Staying Power' }),
      episode(2, { kind: 'song', title: 'Dancer' }),
    ])
    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('Dancer'))!
      .trigger('click')
    await flushPromises()
    const marked = wrapper.findAll('[aria-current="true"]')
    expect(marked).toHaveLength(1)
    expect(marked[0]!.text()).toContain('Dancer')
  })

  test('and a track list that failed can be asked for again', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(film({ kind: 'album', id: 'album' }) as never)
    vi.mocked(catalogueChildren).mockRejectedValue(new ApiError(500, 'no'))
    const { wrapper } = await open(Detail, '/library/music/item/album')
    expect(wrapper.text()).toContain('Could not load the track list')
    expect(wrapper.findAll('button').some((b) => b.text() === 'Try again')).toBe(true)
  })
})

describe('the season page, in more detail', () => {
  const show = () => film({ kind: 'series', id: 'show', title: 'Fringe', duration_ms: null })

  test('opens on the first thing you have not finished', async () => {
    // The reason you came. Landing on nothing means finding your place twice.
    vi.mocked(catalogueDetail).mockImplementation(async (_library, id) =>
      id === 'show'
        ? (show() as never)
        : (film({ id, kind: 'episode', title: `Open ${id}` }) as never),
    )
    vi.mocked(catalogueChildren).mockResolvedValue({
      children: [episode(1, { played: true }), episode(2), episode(3)],
    } as never)
    const { wrapper } = await open(Season, '/library/shows/item/show/season/1')
    expect(wrapper.text()).toContain('Open e2')
  })

  test('and the panel is not left showing another season’s episode', async () => {
    vi.mocked(catalogueDetail).mockImplementation(async (_library, id) =>
      id === 'show'
        ? (show() as never)
        : (film({ id, kind: 'episode', title: `Open ${id}` }) as never),
    )
    vi.mocked(catalogueChildren).mockImplementation(
      async (_library, _id, params) =>
        ({
          children: params?.season === '2' ? [episode(2, { season: 2 })] : [episode(1)],
        }) as never,
    )
    const { router, wrapper } = await open(Season, '/library/shows/item/show/season/1')
    expect(wrapper.text()).toContain('Open e1')

    await router.push('/library/shows/item/show/season/2')
    await flushPromises()
    expect(wrapper.text()).not.toContain('Open e1')
    expect(wrapper.text()).toContain('Open e2')
  })

  test('the episodes are what the page is: their failure takes the screen', async () => {
    // Which of the two failures the viewer saw used to depend on which
    // request settled last.
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockRejectedValue(new ApiError(500, 'no episodes'))
    const { wrapper } = await open(Season, '/library/shows/item/show/season/1')
    expect(wrapper.text()).toContain('Could not load this season.')
    expect(wrapper.findAll('button').some((b) => b.text() === 'Try again')).toBe(true)
  })

  test('and the show’s own details failing is only a notice', async () => {
    // All it supplies is the title on the back button; the episodes are fine.
    vi.mocked(catalogueDetail).mockRejectedValue(new ApiError(500, 'no title'))
    vi.mocked(catalogueChildren).mockResolvedValue({ children: [episode(1)] } as never)
    const { wrapper } = await open(Season, '/library/shows/item/show/season/1')
    expect(wrapper.text()).not.toContain('Could not load this season.')
    expect(wrapper.text()).toContain('Episode 1')
    expect(notice.value).toContain("show's details")
  })

  test('nothing is asked for an episode nobody has picked', async () => {
    // An empty id is a QUERY for `/items//query`, on every visit and after
    // every mark.
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockResolvedValue({ children: [] } as never)
    await open(Season, '/library/shows/item/show/season/1')
    expect(vi.mocked(catalogueDetail).mock.calls.map((c) => c[0])).not.toContain('')
  })

  test('the episodes are asked for straight away, not behind the show', async () => {
    // The show id is in the URL, and the episodes ARE the page: waiting for
    // the item puts a round trip in front of every still.
    let answerShow = () => {}
    vi.mocked(catalogueDetail).mockReturnValue(
      new Promise((resolve) => (answerShow = () => resolve(show() as never))) as never,
    )
    vi.mocked(catalogueChildren).mockResolvedValue({ children: [episode(1)] } as never)
    const { wrapper } = await open(Season, '/library/shows/item/show/season/1')
    expect(wrapper.text()).toContain('Episode 1')
    answerShow()
    await flushPromises()
  })

  test('marking the season sends this season’s episodes, not the whole series', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockResolvedValue({
      children: [episode(1), episode(2)],
    } as never)
    const { wrapper } = await open(Season, '/library/shows/item/show/season/1')
    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('Mark all watched'))!
      .trigger('click')
    await flushPromises()
    expect(catalogueSetWatched).toHaveBeenCalledWith(expect.any(String), 'show', {
      played: true,
      items: ['e1', 'e2'],
    })
  })

  test('and a season already watched offers to unmark it', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockResolvedValue({
      children: [episode(1, { played: true })],
    } as never)
    const { wrapper } = await open(Season, '/library/shows/item/show/season/1')
    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('Mark none watched'))!
      .trigger('click')
    await flushPromises()
    expect(catalogueSetWatched).toHaveBeenCalledWith(expect.any(String), 'show', {
      played: false,
      items: ['e1'],
    })
  })

  test('a still says which episode it is, using the numbering on screen', async () => {
    // Only browse carries the projection: asking the item for itself gets a
    // null projected season, and the panel printed E10 under a card badged
    // S01E10 — the same episode, numbered two ways, a centimetre apart.
    vi.mocked(catalogueDetail).mockImplementation(async (_library, id) =>
      id === 'show'
        ? (show() as never)
        : (film({ id, kind: 'episode', season: null, episode: 26, title: 'Late' }) as never),
    )
    vi.mocked(catalogueChildren).mockResolvedValue({
      children: [episode(26, { season: null, proj_season: 2, proj_episode: 1 })],
    } as never)
    const { wrapper } = await open(Season, '/library/shows/item/show/season/2')
    expect(wrapper.text()).toContain('S02E01')
    expect(wrapper.text()).not.toContain('E26')
  })

  test('and the picked card says it is the picked one', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(show() as never)
    vi.mocked(catalogueChildren).mockResolvedValue({ children: [episode(1), episode(2)] } as never)
    const { wrapper } = await open(Season, '/library/shows/item/show/season/1')
    const pressed = wrapper.findAll('[aria-pressed="true"]')
    expect(pressed).toHaveLength(1)
  })
})

describe('what an item page does not ask for', () => {
  test('a film has no children, so none are asked for', async () => {
    await open(Detail, '/library/films/item/heat')
    expect(catalogueChildren).not.toHaveBeenCalled()
  })

  test('and an episode does not either', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(film({ kind: 'episode', id: 'e1' }) as never)
    await open(Detail, '/library/shows/item/e1')
    expect(catalogueChildren).not.toHaveBeenCalled()
  })
})

describe('un-ticking', () => {
  test('an episode that has been watched offers to unmark it', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(film({ kind: 'series', id: 'show' }) as never)
    vi.mocked(catalogueChildren).mockResolvedValue({
      children: [episode(1, { played: true })],
    } as never)
    const { wrapper } = await open(Detail, '/library/shows/item/show')
    await wrapper.find('[aria-label^="Mark as unwatched"]').trigger('click')
    await flushPromises()
    expect(catalogueSetWatched).toHaveBeenCalledWith(expect.any(String), 'e1', { played: false })
  })
})

describe('a season still loading', () => {
  test('does not say it is empty', async () => {
    // An empty array meant either "loading" or "this show has no episodes",
    // so the explanation was suppressed for the case it was written for.
    vi.mocked(catalogueDetail).mockResolvedValue(film({ kind: 'series', id: 'show' }) as never)
    vi.mocked(catalogueChildren).mockReturnValue(new Promise(() => {}) as never)
    const { wrapper } = await open(Season, '/library/shows/item/show/season/1')
    expect(wrapper.text()).not.toContain('No episodes in')
  })
})

describe('a still whose episode will not open', () => {
  test('lets go, so pressing it again is not a dead click', async () => {
    // The card took the highlight, nothing opened, and clicking it again was
    // a no-op because the selection had not changed.
    vi.mocked(catalogueDetail).mockImplementation(async (_library, id) =>
      id === 'show'
        ? (film({ kind: 'series', id: 'show' }) as never)
        : Promise.reject(new ApiError(500, 'no')),
    )
    vi.mocked(catalogueChildren).mockResolvedValue({ children: [episode(1)] } as never)
    const { wrapper } = await open(Season, '/library/shows/item/show/season/1')
    await flushPromises()
    expect(wrapper.findAll('[aria-pressed="true"]')).toHaveLength(0)
  })
})

describe('the mark itself', () => {
  test('refuses a second press while one is out, even without a disabled button', async () => {
    // The season page marks a whole season from one control, and a caller
    // that is not a button has nothing to grey out.
    const { useWatched } = await import('../src/composables/item.ts')
    let settle = () => {}
    vi.mocked(catalogueSetWatched).mockReturnValue(
      new Promise((resolve) => (settle = () => resolve({ updated: 1 } as never))) as never,
    )

    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
    let api!: ReturnType<typeof useWatched>
    mount(
      defineComponent({
        setup() {
          api = useWatched(ref('films'))
          return () => h('div')
        },
      }),
      {
        global: {
          plugins: [
            [VueQueryPlugin, { queryClient: client }] as [
              typeof VueQueryPlugin,
              { queryClient: QueryClient },
            ],
          ],
        },
      },
    )

    const first = api.mark('x', true)
    const second = api.mark('x', true)
    expect(catalogueSetWatched).toHaveBeenCalledTimes(1)
    await expect(second).resolves.toBe(false)
    settle()
    await first
  })
})

describe('the last session for this item (OPS-10)', () => {
  const withPlan = () =>
    film({
      negotiated: {
        cost: 'copy',
        mode: 'remux',
        source: null,
        streams: { video: 'copy', audio: 'copy' },
        subtitles: [],
        target_duration_secs: 6,
      },
    })

  beforeEach(() => {
    admin.value = true
    vi.mocked(catalogueDetail).mockResolvedValue(withPlan() as never)
    vi.mocked(adminItemLog).mockResolvedValue('the log' as never)
  })
  afterEach(() => (admin.value = false))

  test('is offered to an administrator, and to nobody else', async () => {
    // The point is debugging a report from somebody else, after they have
    // closed the player — so it is on the item rather than on the session.
    expect((await open(Detail, '/library/films/item/heat')).wrapper.text()).toContain(
      'Last session log',
    )
    admin.value = false
    expect((await open(Detail, '/library/films/item/heat')).wrapper.text()).not.toContain(
      'Last session log',
    )
  })

  test('and downloading it names the item', async () => {
    const click = vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {})
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Last session log')!
      .trigger('click')
    await flushPromises()
    expect(adminItemLog).toHaveBeenCalledWith('heat')
    expect(click).toHaveBeenCalled()
    click.mockRestore()
  })

  test('and one that could not be fetched says so rather than failing silently', async () => {
    vi.mocked(adminItemLog).mockRejectedValue(new ApiError(404, 'no session for that item'))
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Last session log')!
      .trigger('click')
    await flushPromises()
    expect(notice.value).toContain('no session for that item')
  })
})

describe('the subtitles section (HUB-24)', () => {
  const withSubs = (subtitles: Record<string, unknown>[]) =>
    film({
      negotiated: {
        cost: 'copy',
        mode: 'remux',
        source: { source_id: 1 },
        streams: { video: 'copy', audio: 'copy' },
        subtitles,
        target_duration_secs: 6,
      },
    })

  test('says what is in the file, and lists what the hub is storing', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(
      withSubs([
        {
          id: 1,
          origin: 'embedded',
          format: 'srt',
          language: 'eng',
          delivery: 'text',
          note: '',
          deletable: false,
        },
        {
          id: 2,
          origin: 'downloaded',
          format: 'srt',
          language: 'fra',
          delivery: 'text',
          note: '',
          deletable: true,
        },
      ]) as never,
    )
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.text()).toContain('1 in the file: eng')
    expect(wrapper.text()).toContain('downloaded')
  })

  test('and the search is filtered by the media type’s preference', async () => {
    // The wiring, not the panel: the media type comes from the library and the
    // wishlist from the account's preferences, and neither is on the item.
    vi.mocked(getPrefs).mockResolvedValue({
      prefs: [{ scope: '', key: 'subs.movies', value: 'eng, fra' }],
    } as never)
    vi.mocked(catalogueDetail).mockResolvedValue(withSubs([]) as never)
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    await wrapper
      .findAll('button')
      .find((b) => b.text().startsWith('Find subtitles'))!
      .trigger('click')
    await flushPromises()
    expect(catalogueSubtitleSearch).toHaveBeenCalledWith(expect.any(String), 'heat', {
      languages: ['eng', 'fra'],
      source: { media_entry_id: '1', source_version: 'fixture' },
    })
  })

  test('and a standing choice for this title is shown, scoped to the series', async () => {
    vi.mocked(getPrefs).mockResolvedValue({
      prefs: [{ scope: 'show', key: 'subs', value: 'fra' }],
    } as never)
    vi.mocked(catalogueDetail).mockResolvedValue(
      film({ kind: 'episode', parent_id: 'show', negotiated: null }) as never,
    )
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    expect(wrapper.text()).toContain('fra for this title')
  })

  test('and removing a track re-reads the item', async () => {
    vi.mocked(catalogueDetail).mockResolvedValue(
      withSubs([
        {
          id: 2,
          origin: 'downloaded',
          format: 'srt',
          language: 'fra',
          delivery: 'text',
          note: '',
          deletable: true,
        },
      ]) as never,
    )
    const { wrapper } = await open(Detail, '/library/films/item/heat')
    const reads = vi.mocked(catalogueDetail).mock.calls.length
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Remove')!
      .trigger('click')
    await flushPromises()
    expect(catalogueSubtitleDelete).toHaveBeenCalledWith('films', 'heat', 2, {
      media_entry_id: '1',
      source_version: 'fixture',
    })
    expect(vi.mocked(catalogueDetail).mock.calls.length).toBeGreaterThan(reads)
  })
})
