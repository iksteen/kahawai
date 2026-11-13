vi.mock('../src/api/catalogue.ts', () => ({
  listItems: vi.fn(),
  listArtists: vi.fn(),
  artistAlbums: vi.fn(),
  upNext: vi.fn(),
  catalogueDetail: vi.fn(),
  catalogueChildren: vi.fn(),
}))
import { QueryClient, VueQueryPlugin } from '@tanstack/vue-query'
import { flushPromises, mount } from '@vue/test-utils'
import { beforeEach, describe, expect, test, vi } from 'vitest'
import { createMemoryHistory, createRouter, createWebHistory } from 'vue-router'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  libraries: vi.fn(),
  getCatalogueArtworkUrl: (library: string, id: string) =>
    `/api/v1/catalogue/libraries/${library}/items/${id}/artwork`,
}))

const { artistAlbums } = await import('./api-fixture.ts')
const Artist = (await import('../src/views/Artist.vue')).default

const album = (id: string, title: string, year: number) =>
  ({
    id,
    title,
    kind: 'album',
    artist: 'Various Artists',
    year,
    played: false,
    art_version: null,
    resume_position_ms: null,
    resume_duration_ms: null,
    sources: 1,
  }) as never

/// Memory history keeps no `back` in its state, so anything that decides
/// between Back and a push needs the browser's (jsdom's) own.
function routerFor(history = createMemoryHistory()) {
  const Blank = { template: '<div />' }
  return createRouter({
    history,
    routes: [
      { path: '/library/:library', name: 'library', component: Blank },
      { path: '/library/:library/artist/:artist', name: 'artist', component: Artist },
      {
        path: '/library/:library/artist/:artist/item/:id',
        name: 'artist-album',
        component: Blank,
      },
    ],
  })
}

async function page() {
  const router = routerFor()
  await router.push('/library/music/artist/various%20artists')
  await router.isReady()
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  const wrapper = mount(Artist, {
    global: { plugins: [router, [VueQueryPlugin, { queryClient }]] },
  })
  await flushPromises()
  return { router, wrapper }
}

beforeEach(() => {
  vi.mocked(artistAlbums).mockResolvedValue({
    artist: {
      key: 'various artists',
      name: 'Various Artists',
      album_count: 2,
    },
    albums: [album('old', 'Old Record', 1971), album('new', 'New Record', 2024)],
    total: 2,
    limit: 100,
    offset: 0,
  })
})

describe('an Album Artist', () => {
  test('shows that artist’s albums chronologically and opens one in context', async () => {
    const { router, wrapper } = await page()
    expect(wrapper.find('h1').text()).toBe('Various Artists')
    expect(wrapper.findAll('.card-title').map((node) => node.text())).toEqual([
      'Old Record',
      'New Record',
    ])
    expect(artistAlbums).toHaveBeenCalledWith(
      'various artists',
      expect.objectContaining({ library: 'music', sort: 'year', offset: 0 }),
    )

    await wrapper.findAll('button.card')[0]!.trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.name).toBe('artist-album')
    expect(router.currentRoute.value.params.id).toBe('old')
    expect(router.currentRoute.value.params.artist).toBe('various artists')
  })

  test('can reverse chronology without losing the artist scope', async () => {
    const { wrapper } = await page()
    vi.mocked(artistAlbums).mockClear()
    await wrapper.find('select').setValue('-year')
    await flushPromises()
    expect(artistAlbums).toHaveBeenCalledWith(
      'various artists',
      expect.objectContaining({ library: 'music', sort: '-year', offset: 0 }),
    )
  })

  test('does not turn API paging into a load-more interaction', async () => {
    vi.mocked(artistAlbums).mockResolvedValueOnce({
      artist: {
        key: 'various artists',
        name: 'Various Artists',
        album_count: 101,
      },
      albums: [album('old', 'Old Record', 1971), album('new', 'New Record', 2024)],
      total: 101,
      limit: 100,
      offset: 0,
    })
    const { wrapper } = await page()
    expect(wrapper.text()).not.toContain('More albums')
  })

  test('Up returns to the library it came from, still filtered', async () => {
    // The library left filtered is `/library/music?q=bjo` in history; Up
    // pushing a bare `/library/music` started it over, unfiltered, at the top.
    const router = routerFor(createWebHistory())
    await router.push('/library/music?q=bjo')
    await router.push('/library/music/artist/various%20artists')
    await router.isReady()
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
    const wrapper = mount(Artist, {
      global: { plugins: [router, [VueQueryPlugin, { queryClient }]] },
    })
    await flushPromises()
    await wrapper.findAll('button')[0]!.trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.fullPath).toBe('/library/music?q=bjo')
    // Back, not a new entry on top of it.
    expect(router.options.history.state.forward).toBe('/library/music/artist/various%20artists')
  })

  test('returns to the containing music library', async () => {
    const { router, wrapper } = await page()
    await wrapper.findAll('button')[0]!.trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.name).toBe('library')
    expect(router.currentRoute.value.params.library).toBe('music')
  })
})
